use crate::error::Error;
use crate::rsa_keys::{RSA_PRIVATE_KEY, RSA_PRIVATE_KEY_FOR_SIGH};
use crate::server::{event_tag, AppState, WithContext};
use crate::{
    html_to_text, HTTPS_DOMAIN, INBOX_RELAYS, KEY_ID, NOTE_ID_PREFIX, OUTBOX_RELAYS, SECRET_KEY,
    USER_AGENT, USER_ID_PREFIX,
};
use axum::http::{Method, Request};
use base64::Engine;
use chrono::{DateTime, Utc};
use json_sign::{get_sign, RsaSignature};
use nostr_lib::nips::nip65::RelayMetadata;
use nostr_lib::{EventBuilder, JsonUtil, Metadata};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::digest::FixedOutput;
use sha2::Digest;
use sha3::Sha3_256;
use sigh::alg::RsaSha256;
use sigh::{Key, SigningConfig};
use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};
use url::Url;

pub const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename = "Document", rename_all = "camelCase")]
pub struct Attachment {
    pub media_type: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct Note {
    pub author: String,
    pub id: String,
    pub nevent: String,
    pub content: String,
    pub misskey_content: String,
    pub published: String,
    pub attachment: Vec<Attachment>,
    pub quote: Option<String>,
    pub in_reply_to: Option<String>,
    pub tag: Vec<NoteTagForSer>,
    pub summary: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "PascalCase")]
pub enum NoteTagForSer {
    Hashtag {
        name: String,
        href: String,
    },
    Emoji {
        name: String,
        icon: ImageForSe,
        id: String,
    },
    Mention {
        href: String,
        name: String,
    },
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename = "Image")]
pub struct ImageForSe {
    pub url: String,
}

// https://codeberg.org/fediverse/fep/src/commit/73bd09423da32646b8d44a98e5348bf470f88c16/fep/fffd/fep-fffd.md
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename = "Link")]
pub struct LinkForSer<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rel: Option<&'static str>,
    pub href: T,
}

impl Serialize for Note {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("type", "Note")?;
        m.serialize_entry("id", &format_args!("{NOTE_ID_PREFIX}{}", self.id))?;
        m.serialize_entry(
            "url",
            &format_args!("https://coracle.social/{}", self.nevent),
        )?;
        m.serialize_entry(
            "proxyOf",
            &[&json!({
                "protocol": "https://github.com/nostr-protocol/nostr",
                "proxied": self.id,
                "authoritative": true,
            })],
        )?;
        // Pleroma does not accept notes with FEP-fffd style url like this:
        // ```
        // m.serialize_entry(
        //     "url",
        //     &[
        //         LinkForSer {
        //             rel: None,
        //             href: &format_args!("https://coracle.social/{}", self.nevent),
        //         },
        //         LinkForSer {
        //             rel: Some("canonical"),
        //             href: &format_args!("nostr:{}", self.id),
        //         },
        //     ],
        // )?;
        // ```
        m.serialize_entry("attributedTo", &self.author)?;
        // Pleroma does not recognize `Public` or `as:Public`
        m.serialize_entry("to", &[AS_PUBLIC])?;
        m.serialize_entry("content", &self.content)?;
        m.serialize_entry("_misskey_content", &self.misskey_content)?;
        m.serialize_entry("published", &self.published)?;
        if !self.attachment.is_empty() {
            m.serialize_entry("attachment", &self.attachment)?;
        }
        if let Some(in_reply_to) = &self.in_reply_to {
            m.serialize_entry("inReplyTo", in_reply_to)?;
        }
        if let Some(quote) = &self.quote {
            m.serialize_entry("quoteUrl", quote)?;
            m.serialize_entry("_misskey_quote", quote)?;
        }
        if !self.tag.is_empty() {
            m.serialize_entry("tag", &self.tag)?;
        }
        if let Some(summary) = &self.summary {
            m.serialize_entry("summary", summary)?;
        }
        m.end()
    }
}

#[derive(Clone, Debug)]
pub struct CreateForSer<'a> {
    pub actor: &'a str,
    pub id: &'a str,
    pub object: &'a Note,
    pub published: &'a str,
}

impl Serialize for CreateForSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("type", "Create")?;
        m.serialize_entry("id", &format_args!("{HTTPS_DOMAIN}/create/{}", self.id))?;
        m.serialize_entry("to", &[AS_PUBLIC])?;
        m.serialize_entry("actor", &self.actor)?;
        m.serialize_entry("object", &self.object)?;
        m.serialize_entry("published", &self.published)?;
        m.end()
    }
}

#[derive(Clone, Debug)]
pub struct DeleteForSer<'a> {
    pub actor: &'a str,
    pub id: &'a str,
    pub object: &'a str,
}

impl Serialize for DeleteForSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("type", "Delete")?;
        m.serialize_entry("id", &format_args!("{HTTPS_DOMAIN}/delete/{}", self.id))?;
        m.serialize_entry("to", &[AS_PUBLIC])?;
        m.serialize_entry("actor", &self.actor)?;
        m.serialize_entry(
            "object",
            &json!({"id":format_args!("{NOTE_ID_PREFIX}{}", self.object),"type":"Tombstone"}),
        )?;
        m.end()
    }
}

