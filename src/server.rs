mod inbox;
mod nodeinfo;

use crate::activity::{ActorOrProxied, Note};
use crate::db::Db;
use crate::error::Error;
use crate::event_deletion_queue::EventDeletionQueue;
use crate::nostr::{get_nostr_user_data, NostrUser};
use crate::nostr_to_ap::{replace_npub_with_ap_handle, Content};
use crate::rsa_keys::RSA_PUBLIC_KEY_STRING;
use crate::server::inbox::http_post_inbox;
pub use crate::server::inbox::{event_tag, InternalApId};
use crate::server::nodeinfo::well_known_nodeinfo;
use crate::util::Merge;
use crate::{
    RelayId, BIND_ADDRESS, DOMAIN, HTTPS_DOMAIN, OUTBOX_RELAYS, RELAYS, USER_AGENT, USER_ID_PREFIX,
};
use axum::extract::{Path, Query, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_macros::debug_handler;
use cached::TimedSizedCache;
use itertools::Itertools;
use linkify::{LinkFinder, LinkKind};
use lru::LruCache;
use mediatype::ReadParams;
use nodeinfo::nodeinfo;
use nostr_lib::nips::nip19::Nip19Profile;
use nostr_lib::{EventId, FromBech32, Metadata, PublicKey, ToBech32};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use relay_pool::{EventWithRelayId, RelayPool};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

type LazyNote = Arc<tokio::sync::OnceCell<Option<EventWithRelayId<RelayId>>>>;
type LazyUser = Arc<tokio::sync::OnceCell<Arc<Result<NostrUser, Error>>>>;

#[derive(Debug)]
pub struct AppState {
    pub nostr: RelayPool<RelayId>,
    pub nostr_account_to_followers: Mutex<FxHashMap<nostr_lib::PublicKey, Arc<HashSet<String>>>>,
    pub nostr_account_to_followers_rev: Mutex<FxHashMap<String, FxHashSet<nostr_lib::PublicKey>>>,
    pub activitypub_accounts: Mutex<FxHashMap<nostr_lib::PublicKey, Arc<String>>>,
    pub http_client: reqwest::Client,
    pub note_cache: Mutex<LruCache<EventId, LazyNote>>,
    pub actor_cache: Mutex<LruCache<String, ActorOrProxied>>,
    pub nostr_user_cache: Mutex<TimedSizedCache<nostr_lib::PublicKey, LazyUser>>,
    pub relay_url: Vec<url::Url>,
    pub main_relays: Arc<FxHashSet<RelayId>>,
    pub metadata_relays: Arc<FxHashSet<RelayId>>,
    pub event_deletion_queue: EventDeletionQueue,
    pub db: Db,
}

pub async fn listen(state: Arc<AppState>) -> Result<(), Error> {
    info!("Listening on {BIND_ADDRESS}");
    let app = Router::new()
        .route("/", get(root))
        .route("/nodeinfo/2.1", get(nodeinfo))
        .route("/inbox", post(http_post_inbox))
        .route("/users/:user", get(http_get_user))
        .route("/notes/:note", get(http_get_note))
        .route("/.well-known/webfinger", get(webfinger))
        .route("/.well-known/nostr.json", get(nostr_json))
        .route("/.well-known/nodeinfo", get(well_known_nodeinfo))
        .fallback(handler_404)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(BIND_ADDRESS).await.unwrap();
    Ok(axum::serve(listener, app).await?)
}

#[debug_handler]
#[tracing::instrument]
pub async fn root() -> Result<&'static str, Error> {
    info!("root");
    Ok("# Momostr

A WIP bridge between Nostr and Fediverse.


## How to follow Fediverse account from Nostr?

Follow `username_at_host@momostr.pink` if the Fediverse account you want to follow is `@username@host`. \
You can get their npub from `https://njump.me/username_at_host@momostr.pink`.

Make sure to add wss://relay.momostr.pink to your relays so that you can read from and write to Fediverse.


## How to follow Nostr account from Fediverse?

Follow `@npub1...@momostr.pink` if the Nostr account you want to follow is `npub1...`.


## Why aren't my notes on Nostr showing up on a Fediverse server.

You need to be followed by someone on that server for your notes to show up on there.


## Limitations

Non-public posts like DMs or followers-only posts are not supported.


## Contact to the admin

mailto:momostr.pink@gmail.com
")
}

#[derive(Deserialize)]
pub struct WebfingerQuery {
    resource: String,
}

#[debug_handler]
#[tracing::instrument(skip_all)]
pub async fn webfinger(
    Query(WebfingerQuery { resource }): Query<WebfingerQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Error> {
    pub static R: Lazy<Regex> =
        Lazy::new(|| Regex::new(&format!(r"^(?:acct:)?([^@]+)@{DOMAIN}$")).unwrap());

    debug!("webfinger?resource={resource}");
    let npub = R
        .captures(&resource)
        .ok_or(Error::NotFound)?
        .get(1)
        .unwrap()
        .as_str();
    let pub_key = nostr_lib::PublicKey::from_bech32(npub).map_err(|_| Error::NotFound)?;
    let a = &*get_nostr_user_data(&state, pub_key).await;
    if let NostrUser::Proxied(url) = a.as_ref().map_err(|e| e.clone())? {
        Ok(Json(json!({
            "subject": format_args!("acct:{npub}@{DOMAIN}"),
            "links": [
                {
                    "rel": "self",
                    "type": "application/activity+json",
                    "href": url,
                },
            ]
        })))
    } else {
        Ok(Json(json!({
            "subject": format_args!("acct:{npub}@{DOMAIN}"),
            "links": [
                {
                    "rel": "self",
                    "type": "application/activity+json",
                    "href": format_args!("{USER_ID_PREFIX}{npub}"),
                },
                {
                    "rel": "http://webfinger.net/rel/profile-page",
                    "type": "text/html",
                    "href": format_args!("https://njump.me/{npub}"),
                },
            ]
        })))
    }
}

#[derive(Deserialize)]
pub struct NostrJsonQuery {
    name: String,
}

#[debug_handler]
#[tracing::instrument(skip_all)]
pub async fn nostr_json(
    Query(NostrJsonQuery { name }): Query<NostrJsonQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, Error> {
    debug!("nostr.json?name={name}");
    let (name_decoded, host) = name.rsplit_once("_at_").ok_or_else(|| Error::NotFound)?;
    let host = host.replace(".at_", "at_");
    #[derive(Deserialize, Debug)]
    struct WebfingerResponse {
        links: Vec<WebfingerLink>,
    }
    #[derive(Deserialize, Debug)]
    struct WebfingerLink {
        r#type: Option<mediatype::MediaTypeBuf>,
        href: Option<String>,
    }
    let WebfingerResponse { links } = state
        .http_client
        .get(format!(
            "https://{host}/.well-known/webfinger?resource=acct:{name_decoded}@{host}"
        ))
        .header(reqwest::header::USER_AGENT, &*USER_AGENT)
        .send()
        .await
        .map_err(|e| Error::NotFoundWithMsg(e.to_string()))?
        .json()
        .await
        .map_err(|e| Error::NotFoundWithMsg(e.to_string()))?;
    let param_profile = mediatype::Name::new("profile").unwrap();
    let value_activitystreams =
        mediatype::Value::new("\"https://www.w3.org/ns/activitystreams\"").unwrap();
    let id = links
        .into_iter()
        .find(|l| {
            if let Some(t) = &l.r#type {
                t.ty() == mediatype::names::APPLICATION
                    && t.suffix() == Some(mediatype::names::JSON)
                    && (t.subty() == mediatype::names::ACTIVITY
                        || t.subty() == mediatype::names::LD
                            && t.get_param(param_profile) == Some(value_activitystreams))
            } else {
                false
            }
        })
        .ok_or(Error::NotFound)?
        .href
        .ok_or(Error::NotFound)?;
    let (ActorOrProxied::Actor(actor), new) = state.get_actor_data_and_if_its_new(&id).await?
    else {
        return Err(Error::NotFound);
    };
    if new {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut r = Json(json!({
        "names": {
            name: actor.npub,
        },
        "relays": {
            actor.npub.to_string(): &*OUTBOX_RELAYS,
        }
    }))
    .into_response();
    r.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::header::HeaderValue::from_static("*"),
    );
    Ok(r)
}

const ACTIVITY_STREAMS_URL: &str = "https://www.w3.org/ns/activitystreams";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
struct Image<'a> {
    url: &'a str,
}

impl<'a> Image<'a> {
    fn new(url: &'a str) -> Self {
        Self { url }
    }
}

pub struct WithContext<T>(pub T);

impl<T: Serialize> Serialize for WithContext<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        struct Context;

        impl Serialize for Context {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let mut m = serializer.serialize_map(Some(1))?;
                m.serialize_entry(
                    "@context",
                    &[
                        "https://www.w3.org/ns/activitystreams",
                        "https://w3id.org/security/v1",
                    ],
                )?;
                m.end()
            }
        }

        Merge {
            f1: &self.0,
            f2: Context,
        }
        .serialize(serializer)
    }
}

