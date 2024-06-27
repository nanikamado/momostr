use super::AppState;
use crate::activity::{
    AcceptActivity, ActivityForDe, ActivityForDeInner, Actor, ActorOrProxied, FollowActivity,
    NoteForDe, NoteTagForDe, StrOrId, AS_PUBLIC, HASHTAG_LINK_REGEX,
};
use crate::error::Error;
use crate::util::get_media_type;
use crate::{
    html_to_text, RelayId, CONTACT_LIST_LEN_LIMIT, DOMAIN, MAIN_RELAY, NOTE_ID_PREFIX, REVERSE_DNS,
    USER_ID_PREFIX,
};
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::uri;
use axum_macros::debug_handler;
use itertools::Itertools;
use nostr_lib::nips::nip10::Marker;
use nostr_lib::{
    Event, EventBuilder, FromBech32, PublicKey, Tag, TagKind, TagStandard, Timestamp, ToBech32,
};
use once_cell::sync::Lazy;
use regex::Regex;
use relay_pool::EventWithRelayId;
use rustc_hash::FxHashSet;
use std::borrow::{Borrow, Cow};
use std::fmt::Write;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, error, info, trace};

#[debug_handler]
#[tracing::instrument(skip_all)]
pub async fn http_post_inbox(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<(), Error> {
    let signature = sigh::Signature::from(&request);
    let body = to_bytes(request.into_body(), 1_000_000_000).await?;
    debug!("/inbox <== {}", std::str::from_utf8(&body).unwrap());
    let activity: ActivityForDe = serde_json::from_slice(&body)?;
    if let ActivityForDeInner::Delete { object } = &activity.activity_inner {
        let object_id =
            InternalApId::get(Cow::Owned(object.0.to_string()), activity.actor.as_ref())?;
        if state.db.get_event_id_from_ap_id(&object_id).is_none() {
            trace!("ignored delete activity as the object not found");
            return Ok(());
        }
    }
    let actor = state.get_actor_data(activity.actor.as_ref()).await?;
    let ActorOrProxied::Actor(actor) = actor else {
        return Err(Error::BadRequest(Some(
            "proxied activitypub account cannot follow accounts of this server".to_string(),
        )));
    };
    {
        if !signature
            .verify(&actor.public_key)
            .map_err(|e| Error::BadRequest(Some(e.to_string())))?
        {
            return Err(Error::BadRequest(Some(
                "failed to verify HTTP signature".to_string(),
            )));
        }
    }
    let ActivityForDe {
        activity_inner,
        actor: actor_id,
    } = activity;
    match activity_inner {
        ActivityForDeInner::Follow { object, id } => {
            info!("{actor_id} followed {object}");
            let followee = get_npub_from_actor_id(object.as_ref())
                .ok_or_else(|| Error::BadRequest(Some("object not found".to_string())))?;
            state
                .db
                .insert_follower_of_nostr(followee, actor_id.to_string());
            let object = object.to_string();
            let inbox = actor.inbox.clone();
            let actor_id = actor_id.to_string();
            let id = id.map(|a| (*a).to_owned());
            tokio::spawn(async move {
                if let Some(inbox) = inbox {
                    let _ = state
                        .send_activity(
                            &inbox,
                            object.as_str(),
                            AcceptActivity {
                                actor: object.as_str(),
                                object: FollowActivity {
                                    actor: actor_id.as_str(),
                                    object: object.as_str(),
                                    id: id.as_deref(),
                                },
                            },
                        )
                        .await;
                }
                let tags = {
                    let l = state.db.insert_followee_of_ap(actor_id, followee);
                    if l.len() < CONTACT_LIST_LEN_LIMIT {
                        l.iter()
                            .map(|p| nostr_lib::Tag::public_key(*p))
                            .chain(std::iter::once(
                                TagStandard::LabelNamespace(REVERSE_DNS.to_string()).into(),
                            ))
                            .collect_vec()
                    } else {
                        Vec::new()
                    }
                };
                let l = EventBuilder::new(nostr_lib::Kind::ContactList, "", tags)
                    .custom_created_at(Timestamp::now())
                    .to_event(&nostr_lib::Keys::new(actor.nsec.clone()))
                    .unwrap();
                state.nostr_send(Arc::new(l)).await;
            });
        }
        ActivityForDeInner::Undo { object } => match object.activity_inner {
            ActivityForDeInner::Follow { object, .. } => {
                info!("{actor_id} unfollowed {object}");
                let object = get_npub_from_actor_id(object.as_ref())
                    .ok_or_else(|| Error::BadRequest(Some("object not found".to_string())))?;
                let actor_id = actor_id.to_string();
                state.db.remove_follower_of_nostr(object, &actor_id);
                let tags = {
                    let l = state.db.remove_followee_of_ap(actor_id, &object);
                    if l.len() < CONTACT_LIST_LEN_LIMIT {
                        Some(
                            l.iter()
                                .map(|p| nostr_lib::Tag::public_key(*p))
                                .collect_vec(),
                        )
                    } else {
                        None
                    }
                };
                if let Some(mut tags) = tags {
                    tags.push(TagStandard::LabelNamespace(REVERSE_DNS.to_string()).into());
                    let l = EventBuilder::new(nostr_lib::Kind::ContactList, "", tags)
                        .custom_created_at(Timestamp::now())
                        .to_event(&nostr_lib::Keys::new(actor.nsec.clone()))
                        .unwrap();
                    state.nostr_send(Arc::new(l)).await;
                }
            }
            ActivityForDeInner::Like { id, .. } | ActivityForDeInner::Announce { id, .. } => {
                debug!("undo like {id}");
                let object_id = InternalApId::get(Cow::Owned(id.to_string()), actor_id.as_ref())?;
                if let Some(e) = state.db.get_event_id_from_ap_id(&object_id) {
                    let nsec = actor.nsec.clone();
                    tokio::spawn(async move {
                        state
                            .nostr_send(Arc::new(
                                EventBuilder::delete([e])
                                    .add_tags([TagStandard::LabelNamespace(
                                        REVERSE_DNS.to_string(),
                                    )
                                    .into()])
                                    .to_event(&nostr_lib::Keys::new(nsec.clone()))
                                    .unwrap(),
                            ))
                            .await;
                    });
                } else {
                    info!("tried to delete a event but could not find it");
                }
            }
            _ => {
                info!("undo of this activity is not supported: {object:?}");
            }
        },
        ActivityForDeInner::Create { object } => {
            debug!("create");
            if let Some(npub) = object
                .url
                .proxied_from
                .as_ref()
                .or(object.proxy_of.as_ref().map(|a| &a.proxied))
            {
                return Err(Error::BadRequest(
                    format!("{npub} is already a nostr event").into(),
                ));
            }
            let ap_id = InternalApId::get(Cow::Borrowed(&object.id), &actor.id)?.into_owned();
            if state.db.get_event_id_from_ap_id(&ap_id).is_some() {
                error!("note {} already exists", object.id);
                return Ok(());
            }
            tokio::spawn(async move {
                if let Err(e) =
                    get_event_from_note(&state, *object, actor.clone(), Cow::Borrowed(&[])).await
                {
                    error!("could not convert AP note to Nostr note: {e:?}");
                }
            });
        }
        ActivityForDeInner::Like {
            object,
            content,
            id,
            tag,
        } => {
            if state.db.is_stopped_ap(actor_id.as_ref()) {
                return Ok(());
            }
            let ap_id = InternalApId::get(Cow::from(id.as_ref()), actor_id.as_ref())?.into_owned();
            if state.db.get_event_id_from_ap_id(&ap_id).is_some() {
                error!("like {} already exists", id);
                return Ok(());
            }
            let note = get_note_from_this_server(&state, object.as_ref())
                .await
                .ok_or_else(|| Error::BadRequest(Some("object not found".to_string())))?;
            let mut tags = vec![Tag::event(note.id), Tag::public_key(note.pubkey)];
            let mut content_converted = Cow::Borrowed("+");
            if let Some(content) = content {
                let shortcode = content.trim_matches(':').to_string();
                let emoji = tag
                    .iter()
                    .find_map(|t| {
                        if let NoteTagForDe::Emoji { name, icon } = t {
                            Some((name, icon))
                        } else {
                            None
                        }
                    })
                    .and_then(|(name, icon)| {
                        if name.trim_matches(':') == shortcode {
                            Some(TagStandard::Emoji {
                                shortcode,
                                url: icon.url.clone().into(),
                            })
                        } else {
                            None
                        }
                    });
                content_converted = content;
                if let Some(e) = emoji {
                    tags.push(e.into());
                }
            }
            send_event(
                &state,
                Arc::new(
                    EventBuilder::new(
                        nostr_lib::Kind::Reaction,
                        content_converted.to_string(),
                        event_tag(id.to_string(), tags),
                    )
                    .to_event(&nostr_lib::Keys::new(actor.nsec.clone()))
                    .unwrap(),
                ),
                ap_id,
            )
            .await;
        }
        ActivityForDeInner::Announce {
            id,
            object,
            published,
            to,
            cc,
        } => {
            if state.db.is_stopped_ap(actor_id.as_ref()) {
                return Ok(());
            }
            let is_private = !to
                .iter()
                .chain(cc.iter())
                .any(|a| [AS_PUBLIC, "Public", "as:Public"].contains(&a.as_ref()));
            if is_private {
                return Ok(());
            }
            let ap_id =
                InternalApId::get(Cow::Borrowed(id.as_ref()), actor_id.as_ref())?.into_owned();
            if state.db.get_event_id_from_ap_id(&ap_id).is_some() {
                error!("repost {} already exists", id);
                return Ok(());
            }
            if let Ok(event) =
                get_event_from_object_id(&state, object.0.to_string(), Cow::Borrowed(&[])).await
            {
                let event = EventBuilder::new(
                    nostr_lib::Kind::Repost,
                    "",
                    event_tag(
                        id.to_string(),
                        [
                            TagStandard::Event {
                                event_id: event.event.id,
                                relay_url: Some(
                                    state.relay_url[event.relay_id.0 as usize].clone().into(),
                                ),
                                marker: None,
                            }
                            .into(),
                            Tag::public_key(event.event.pubkey),
                        ],
                    ),
                )
                .custom_created_at(Timestamp::from(published.timestamp() as u64))
                .to_event(&nostr_lib::Keys::new(actor.nsec.clone()))
                .unwrap();
                send_event(&state, Arc::new(event), ap_id.into_owned()).await;
            }
        }
        ActivityForDeInner::Delete {
            object: StrOrId(id),
        } => {
            let object_id = InternalApId::get(Cow::Owned(id.to_string()), actor_id.as_ref())?;
            if let Some(e) = state.db.get_event_id_from_ap_id(&object_id) {
                info!("sending delete request ...");
                let nsec = actor.nsec.clone();
                tokio::spawn(async move {
                    state
                        .nostr_send(Arc::new(
                            EventBuilder::delete([e])
                                .add_tags([
                                    TagStandard::LabelNamespace(REVERSE_DNS.to_string()).into()
                                ])
                                .to_event(&nostr_lib::Keys::new(nsec.clone()))
                                .unwrap(),
                        ))
                        .await;
                    state.event_deletion_queue.delete(e, nsec)
                });
            } else {
                info!("tried to delete a event but could not find it");
            }
        }
        ActivityForDeInner::Update { object } => {
            info!("update of actor");
            state.update_actor_metadata(&object, None).await?;
        }
        ActivityForDeInner::Other(a) => {
            info!("not implemented {}", a);
        }
    }
    Ok(())
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct InternalApId<'a>(Cow<'a, str>);

impl<'a> InternalApId<'a> {
    pub fn as_bytes(&'a self) -> &'a [u8] {
        self.0.as_bytes()
    }

    pub fn into_owned(self) -> InternalApId<'static> {
        InternalApId(Cow::Owned(self.0.into_owned()))
    }

    fn get(ap_id: Cow<'a, str>, actor_id: &str) -> Result<InternalApId<'a>, Error> {
        let actor_id = uri::Uri::from_str(actor_id)?;
        let host = actor_id
            .host()
            .ok_or_else(|| Error::BadRequest(Some("actor id is not a url".to_string())))?;
        if uri::Uri::from_str(ap_id.as_ref())
            .ok()
            .and_then(|url| url.host().map(|a| a == host))
            .unwrap_or(false)
        {
            Ok(InternalApId(ap_id))
        } else {
            Err(Error::BadRequest(Some(format!(
                "activity id is {ap_id} but it's author has different host name {host}"
            ))))
        }
    }

    fn get_unchecked(ap_id: Cow<'a, str>) -> InternalApId<'a> {
        Self(ap_id)
    }
}

async fn send_event(state: &AppState, event: Arc<Event>, ap_id: InternalApId<'static>) {
    state.db.insert_ap_id_to_event_id(ap_id, event.id);
    state.nostr_send(event).await;
}

async fn get_note_from_this_server(state: &AppState, url: &str) -> Option<Arc<Event>> {
    let object = url.get(NOTE_ID_PREFIX.len()..)?;
    let object = nostr_lib::EventId::from_bech32(object).ok()?;
    state.get_note(object).await.map(|e| e.event)
}

fn get_npub_from_actor_id(id: &str) -> Option<PublicKey> {
    id.strip_prefix(USER_ID_PREFIX)
        .and_then(|npub| PublicKey::from_bech32(npub).ok())
}

pub fn event_tag(id: String, tags: impl IntoIterator<Item = Tag>) -> Vec<nostr_lib::Tag> {
    let id_for_l = format!("{}.activitypub:{id}", *REVERSE_DNS);
    tags.into_iter()
        .chain(
            [
                TagStandard::Proxy {
                    id,
                    protocol: nostr_lib::nips::nip48::Protocol::ActivityPub,
                },
                TagStandard::LabelNamespace(REVERSE_DNS.to_string()),
                TagStandard::Label(vec![id_for_l, REVERSE_DNS.to_string()]),
                TagStandard::Expiration(Timestamp::now() + 2592000),
            ]
            .map(|t| t.into()),
        )
        .collect()
}

#[tracing::instrument(skip_all)]
#[async_recursion::async_recursion]
async fn get_event_from_object_id<'a>(
    state: &'a AppState,
    url: String,
    mut visited: Cow<'a, [String]>,
) -> Result<EventWithRelayId<RelayId>, NostrConversionError> {
    if let Some(event_id) = url.strip_prefix(NOTE_ID_PREFIX) {
        let event_id = nostr_lib::EventId::from_bech32(event_id)
            .map_err(|_| NostrConversionError::InvalidEventId)?;
        return state
            .get_note(event_id)
            .await
            .ok_or(NostrConversionError::CouldNotGetEventFromNostr);
    }
    if visited.contains(&url) {
        return Err(NostrConversionError::CyclicReference);
    }
    if visited.len() > 100 {
        return Err(NostrConversionError::TooLongThread);
    }
    if let Some(e) = state
        .db
        .get_event_id_from_ap_id(&InternalApId::get_unchecked(Cow::Owned(url.clone())))
    {
        if let Some(e) = state.get_note(e).await {
            return Ok(e);
        }
    }
    let note: NoteForDe = state
        .get_activity_json_with_retry(&url.parse::<uri::Uri>().unwrap())
        .await
        .map_err(|_| NostrConversionError::CouldNotGetObjectFromAp)?;
    if let Some(event_id) = &note.url.proxied_from {
        let event_id = nostr_lib::EventId::from_bech32(event_id)
            .map_err(|_| NostrConversionError::InvalidEventId)?;
        return state
            .get_note(event_id)
            .await
            .ok_or(NostrConversionError::CouldNotGetEventFromNostr);
    }
    let ActorOrProxied::Actor(actor) = state
        .get_actor_data(&note.attributed_to)
        .await
        .map_err(|_| NostrConversionError::CouldNotGetObjectFromAp)?
    else {
        return Err(NostrConversionError::IsProxied);
    };
    visited.to_mut().push(url);
    get_event_from_note(state, note, actor, visited)
        .await
        .map(|event| EventWithRelayId {
            event,
            relay_id: MAIN_RELAY,
        })
}

async fn get_npub_of_actor(state: &AppState, id: &str) -> Result<PublicKey, NostrConversionError> {
    match state
        .get_actor_data(id)
        .await
        .map_err(|_| NostrConversionError::CouldNotGetObjectFromAp)?
    {
        ActorOrProxied::Proxied(a) => {
            PublicKey::from_bech32(&*a).map_err(|_| NostrConversionError::InvalidEventId)
        }
        ActorOrProxied::Actor(a) => Ok(a.npub),
    }
}

static HEAD_MENTIONS_REGEX: Lazy<Regex> = Lazy::new(|| {
    let handle = r"@[[:word:].-]+(?:@[[:word:].-]+)?";
    let handle_text = format!(r"(?:(?:{handle}) | (?:\[{handle}\]\([^)]*\)))");
    Regex::new(&format!(
        r"(?x)
        ^
        \s*
        (?:{handle_text}\ )*{handle_text}\s*"
    ))
    .unwrap()
});

#[derive(Debug)]
enum NostrConversionError {
    IsPrivate,
    OptOutedAccount,
    IsProxied,
    CyclicReference,
    CouldNotGetEventFromNostr,
    CouldNotGetObjectFromAp,
    InvalidEventId,
    InvalidActorId,
    TooLongThread,
}

#[tracing::instrument(skip_all)]
async fn get_event_from_note<'a>(
    state: &AppState,
    note: NoteForDe,
    actor: Arc<Actor>,
    visited: Cow<'_, [String]>,
) -> Result<Arc<Event>, NostrConversionError> {
    let is_private_note = !note
        .to
        .iter()
        .chain(note.cc.iter())
        .any(|a| [AS_PUBLIC, "Public", "as:Public"].contains(&a.as_str()));
    let mut tags: FxHashSet<nostr_lib::Tag> = FxHashSet::default();
    if let Some(r) = note.summary {
        if !r.is_empty() {
            tags.insert(TagStandard::ContentWarning { reason: Some(r) }.into());
        }
    } else if note.sensitive.unwrap_or(false) {
        tags.insert(TagStandard::ContentWarning { reason: None }.into());
    }
    let is_reply = note.in_reply_to.is_some();
    if let Some(r) = note.in_reply_to {
        let e = get_event_from_object_id(state, r, Cow::Borrowed(visited.borrow())).await?;
        let mut root = None;
        for t in &e.event.tags {
            match t.as_standardized() {
                Some(TagStandard::PublicKey {
                    public_key,
                    uppercase: false,
                    ..
                }) => {
                    tags.insert(Tag::public_key(*public_key));
                }
                Some(TagStandard::Event {
                    event_id,
                    relay_url: _,
                    marker: Some(Marker::Root),
                }) => {
                    root = Some(*event_id);
                }
                _ => (),
            }
        }
        tags.insert(Tag::public_key(e.event.pubkey));
        if let Some(root) = root {
            tags.insert(
                TagStandard::Event {
                    event_id: root,
                    relay_url: None,
                    marker: Some(Marker::Root),
                }
                .into(),
            );
            tags.insert(
                TagStandard::Event {
                    event_id: e.event.id,
                    relay_url: None,
                    marker: Some(Marker::Reply),
                }
                .into(),
            );
        } else {
            tags.insert(
                TagStandard::Event {
                    event_id: e.event.id,
                    relay_url: None,
                    marker: Some(Marker::Root),
                }
                .into(),
            );
        }
    }
    for t in &note.tag {
        match t {
            NoteTagForDe::Mention { href, name: _ } => {
                if let Ok(npub) = get_npub_of_actor(state, href).await {
                    tags.insert(Tag::public_key(npub));
                } else {
                    error!("could not get npub of actor = {href}");
                }
            }
            NoteTagForDe::Emoji { name, icon } => {
                tags.insert(
                    TagStandard::Emoji {
                        shortcode: name.trim_matches(':').to_string(),
                        url: icon.url.clone().into(),
                    }
                    .into(),
                );
            }
            NoteTagForDe::Hashtag { name } => {
                tags.insert(
                    TagStandard::Hashtag(name.strip_prefix('#').unwrap_or(name).to_string()).into(),
                );
            }
            _ => (),
        }
    }
    let content_tmp: String;
    let content = match &note.source {
        Some(source) if source.media_type == "text/x.misskeymarkdown" => Cow::from(&source.content),
        _ => {
            content_tmp = html_to_text(&note.content);
            HASHTAG_LINK_REGEX.replace_all(&content_tmp, "$tag")
        }
    };
    let content = if is_reply {
        if let Some(m) = HEAD_MENTIONS_REGEX.find(&content) {
            Cow::from(&content[m.end()..])
        } else {
            content
        }
    } else {
        content
    };
    let content = if parser::mention(content.as_ref()).is_ok() {
        let mut c = String::with_capacity(content.len());
        let mut content = content.as_ref();
        while let Ok((skipped, m, r)) = parser::mention(content) {
            let npub = if m.domain.map_or(false, |d| d == DOMAIN) {
                PublicKey::from_bech32(m.username).ok()
            } else if let Some(a) = if let Some(url) = m.url {
                state.get_actor_data(url.trim_end()).await.ok()
            } else {
                None
            } {
                match a {
                    ActorOrProxied::Proxied(npub) => PublicKey::from_bech32(&*npub).ok(),
                    ActorOrProxied::Actor(actor) => Some(actor.npub),
                }
            } else {
                None
            };
            if let Some(npub) = npub {
                write!(&mut c, "{skipped}nostr:{}", &npub.to_bech32().unwrap()).unwrap();
            } else {
                write!(&mut c, "{}", &content[..content.len() - r.len()]).unwrap();
            }
            content = r;
        }
        write!(&mut c, "{}", &content).unwrap();
        Cow::from(c)
    } else {
        content
    };
    let mut content = if note.attachment.is_empty() {
        content
    } else {
        let mut content = content.into_owned();
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        for a in &note.attachment {
            writeln!(&mut content, "{}", a.url).unwrap();
            let media_type = if let Some(m) = &a.media_type {
                Some(format!("m {m}"))
            } else {
                get_media_type(&a.url, &state.http_client)
                    .await
                    .map(|m| format!("m {m}"))
            };
            tags.insert(nostr_lib::Tag::custom(
                TagKind::Custom("imeta".into()),
                [format!("url {}", a.url)].into_iter().chain(media_type),
            ));
        }
        Cow::Owned(content)
    };
    if let Some(url) = note.quote_url.or(note.misskey_quote) {
        if let Ok(e) = get_event_from_object_id(state, url.clone(), visited).await {
            tags.insert(nostr_lib::Tag::custom(
                TagKind::Custom("q".into()),
                vec![e.event.id.to_string()],
            ));
            tags.insert(
                TagStandard::PublicKey {
                    public_key: e.event.author(),
                    relay_url: None,
                    alias: None,
                    uppercase: false,
                }
                .into(),
            );
            if !content.ends_with('\n') && !content.is_empty() {
                content.to_mut().push('\n');
            }
            writeln!(
                content.to_mut(),
                "nostr:{}",
                e.event.id.to_bech32().unwrap()
            )
            .unwrap();
        } else {
            error!("could not get event id from {url}");
        }
    }
    if let Some(url) = note.url.url {
        tags.insert(
            TagStandard::Proxy {
                id: url,
                protocol: nostr_lib::nips::nip48::Protocol::Web,
            }
            .into(),
        );
    }
    if is_private_note {
        info!("skipped private note as it's not supported");
        return Err(NostrConversionError::IsPrivate);
    }
    if state.db.is_stopped_ap(&actor.id) {
        let has_mention_to_nostr = tags.iter().any(|t| {
            if let Some(TagStandard::PublicKey {
                public_key,
                uppercase: false,
                ..
            }) = t.as_standardized()
            {
                state.db.get_ap_id_of_npub(public_key).is_none()
            } else {
                false
            }
        });
        if has_mention_to_nostr {
            // TODO: notify the author that their mention would not be bridged
        }
        return Err(NostrConversionError::OptOutedAccount);
    }
    let event = EventBuilder::new(
        nostr_lib::Kind::TextNote,
        content,
        event_tag(note.id.clone(), tags),
    )
    .custom_created_at(Timestamp::from(note.published.timestamp() as u64))
    .to_event(&nostr_lib::Keys::new(actor.nsec.clone()))
    .unwrap();
    let event = Arc::new(event);
    let ap_id = InternalApId::get(note.id.into(), &actor.id)
        .map_err(|_| NostrConversionError::InvalidActorId)?
        .into_owned();
    send_event(state, event.clone(), ap_id).await;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::HEAD_MENTIONS_REGEX;
    use crate::server::inbox::HASHTAG_LINK_REGEX;
    use chrono::{DateTime, Utc};
    use nostr_lib::{EventBuilder, FromBech32, SecretKey, Timestamp, ToBech32};

    #[test]
    fn deterministic_event_id() {
        let id = EventBuilder::new(nostr_lib::Kind::TextNote, "content", [])
            .custom_created_at(Timestamp::from(
                "2024-03-02T12:13:19Z"
                    .parse::<DateTime<Utc>>()
                    .unwrap()
                    .timestamp() as u64,
            ))
            .to_event(&nostr_lib::Keys::new(
                SecretKey::from_bech32(
                    "nsec1jqkh2ldzxh9xyltzlxxtp4zjz80l2mq95zs97u42ks6c9pxetfvq2g2w2x",
                )
                .unwrap(),
            ))
            .unwrap()
            .id;
        assert_eq!(
            id.to_bech32().unwrap(),
            "note1hlwtagk67vs4tgvke2f3c0z2azp7q3667c3j550clfu9cg8md3qsvceynx"
        );
    }

    #[test]
    fn remove_mention_1() {
        let s = "[@momo_test](https://example.com/@momo_test ) test🍉";
        let a = HEAD_MENTIONS_REGEX.find(s).unwrap();
        debug_assert_eq!(&s[a.end()..], "test🍉");
    }

    #[test]
    fn remove_mention_2() {
        // cSpell:disable
        let s = "@momo_test test🍉";
        let a = HEAD_MENTIONS_REGEX.find(s).unwrap();
        debug_assert_eq!(&s[a.end()..], "test🍉");
    }

    #[test]
    fn remove_mention_3() {
        let s = "[@momo_test](https://example.com/@momo_test ) [@momo_test](https://example.com/@momo_test )\n\n[@momo_test](https://example.com/@momo_test )a";
        let a = HEAD_MENTIONS_REGEX.find(s).unwrap();
        assert_eq!(
            &s[a.end()..],
            "[@momo_test](https://example.com/@momo_test )a"
        );
    }

    #[test]
    fn remove_hashtag_link_1() {
        let s = "🍉 [#example](https://example.com/tags/example ) 🍉";
        let s = HASHTAG_LINK_REGEX.replace_all(s, "$tag");
        debug_assert_eq!(s, "🍉 #example 🍉");
    }
}