#[derive(Clone, Debug)]
pub struct ReactionForSer<'a> {
    pub content: Option<&'a str>,
    pub id: &'a str,
    pub object: &'a str,
    pub actor: &'a str,
    pub tag: Option<NoteTagForSer>,
}

impl Serialize for ReactionForSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("type", "Like")?;
        m.serialize_entry("id", &format_args!("{HTTPS_DOMAIN}/reaction/{}", self.id))?;
        m.serialize_entry("actor", &self.actor)?;
        m.serialize_entry("object", &self.object)?;
        if let Some(t) = &self.content {
            m.serialize_entry("content", t)?;

            // needed to work with Pleroma (https://git.pleroma.social/pleroma/pleroma/-/blob/9953b0da59924f936ecc646b22cd3e3a58493d6a/lib/pleroma/web/activity_pub/transmogrifier.ex#L453)
            m.serialize_entry("_misskey_reaction", t)?;
        }
        if let Some(t) = &self.tag {
            m.serialize_entry("tag", &[t])?;
        }
        m.end()
    }
}

#[derive(Clone, Debug)]
pub struct AnnounceForSer<'a> {
    pub id: &'a str,
    pub object: &'a str,
    pub actor: &'a str,
    pub published: &'a str,
}

impl Serialize for AnnounceForSer<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("type", "Announce")?;
        m.serialize_entry("id", &format_args!("{HTTPS_DOMAIN}/announce/{}", self.id))?;
        m.serialize_entry("actor", &self.actor)?;
        m.serialize_entry("object", &self.object)?;
        m.serialize_entry("to", &[AS_PUBLIC])?;
        m.serialize_entry("published", &self.published)?;
        m.end()
    }
}

#[derive(Clone, Debug)]
pub struct UpdateForSer<'a, M: Serialize> {
    pub id: &'a str,
    pub object: M,
    pub actor: &'a str,
    pub published: &'a str,
}

impl<M: Serialize> Serialize for UpdateForSer<'_, M> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("type", "Update")?;
        m.serialize_entry("id", &format_args!("{HTTPS_DOMAIN}/update/{}", self.id))?;
        m.serialize_entry("actor", &self.actor)?;
        m.serialize_entry("object", &self.object)?;
        m.serialize_entry("to", &[AS_PUBLIC])?;
        m.serialize_entry("published", &self.published)?;
        m.end()
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub struct ActivityForDe<'a> {
    #[serde(borrow)]
    pub actor: Cow<'a, str>,
    #[serde(flatten)]
    pub activity_inner: ActivityForDeInner<'a>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NoteForDe {
    pub id: String,
    pub content: String,
    pub source: Option<Source>,
    pub published: DateTime<Utc>,
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub tag: Vec<NoteTagForDe>,
    #[serde(default)]
    pub attachment: Vec<AttachedImage>,
    #[serde(default)]
    pub url: ActorUrl,
    pub proxy_of: Option<ProxyOf>,
    pub attributed_to: String,
    pub quote_url: Option<String>,
    // threads.net only provides `_misskey_quote`
    #[serde(rename = "_misskey_quote")]
    pub misskey_quote: Option<String>,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    pub sensitive: Option<bool>,
    pub summary: Option<String>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub content: String,
    pub media_type: String,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type")]
pub enum NoteTagForDe {
    Mention {
        href: String,
        name: String,
    },
    Emoji {
        name: String,
        icon: ImageForDe,
    },
    Hashtag {
        name: String,
    },
    #[serde(untagged)]
    Other(IgnoredAny),
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct ImageForDe {
    pub url: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ActivityForDeInner<'a> {
    Follow {
        object: Cow<'a, str>,
        id: Option<Cow<'a, str>>,
    },
    Undo {
        #[serde(borrow)]
        object: Box<ActivityForDe<'a>>,
    },
    #[serde(alias = "EmojiReact")]
    Like {
        object: Cow<'a, str>,
        content: Option<Cow<'a, str>>,
        id: Cow<'a, str>,
        #[serde(default)]
        tag: Vec<NoteTagForDe>,
    },
    Announce {
        id: Cow<'a, str>,
        object: StrOrId<'a>,
        published: DateTime<Utc>,
        #[serde(default)]
        to: Vec<Cow<'a, str>>,
        #[serde(default)]
        cc: Vec<Cow<'a, str>>,
    },
    Update {
        object: ActorOrProxied,
    },
    Create {
        object: Box<NoteForDe>,
    },
    Delete {
        object: StrOrId<'a>,
    },
    #[serde(untagged)]
    Other(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub struct StrOrId<'a>(pub Cow<'a, str>);

impl<'a, 'b> Deserialize<'a> for StrOrId<'b> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'a>,
    {
        #[derive(Deserialize, Clone, Debug, PartialEq)]
        #[serde(untagged)]
        pub enum StrOrIdInner<'a> {
            Str(Cow<'a, str>),
            Id { id: Cow<'a, str> },
        }
        let (StrOrIdInner::Str(id) | StrOrIdInner::Id { id }) =
            StrOrIdInner::deserialize(deserializer)?;
        Ok(Self(id))
    }
}

impl<'a> AsRef<ActivityForDe<'a>> for ActivityForDe<'a> {
    fn as_ref(&self) -> &ActivityForDe<'a> {
        self
    }
}