#[derive(Debug)]
pub struct MetadataActivity<'a> {
    metadata: &'a Metadata,
    npub: PublicKey,
    sumarry: Option<String>,
}

impl Serialize for MetadataActivity<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let npub = self.npub.to_bech32().unwrap();
        let nprofile = Nip19Profile {
            public_key: self.npub,
            relays: RELAYS
                .iter()
                .filter_map(|s| nostr_lib::Url::from_str(s).ok())
                .collect(),
        }
        .to_bech32()
        .unwrap();
        let id = format!("{USER_ID_PREFIX}{npub}");
        let inbox = format!("{HTTPS_DOMAIN}/inbox");
        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry(
            "@context",
            &[ACTIVITY_STREAMS_URL, "https://w3id.org/security/v1"],
        )?;
        m.serialize_entry("type", "Person")?;
        m.serialize_entry("id", &id)?;
        m.serialize_entry("preferredUsername", &npub)?;
        match &self.metadata.display_name {
            Some(name) if !name.is_empty() => {
                m.serialize_entry("name", name)?;
            }
            _ => {
                if let Some(name) = &self.metadata.name {
                    m.serialize_entry("name", name)?;
                }
            }
        }
        m.serialize_entry("inbox", &inbox)?;

        // needed to work with threads.net
        // m.serialize_entry("outbox", &format_args!("{id}/outbox"))?;
        // m.serialize_entry("followers", &format_args!("{id}/followers"))?;
        // m.serialize_entry("following", &format_args!("{id}/following"))?;

        m.serialize_entry("endpoints", &json!({ "sharedInbox": inbox }))?;
        m.serialize_entry(
            "url",
            &format_args!("https://coracle.social/people/{nprofile}"),
        )?;

        // this format is not compatible with threads.net
        // m.serialize_entry(
        //     "url",
        //     &[
        //         LinkForSer {
        //             rel: None,
        //             href: &format_args!("https://coracle.social/people/{nprofile}"),
        //         },
        //         LinkForSer {
        //             rel: Some("canonical"),
        //             href: &format_args!("nostr:{npub}"),
        //         },
        //     ],
        // )?;

        m.serialize_entry(
            "proxyOf",
            &[&json!({
                "protocol": "https://github.com/nostr-protocol/nostr",
                "proxied": npub,
                "authoritative": true,
            })],
        )?;
        if let Some(summary) = &self.sumarry {
            m.serialize_entry("summary", summary)?;
        }
        if let Some(icon) = &self.metadata.picture {
            m.serialize_entry("icon", &Image::new(icon))?;
        }
        if let Some(image) = &self.metadata.banner {
            m.serialize_entry("image", &Image::new(image))?;
        }
        m.serialize_entry("discoverable", &true)?;
        m.serialize_entry("indexable", &true)?;
        m.serialize_entry(
            "publicKey",
            &json!({
                "id": id,
                "type": "Key",
                "owner": id,
                "publicKeyPem": *RSA_PUBLIC_KEY_STRING,
            }),
        )?;
        match &self.metadata.website {
            Some(website) if !website.is_empty() => {
                m.serialize_entry(
                    "attachment",
                    &[json!({
                        "type": "PropertyValue",
                        "name": "Website",
                        "value": website
                    })],
                )?;
            }
            _ => (),
        }
        m.end()
    }
}