#[derive(Clone, Debug)]
pub struct AcceptActivity<'a> {
    pub object: FollowActivity<'a>,
    pub actor: &'a str,
}

impl<'a> Serialize for AcceptActivity<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("type", "Accept")?;
        if let Some(id) = self.object.id {
            m.serialize_entry("id", &format_args!("{HTTPS_DOMAIN}/accept/{}", id))?;
        }
        m.serialize_entry("actor", &self.actor)?;
        m.serialize_entry("object", &self.object)?;
        m.end()
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename = "Follow")]
pub struct FollowActivity<'a> {
    pub actor: &'a str,
    pub object: &'a str,
    pub id: Option<&'a str>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename = "Undo")]
pub struct UndoFollowActivity<'a> {
    pub object: FollowActivity<'a>,
    pub actor: &'a str,
    pub id: &'a str,
}

#[derive(Serialize)]
pub struct WithSign<'a, A> {
    #[serde(flatten)]
    pub activity: A,
    pub signature: RsaSignature<'a>,
}

pub async fn activity_to_string<A: Serialize, S: AsRef<str>>(
    activity: A,
    author: S,
    sign: bool,
) -> String {
    let activity = WithContext(activity);
    let body = serde_json::to_string(&activity).unwrap();
    if !sign {
        return body;
    }
    if let Some(signature) = get_sign(&body, &RSA_PRIVATE_KEY, author.as_ref()).await {
        serde_json::to_string(&WithSign {
            activity,
            signature,
        })
        .unwrap()
    } else {
        tracing::error!("could not sing");
        body
    }
}

impl AppState {
    #[tracing::instrument(skip_all)]
    pub async fn send_activity<S: AsRef<str>, A: Serialize>(
        &self,
        inbox: &Url,
        author: S,
        activity: A,
        sign: bool,
    ) -> Result<(), Error> {
        let body = activity_to_string(activity, author.as_ref(), sign).await;
        self.send_string_activity(inbox, author, body).await
    }

    pub async fn send_string_activity<S: AsRef<str>>(
        &self,
        inbox: &Url,
        author: S,
        body: String,
    ) -> Result<(), Error> {
        let host = inbox.host().unwrap();
        info!("send_activity: {inbox} <== {body}");
        let digest = sha2::Sha256::digest(&body);
        let digest = base64::prelude::BASE64_STANDARD.encode(digest);
        let mut r = Request::builder()
            .method(Method::POST)
            .uri(inbox.as_str())
            .header(
                axum::http::header::CONTENT_TYPE,
                "application/activity+json",
            )
            .header(axum::http::header::USER_AGENT, &*USER_AGENT)
            .header("host", host.to_string())
            .header(
                "date",
                httpdate::HttpDate::from(std::time::SystemTime::now()).to_string(),
            )
            .header("digest", format!("SHA-256={digest}"))
            .body(body)
            .unwrap();
        SigningConfig::new(RsaSha256, &RSA_PRIVATE_KEY_FOR_SIGH, author.as_ref())
            .sign(&mut r)
            .unwrap();
        let r = reqwest::Request::try_from(r)?;
        let r = self.http_client.execute(r).await?;
        info!(
            "send_activity response: {inbox} ==> status: {}, body: {:?}",
            r.status(),
            r.text().await
        );
        Ok(())
    }

    pub async fn get_activity_json<T: DeserializeOwned>(&self, url: &Url) -> Result<T, Error> {
        let digest = sha2::Sha256::digest([]);
        let digest = base64::prelude::BASE64_STANDARD.encode(digest);
        let mut r = Request::builder()
            .method(Method::GET)
            .uri(url.as_str())
            .header(axum::http::header::ACCEPT, "application/activity+json")
            .header(axum::http::header::USER_AGENT, &*USER_AGENT)
            .header("host", url.host().unwrap().to_string())
            .header(
                "date",
                httpdate::HttpDate::from(std::time::SystemTime::now()).to_string(),
            )
            .header("digest", format!("SHA-256={digest}"))
            // Content-Type doesn't have to be text/plain but should not be empty to work with Mastodon
            .header(axum::http::header::CONTENT_TYPE, "text/plain")
            .body(())
            .unwrap();
        SigningConfig::new(RsaSha256, &RSA_PRIVATE_KEY_FOR_SIGH, &*KEY_ID)
            .sign(&mut r)
            .unwrap();
        let t = self
            .http_client
            .get(&url.to_string())
            .headers(r.headers().clone())
            .send()
            .await?
            .text()
            .await?;
        debug!("{url} ==> {t}");
        Ok(serde_json::from_str(&t)?)
    }