impl IntoResponse for MetadataActivity<'_> {
    fn into_response(self) -> Response {
        Response::builder()
            .header(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/activity+json"),
            )
            .body(serde_json::to_string(&self).unwrap())
            .unwrap()
            .into_response()
    }
}

pub async fn metadata_to_activity<'a>(
    state: &Arc<AppState>,
    npub: PublicKey,
    metadata: &'a Metadata,
) -> MetadataActivity<'a> {
    let sumarry = if let Some(a) = &metadata.about {
        let mut sumarry = Content {
            html: String::with_capacity(a.len()),
            misskey: String::with_capacity(a.len()),
        };
        let spans = {
            let mut link_finder = LinkFinder::new();
            link_finder.kinds(&[LinkKind::Url]);
            link_finder.spans(a).collect_vec()
        };
        for span in spans {
            if span.kind().is_some() {
                sumarry.link(span.as_str());
            } else {
                replace_npub_with_ap_handle(
                    &mut sumarry,
                    span.as_str(),
                    state,
                    &mut FxHashMap::default(),
                )
                .await
                .unwrap();
            }
        }
        Some(sumarry.html)
    } else {
        None
    };
    MetadataActivity {
        metadata,
        npub,
        sumarry,
    }
}

#[debug_handler]
#[tracing::instrument(skip(state))]
pub async fn http_get_user(
    Path(npub): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<axum::http::Response<axum::body::Body>, Error> {
    debug!("get user");
    let public_key = nostr_lib::PublicKey::from_bech32(&npub).map_err(|_| Error::NotFound)?;
    let a = &*get_nostr_user_data(&state, public_key).await;
    match a.as_ref().map_err(|e| e.clone())? {
        NostrUser::Proxied(_) => Err(Error::NotFound),
        NostrUser::Metadata(metadata) => Ok(metadata_to_activity(&state, public_key, metadata)
            .await
            .into_response()),
    }
}

struct JsonActivity(String);

impl IntoResponse for JsonActivity {
    fn into_response(self) -> Response {
        Response::builder()
            .header(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/activity+json"),
            )
            .body(self.0)
            .unwrap()
            .into_response()
    }
}

#[debug_handler]
#[tracing::instrument(skip(state))]
pub async fn http_get_note(
    Path(note): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<JsonActivity, Error> {
    info!("");
    let note_id = EventId::from_bech32(&note).map_err(|_| Error::NotFound)?;
    let note = state.get_note(note_id).await.ok_or(Error::NotFound)?;
    let note = Note::from_nostr_event(&state, &note.event)
        .await
        .ok_or(Error::NotFound)?;
    let s = serde_json::to_string(&WithContext(&note)).unwrap();
    Ok(JsonActivity(s))
}

#[debug_handler]
#[tracing::instrument(skip(request))]
async fn handler_404(request: Request) -> Error {
    info!("handler_404: {}", request.uri());
    Error::NotFound
}