    pub async fn get_activity_json_with_retry<T: DeserializeOwned>(
        &self,
        url: &Url,
    ) -> Result<T, Error> {
        match self.get_activity_json(url).await {
            Ok(actor) => Ok(actor),
            Err(e) => {
                warn!("could not get activity from {url}: {e:?}");
                tokio::time::sleep(Duration::from_secs(30)).await;
                debug!("retrying ...");
                match self.get_activity_json(url).await {
                    Ok(actor) => {
                        debug!("retry succeeded");
                        Ok(actor)
                    }
                    Err(e) => {
                        debug!("retry failed");
                        Err(e)
                    }
                }
            }
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_actor_data(&self, id: &str) -> Result<ActorOrProxied, Error> {
        self.get_actor_data_and_if_its_new(id, None)
            .await
            .map(|(a, _)| a)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_actor_data_and_if_its_new(
        &self,
        id: &str,
        webfinger: Option<&str>,
    ) -> Result<(ActorOrProxied, bool), Error> {
        {
            if let Some(actor) = self.actor_cache.lock().get(id) {
                return Ok((actor.clone(), false));
            }
        }
        if let Some(npub) = id.strip_prefix(USER_ID_PREFIX) {
            let actor = ActorOrProxied::Proxied(Arc::new(npub.to_string()));
            self.actor_cache.lock().push(id.to_string(), actor.clone());
            return Ok((actor, false));
        }
        let actor: ActorOrProxied = self
            .get_activity_json_with_retry(&id.parse::<Url>()?)
            .await
            .map_err(|e| {
                Error::BadRequest(Some(format!("could not get user data from {id}: {e:?}")))
            })?;
        let new = self.update_actor_metadata(&actor, webfinger).await?;
        self.actor_cache.lock().push(id.to_string(), actor.clone());
        Ok((actor, new))
    }

    pub async fn update_actor_metadata(
        &self,
        actor: &ActorOrProxied,
        webfinger: Option<&str>,
    ) -> Result<bool, Error> {
        static R: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[[:word:].-]+$").unwrap());
        let ActorOrProxied::Actor(actor) = &actor else {
            return Ok(false);
        };
        let nip05 = match (Url::parse(&actor.id)?.domain(), &actor.preferred_username) {
            (Some(domain), Some(name)) if R.is_match(name) => {
                let n = format!(
                    "{}_at_{}@momostr.pink",
                    name.to_lowercase(),
                    domain.replace("at_", ".at_")
                );
                if webfinger == Some(name) {
                    Some(n)
                } else {
                    match self.get_ap_id_from_webfinger(name, domain).await {
                        Ok(id) => {
                            if id == actor.id {
                                Some(n)
                            } else {
                                warn!("preferred username is invalid: {}", actor.id);
                                None
                            }
                        }
                        Err(e) => {
                            warn!("could not get ap id from webfinger: {e:?}");
                            None
                        }
                    }
                }
            }
            _ => None,
        };
        let key = nostr_lib::Keys::new(actor.nsec.clone());
        let mut about = actor.summary.clone();
        let mut first_property = true;
        let mut lud16 = None;
        use std::fmt::Write;
        for a in &actor.property_values {
            if (a.name == "⚡" || a.name == "⚡\u{fe0f}") && a.value.contains('@') {
                lud16 = Some(a.value.clone());
            } else {
                if first_property {
                    first_property = false;
                    if about.is_none() {
                        about = Some(String::new());
                    }
                    writeln!(&mut about.as_mut().unwrap(), "\n")?;
                }
                writeln!(&mut about.as_mut().unwrap(), "{}: {}", a.name, a.value)?;
            }
        }
        let metadata = EventBuilder::new(
            nostr_lib::Kind::Metadata,
            Metadata {
                name: Some(actor.name.clone()),
                about,
                website: Some(actor.url.clone().unwrap_or_else(|| actor.id.clone())),
                picture: actor.icon.clone(),
                banner: actor.image.clone(),
                nip05,
                lud16,
                ..Default::default()
            }
            .as_json(),
            event_tag(
                actor.id.clone(),
                actor.tag.iter().filter_map(|t| match t {
                    NoteTagForDe::Emoji { name, icon } => Some(
                        nostr_lib::TagStandard::Emoji {
                            shortcode: name.trim_matches(':').to_string(),
                            url: icon.url.clone().into(),
                        }
                        .into(),
                    ),
                    NoteTagForDe::Hashtag { name } => Some(
                        nostr_lib::TagStandard::Hashtag(
                            name.strip_prefix('#').unwrap_or(name).to_string(),
                        )
                        .into(),
                    ),
                    _ => None,
                }),
            ),
        )
        .to_event(&key)
        .unwrap();
        static MAIL_BOX: Lazy<Vec<(Url, Option<RelayMetadata>)>> = Lazy::new(|| {
            OUTBOX_RELAYS
                .iter()
                .map(|r| {
                    let marker = if INBOX_RELAYS.contains(r) {
                        None
                    } else {
                        Some(RelayMetadata::Write)
                    };
                    (Url::parse(r).unwrap(), marker)
                })
                .chain(INBOX_RELAYS.iter().filter_map(|r| {
                    if OUTBOX_RELAYS.contains(r) {
                        None
                    } else {
                        Some((Url::parse(r).unwrap(), Some(RelayMetadata::Read)))
                    }
                }))
                .collect()
        });
        let kind10002 = EventBuilder::relay_list(MAIL_BOX.clone())
            .to_event(&key)
            .unwrap();
        tokio::join!(
            self.nostr
                .send(Arc::new(metadata), self.metadata_relays.clone()),
            self.nostr
                .send(Arc::new(kind10002), self.metadata_relays.clone()),
        );
        let new = self.db.get_ap_id_of_npub(&actor.npub).is_none();
        if new {
            self.db
                .insert_ap_id_of_npub(&actor.npub, Arc::new(actor.id.clone()));
        }
        Ok(new)
    }

    pub async fn get_ap_id_from_webfinger(&self, name: &str, host: &str) -> Result<String, Error> {
        #[derive(Deserialize, Debug)]
        struct WebfingerResponse {
            links: Vec<WebfingerLink>,
        }
        #[derive(Deserialize, Debug)]
        struct WebfingerLink {
            r#type: Option<mediatype::MediaTypeBuf>,
            href: Option<String>,
        }
        let WebfingerResponse { links } = self
            .http_client
            .get(format!(
                "https://{host}/.well-known/webfinger?resource=acct:{name}@{host}"
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
        use mediatype::ReadParams;
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
        Ok(id)
    }
}

#[derive(Debug, Clone)]
pub enum ActorOrProxied {
    Proxied(Arc<String>),
    Actor(Arc<Actor>),
}

#[derive(Debug)]
pub struct Actor {
    pub public_key: sigh::PublicKey,
    pub inbox: Option<Url>,
    pub summary: Option<String>,
    pub icon: Option<String>,
    pub image: Option<String>,
    pub name: String,
    pub nsec: nostr_lib::SecretKey,
    pub npub: nostr_lib::PublicKey,
    pub url: Option<String>,
    pub id: String,
    pub preferred_username: Option<String>,
    pub tag: Vec<NoteTagForDe>,
    pub property_values: Vec<PropertyValue>,
}

pub static HASHTAG_LINK_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?x)
        \[(?<tag>\#\w+)\]\([^)]*\)",
    )
    .unwrap()
});

impl<'a> Deserialize<'a> for ActorOrProxied {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'a>,
    {
        let a = ActorForParse::deserialize(deserializer)?;
        let summary = a.summary.map(|a| {
            HASHTAG_LINK_REGEX
                .replace_all(&html_to_text(&a), "$tag")
                .into_owned()
        });
        let mut hasher = Sha3_256::default();
        hasher.update(a.id.as_bytes());
        hasher.update(SECRET_KEY.as_bytes());
        let hash = hasher.finalize_fixed();
        let nsec = nostr_lib::SecretKey::from_slice(&hash).unwrap();
        if let Some(npub) = a.url.proxied_from {
            Ok(ActorOrProxied::Proxied(Arc::new(npub)))
        } else if let Some(ProxyOf { proxied: npub }) = a.proxy_of {
            Ok(ActorOrProxied::Proxied(Arc::new(npub)))
        } else {
            Ok(ActorOrProxied::Actor(Arc::new(Actor {
                public_key: a.public_key.public_key_pem,
                inbox: a.endpoints.and_then(|a| a.shared_inbox).or(a.inbox),
                summary,
                icon: a.icon.and_then(|a| a.get_first().map(|a| a.url)),
                image: a.image.and_then(|a| a.get_first().map(|a| a.url)),
                name: a
                    .name
                    .or_else(|| a.preferred_username.clone())
                    .unwrap_or_else(|| a.id.clone()),
                npub: nostr_lib::Keys::new(nsec.clone()).public_key(),
                nsec,
                url: a.url.url,
                id: a.id.clone(),
                preferred_username: a.preferred_username,
                tag: a.tag,
                property_values: a
                    .attachment
                    .into_iter()
                    .flat_map(|a| match a {
                        ActorAttachment::PropertyValue { name, value } => Some(PropertyValue {
                            name,
                            value: html_to_text(&value),
                        }),
                        ActorAttachment::Other(_) => None,
                    })
                    .collect(),
            })))
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ActorForParse {
    public_key: PublicKeyJsonInner,
    endpoints: Option<EndPoints>,
    inbox: Option<Url>,
    summary: Option<String>,
    icon: Option<ListOrSingle<UrlStruct>>,
    image: Option<ListOrSingle<UrlStruct>>,
    name: Option<String>,
    preferred_username: Option<String>,
    id: String,
    #[serde(default)]
    url: ActorUrl,
    proxy_of: Option<ProxyOf>,
    #[serde(default)]
    tag: Vec<NoteTagForDe>,
    #[serde(default)]
    attachment: Vec<ActorAttachment>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum ListOrSingle<T> {
    Single(T),
    Vec(Vec<OptionForDe<T>>),
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum OptionForDe<T> {
    Some(T),
    None(IgnoredAny),
}

impl<T> From<OptionForDe<T>> for Option<T> {
    fn from(value: OptionForDe<T>) -> Self {
        match value {
            OptionForDe::Some(a) => Some(a),
            OptionForDe::None(_) => None,
        }
    }
}

impl<T> ListOrSingle<T> {
    fn get_first(self) -> Option<T> {
        match self {
            ListOrSingle::Single(a) => Some(a),
            ListOrSingle::Vec(a) => a.into_iter().filter_map(|a| a.into()).next(),
        }
    }
}

#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "protocol", rename = "https://github.com/nostr-protocol/nostr")]
pub struct ProxyOf {
    pub proxied: String,
}

#[derive(Clone, Debug)]
pub struct PropertyValue {
    name: String,
    value: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ActorAttachment {
    PropertyValue {
        name: String,
        value: String,
    },
    #[serde(untagged)]
    Other(IgnoredAny),
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct LinkForDe {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rel: Option<String>,
    pub href: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ActorUrl {
    pub url: Option<String>,
    pub proxied_from: Option<String>,
}

impl<'a> Deserialize<'a> for ActorUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'a>,
    {
        #[derive(Deserialize, Debug, Clone, PartialEq)]
        #[serde(untagged)]
        pub enum ActorUrlForDe {
            Simple(String),
            Single(LinkForDe),
            Array(Vec<LinkForDe>),
        }
        Ok(match Option::<ActorUrlForDe>::deserialize(deserializer)? {
            Some(ActorUrlForDe::Simple(url)) => ActorUrl {
                url: Some(url),
                proxied_from: None,
            },
            Some(ActorUrlForDe::Single(l)) => {
                if l.href.starts_with("nostr:") && l.rel.map(|a| a == "canonical").unwrap_or(false)
                {
                    ActorUrl {
                        url: None,
                        proxied_from: Some(l.href[6..].to_string()),
                    }
                } else {
                    ActorUrl {
                        url: Some(l.href),
                        proxied_from: None,
                    }
                }
            }
            Some(ActorUrlForDe::Array(ls)) => {
                let mut url = None;
                let mut proxied_from = None;
                for l in ls {
                    if let Some(rel) = l.rel {
                        if proxied_from.is_none()
                            && l.href.starts_with("nostr:")
                            && rel == "canonical"
                        {
                            proxied_from = Some(l.href[6..].to_string());
                        }
                    } else if url.is_none() {
                        url = Some(l.href);
                    }
                }
                ActorUrl { url, proxied_from }
            }
            None => ActorUrl {
                url: None,
                proxied_from: None,
            },
        })
    }
}

#[derive(Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UrlStruct {
    pub url: String,
}

#[derive(Deserialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AttachedImage {
    pub url: String,
    pub media_type: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct PublicKeyJsonInner {
    #[serde(deserialize_with = "deserialize_pem")]
    public_key_pem: sigh::PublicKey,
}

fn deserialize_pem<'de, D>(deserializer: D) -> Result<sigh::PublicKey, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = sigh::PublicKey;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a pem")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            sigh::PublicKey::from_pem(v.as_bytes()).map_err(|e| E::custom(e))
        }
    }
    deserializer.deserialize_str(Visitor)
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct EndPoints {
    shared_inbox: Option<Url>,
}

#[cfg(test)]
mod tests {
    use super::{ListOrSingle, NoteForDe, UrlStruct};
    use crate::activity::{ActivityForDeInner, ActorOrProxied, OptionForDe, StrOrId};
    use serde::de::IgnoredAny;

    #[test]
    fn activity_de_1() {
        let a = r##"{"@context":"https://www.w3.org/ns/activitystreams","id":"https://example.com/users/example#delete","type":"Delete","to":["https://www.w3.org/ns/activitystreams#Public"],"object":"https://example.com/users/example","signature":{"type":"RsaSignature2017","creator":"https://example.com/users/example#main-key","created":"2024-03-03T06:10:00Z","signatureValue":"GSezGidctZL35ZWgUf4Kw59qwQF+lb/soQ2pvBweNfk3+k2YfgVwCXN4wNBuLwOZ2jAiRyKYlwSC6V52FhgIU0CCUjIYSCUSijPkqbfdj7KshCH3RxrVymqe1jbh+O6epZY5WRDbe93a7NHgiYCdjdWvUR8jNeoHjkOdpq4gB1GoCtfF68tZX/ExnuT28b8kh5EkWyuxp46tQ//uhCKDUI5wCD3oB9PZV7NoeV0tp2xKEjRFQf3dZbUTpdHO8k24sCDl3+aRm9jWnsQ7I/K4FYrFq0RPLxstxq5lnNKhGOpLswYFjNvCW2C4qX3IVce+6aYDcoP+E26QQlgmknxhiA=="}}"##;
        if let ActivityForDeInner::Delete {
            object: StrOrId(id),
        } = serde_json::from_str(a).unwrap()
        {
            assert_eq!(id, "https://example.com/users/example");
        } else {
            panic!()
        }
    }

    #[test]
    fn activity_de_2() {
        let a = r##"{"@context":["https://www.w3.org/ns/activitystreams","https://w3id.org/security/v1",{"Key":"sec:Key","manuallyApprovesFollowers":"as:manuallyApprovesFollowers","sensitive":"as:sensitive","Hashtag":"as:Hashtag","quoteUrl":"as:quoteUrl","toot":"http://joinmastodon.org/ns#","Emoji":"toot:Emoji","featured":"toot:featured","discoverable":"toot:discoverable","schema":"http://schema.org#","PropertyValue":"schema:PropertyValue","value":"schema:value","misskey":"https://misskey-hub.net/ns#","_misskey_content":"misskey:_misskey_content","_misskey_quote":"misskey:_misskey_quote","_misskey_reaction":"misskey:_misskey_reaction","_misskey_votes":"misskey:_misskey_votes","_misskey_summary":"misskey:_misskey_summary","isCat":"misskey:isCat","vcard":"http://www.w3.org/2006/vcard/ns#"}],"type":"Delete","object":{"id":"https://example.com/notes/aaa","type":"Tombstone"},"published":"2024-03-03T12:00:14.757Z","id":"https://example.com"}"##;
        if let ActivityForDeInner::Delete {
            object: StrOrId(id),
        } = serde_json::from_str(a).unwrap()
        {
            assert_eq!(id, "https://example.com/notes/aaa");
        } else {
            panic!()
        }
    }

    #[test]
    fn activity_de_3() {
        let a = r##"{"type":"Update","object":{"@context":["https://www.w3.org/ns/activitystreams","https://w3id.org/security/v1"],"type":"Person","id":"https://example.com/users/a","preferredUsername":"a","name":"test","inbox":"https://momostr.pink/inbox","sharedInbox":"https://momostr.pink/inbox","endpoints":{"sharedInbox":"https://momostr.pink/inbox"},"summary":"list","icon":{"type":"Image","url":"https://image.nostr.build/12f71e76bb9bd2b9b4bea58348c08d78ab7550566a468bb524021bc9875a15c7.jpg"},"manuallyApprovesFollowers":false,"discoverable":true,"publicKey":{"id":"https://example.com/users/a","type":"Key","owner":"https://example.com/users/a","publicKeyPem":"-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAs8T30Ro4ga5Fo4ArMUfB\niBXwMtHIThmBZEYBhLFUOXNswDADd1LyIZ0yt2qDlIae646C9RWqXB3qrhr3TpcA\nBDBKc1XxffSAmOzNzoFJ2FdXET97KJ2hXhfILcuMPz3MMBBNbpmgOMb4tKFpiFqH\nYhZIJGeTOUQ8VjWaiH8szixKBByVbgZOWisD9Zf39nCSQ3JJ2LvrzUIhfmocfidL\nekUtwSSi7gzr/53KpS08jP5fCaHs7S5NsgeOE6KnWpNrM19hxk7CtRJqvEbAw4yG\nxcDdvW/UYqI6hHYVmYRRkYs4NO34ZfM6v/xcFgmsMwEBaNBE0itMCMziPJ9pvyCc\nQwIDAQAB\n-----END PUBLIC KEY-----\n"}}}"##;
        let a: crate::activity::ActivityForDeInner = serde_json::from_str(a).unwrap();
        assert!(matches!(a, ActivityForDeInner::Update { .. }));
    }

    #[test]
    fn actor_de_1() {
        let a = r##"{"@context":["https://www.w3.org/ns/activitystreams","https://w3id.org/security/v1"],"type":"Person","id":"https://example.com/users/a","preferredUsername":"a","name":"test","inbox":"https://momostr.pink/inbox","sharedInbox":"https://momostr.pink/inbox","endpoints":{"sharedInbox":"https://momostr.pink/inbox"},"url":[{"type":"Link","href":"https://example.com/@a"},{"type":"Link","rel":"canonical","href":"nostr:npub1tv6h9amqvd86znquru2m3j9tszc43lul63dwhdgxe0d2lkz33asswd4yyj"}],"summary":"list","icon":{"type":"Image","url":"https://image.nostr.build/12f71e76bb9bd2b9b4bea58348c08d78ab7550566a468bb524021bc9875a15c7.jpg"},"manuallyApprovesFollowers":false,"discoverable":true,"publicKey":{"id":"https://example.com/users/a","type":"Key","owner":"https://example.com/users/a","publicKeyPem":"-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAs8T30Ro4ga5Fo4ArMUfB\niBXwMtHIThmBZEYBhLFUOXNswDADd1LyIZ0yt2qDlIae646C9RWqXB3qrhr3TpcA\nBDBKc1XxffSAmOzNzoFJ2FdXET97KJ2hXhfILcuMPz3MMBBNbpmgOMb4tKFpiFqH\nYhZIJGeTOUQ8VjWaiH8szixKBByVbgZOWisD9Zf39nCSQ3JJ2LvrzUIhfmocfidL\nekUtwSSi7gzr/53KpS08jP5fCaHs7S5NsgeOE6KnWpNrM19hxk7CtRJqvEbAw4yG\nxcDdvW/UYqI6hHYVmYRRkYs4NO34ZfM6v/xcFgmsMwEBaNBE0itMCMziPJ9pvyCc\nQwIDAQAB\n-----END PUBLIC KEY-----\n"}}"##;
        let a: ActorOrProxied = serde_json::from_str(a).unwrap();
        if let ActorOrProxied::Proxied(npub) = a {
            assert_eq!(
                *npub,
                "npub1tv6h9amqvd86znquru2m3j9tszc43lul63dwhdgxe0d2lkz33asswd4yyj"
            )
        } else {
            panic!()
        }
    }

    #[test]
    fn actor_de_2() {
        let a = r##"{"@context":["https://www.w3.org/ns/activitystreams","https://w3id.org/security/v1"],"type":"Person","id":"https://example.com/users/a","preferredUsername":"a","name":"test","inbox":"https://momostr.pink/inbox","sharedInbox":"https://momostr.pink/inbox","endpoints":{"sharedInbox":"https://momostr.pink/inbox"},"summary":"list","icon":{"type":"Image","url":"https://image.nostr.build/12f71e76bb9bd2b9b4bea58348c08d78ab7550566a468bb524021bc9875a15c7.jpg"},"manuallyApprovesFollowers":false,"discoverable":true,"publicKey":{"id":"https://example.com/users/a","type":"Key","owner":"https://example.com/users/a","publicKeyPem":"-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAs8T30Ro4ga5Fo4ArMUfB\niBXwMtHIThmBZEYBhLFUOXNswDADd1LyIZ0yt2qDlIae646C9RWqXB3qrhr3TpcA\nBDBKc1XxffSAmOzNzoFJ2FdXET97KJ2hXhfILcuMPz3MMBBNbpmgOMb4tKFpiFqH\nYhZIJGeTOUQ8VjWaiH8szixKBByVbgZOWisD9Zf39nCSQ3JJ2LvrzUIhfmocfidL\nekUtwSSi7gzr/53KpS08jP5fCaHs7S5NsgeOE6KnWpNrM19hxk7CtRJqvEbAw4yG\nxcDdvW/UYqI6hHYVmYRRkYs4NO34ZfM6v/xcFgmsMwEBaNBE0itMCMziPJ9pvyCc\nQwIDAQAB\n-----END PUBLIC KEY-----\n"}}"##;
        let a: ActorOrProxied = serde_json::from_str(a).unwrap();
        if let ActorOrProxied::Actor(actor) = a {
            assert!(actor.url.is_none());
        } else {
            panic!()
        }
    }

    #[test]
    fn note_de_1() {
        let a = r##"{"@context":["https://www.w3.org/ns/activitystreams",{"ostatus":"http://ostatus.org#","atomUri":"ostatus:atomUri","inReplyToAtomUri":"ostatus:inReplyToAtomUri","conversation":"ostatus:conversation","sensitive":"as:sensitive","toot":"http://joinmastodon.org/ns#","votersCount":"toot:votersCount"}],"id":"https://example.com/users/momo_test/statuses/112114313751387030","type":"Note","summary":null,"inReplyTo":null,"published":"2024-03-18T02:24:24Z","url":"https://example.com/@momo_test/112114313751387030","attributedTo":"https://example.com/users/momo_test","to":["https://www.w3.org/ns/activitystreams#Public"],"cc":["https://example.com/users/momo_test/followers"],"sensitive":false,"atomUri":"https://example.com/users/momo_test/statuses/112114313751387030","inReplyToAtomUri":null,"conversation":"tag:pawoo.net,2024-03-18:objectId=473072274:objectType=Conversation","content":"\u003cp\u003etest⛈\u003c/p\u003e","contentMap":{"ja":"\u003cp\u003etest⛈\u003c/p\u003e"},"attachment":[],"tag":[],"replies":{"id":"https://example.com/users/momo_test/statuses/112114313751387030/replies","type":"Collection","first":{"type":"CollectionPage","next":"https://example.com/users/momo_test/statuses/112114313751387030/replies?only_other_accounts=true\u0026page=true","partOf":"https://example.com/users/momo_test/statuses/112114313751387030/replies","items":[]}}}"##;
        let _: NoteForDe = serde_json::from_str(a).unwrap();
    }

    #[test]
    fn list_or_single_de_1() {
        let s = r##"{"url":"a"}"##;
        let a: ListOrSingle<UrlStruct> = serde_json::from_str(s).unwrap();
        assert_eq!(
            a,
            ListOrSingle::Single(UrlStruct {
                url: "a".to_string(),
            })
        );
    }

    #[test]
    fn list_or_single_de_2() {
        let s = r##"[{},{"url":"a"}]"##;
        let a: ListOrSingle<UrlStruct> = serde_json::from_str(s).unwrap();
        assert_eq!(
            a,
            ListOrSingle::Vec(vec![
                OptionForDe::None(IgnoredAny),
                OptionForDe::Some(UrlStruct {
                    url: "a".to_string(),
                })
            ])
        );
    }

    #[test]
    fn list_or_single_de_3() {
        let s = r##"1"##;
        let a: OptionForDe<UrlStruct> = serde_json::from_str(s).unwrap();
        assert_eq!(a, OptionForDe::None(IgnoredAny));
    }
}
