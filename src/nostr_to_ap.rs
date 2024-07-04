use crate::activity::{
    activity_to_string, Actor, ActorOrProxied, AnnounceForSer, Attachment, CreateForSer,
    DeleteForSer, FollowActivity, ImageForSe, Note, NoteForDe, NoteTagForSer, ReactionForSer,
    UndoFollowActivity, UpdateForSer,
};
use crate::bot::{handle_dm_message_to_bot, handle_message_to_bot};
use crate::error::Error;
use crate::nostr::{get_nostr_user_data, NostrUser};
use crate::server::{metadata_to_activity, AppState};
use crate::util::get_media_type;
use crate::{
    RelayId, AP_RELAYS, BOT_PUB, DOMAIN, HTTPS_DOMAIN, NOTE_ID_PREFIX, NPUB_REG, OUTBOX_RELAYS,
    REVERSE_DNS, USER_ID_PREFIX,
};
use futures_util::StreamExt;
use html_escape::{encode_double_quoted_attribute, encode_text};
use itertools::Itertools;
use line_span::LineSpanExt;
use linkify::{Link, LinkFinder, LinkKind};
use nostr_lib::event::{Kind, TagStandard};
use nostr_lib::nips::nip10::Marker;
use nostr_lib::nips::nip19::{Nip19Event, Nip19Profile};
use nostr_lib::nips::nip48::Protocol;
use nostr_lib::types::Metadata;
use nostr_lib::util::JsonUtil;
use nostr_lib::{Event, EventId, FromBech32, PublicKey, ToBech32};
use once_cell::sync::Lazy;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::{Captures, Regex};
use relay_pool::{EventStream, EventWithRelayId};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::borrow::Cow;
use std::fmt::{Debug, Write};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tracing::{debug, error, info};

#[allow(clippy::mutable_key_type)]
#[tracing::instrument(skip_all)]
async fn broadcast_to_actors<A: Serialize, S: AsRef<str> + Debug>(
    state: &AppState,
    activity: A,
    author: &str,
    r: impl Iterator<Item = S>,
    to_relay: bool,
) -> FxHashSet<axum::http::Uri> {
    let r: Vec<_> = r.collect();
    if r.is_empty() && !to_relay {
        return FxHashSet::default();
    }
    debug!("broadcast_to_actors, r = {r:?}, to_relay = {to_relay}");
    let body = activity_to_string(activity, author, true).await;
    let mut sent = FxHashSet::default();
    for actor_id in r {
        match state.get_actor_data(actor_id.as_ref()).await {
            Ok(actor) => match actor {
                ActorOrProxied::Actor(actor) => {
                    if let Some(inbox) = &actor.inbox {
                        if sent.insert(inbox.clone()) {
                            if let Err(e) = state
                                .send_string_activity(inbox, author, body.clone())
                                .await
                            {
                                error!("could not send activity: {e:?}");
                            }
                        }
                    }
                }
                ActorOrProxied::Proxied(npub) => {
                    error!("cannot send activity to proxied ActivityPub account: {npub}");
                }
            },
            Err(e) => {
                error!("failed to get actor data while trying to send an activity: {e:?}");
            }
        }
    }
    if to_relay {
        for inbox in &*AP_RELAYS {
            let inbox = axum::http::Uri::from_str(inbox).unwrap();
            if sent.insert(inbox.clone()) {
                if let Err(e) = state
                    .send_string_activity(&inbox, author, body.clone())
                    .await
                {
                    error!("could not send activity: {e:?}");
                }
            }
        }
    }
    sent
}

#[tracing::instrument(skip_all)]
fn handle_event(
    state: &Arc<AppState>,
    EventWithRelayId { event, relay_id }: EventWithRelayId<RelayId>,
) {
    let proxied = event.tags.iter().any(|t| {
        matches!(
            t.as_standardized(),
            Some(TagStandard::Proxy {
                protocol: Protocol::ActivityPub,
                ..
            })
        )
    });
    if proxied {
        let to_bot = event.kind == Kind::TextNote
            && event.tags.iter().any(|t| {
                if let Some(TagStandard::PublicKey {
                    public_key,
                    uppercase: false,
                    ..
                }) = t.as_standardized()
                {
                    public_key == &*BOT_PUB
                } else {
                    false
                }
            });
        if to_bot {
            let state = state.clone();
            tokio::spawn(async move {
                handle_message_to_bot(&state, event).await;
            });
        }
        return;
    }
    match event.kind {
        nostr_lib::Kind::TextNote => {
            let mut ps = Vec::new();
            let mut to_bot = false;
            for t in &event.tags {
                if let Some(TagStandard::PublicKey {
                    public_key,
                    uppercase: false,
                    ..
                }) = t.as_standardized()
                {
                    if public_key == &*BOT_PUB {
                        to_bot = true;
                    }
                    if let Some(a) = state.db.get_ap_id_of_npub(public_key) {
                        ps.push(a.clone());
                    }
                }
            }
            if to_bot {
                let state = state.clone();
                let event = event.clone();
                tokio::spawn(async move {
                    handle_message_to_bot(&state, event).await;
                });
            }
            let state = state.clone();
            tokio::spawn(async move {
                let followers = {
                    let followers = state.db.get_followers_of_nostr(event.author_ref());
                    if !ps.is_empty() || !followers.as_ref().map_or(true, |a| a.is_empty()) {
                        Some(followers.unwrap_or_default())
                    } else {
                        None
                    }
                };
                if let Some(followers) = followers {
                    debug!("new note: {:?}", event.content);
                    {
                        state.note_cache.lock().put(
                            event.id,
                            Arc::new(OnceCell::const_new_with(Some(EventWithRelayId {
                                event: event.clone(),
                                relay_id,
                            }))),
                        );
                    }
                    if let Some(note) = Note::from_nostr_event(&state, &event).await {
                        #[allow(clippy::mutable_key_type)]
                        let inboxes = broadcast_to_actors(
                            &state,
                            CreateForSer {
                                actor: &note.author,
                                id: &note.id,
                                published: &note.published,
                                object: &note,
                            },
                            &note.author,
                            ps.iter()
                                .map(|a| a.as_str())
                                .chain(followers.iter().map(|a| a.as_str())),
                            true,
                        )
                        .await;
                        state
                            .db
                            .insert_event_id_to_inbox(
                                event.id.as_bytes(),
                                inboxes.into_iter().map(|l| l.to_string()),
                            )
                            .await;
                    }
                }
            });
        }
        nostr_lib::Kind::Reaction => {
            if state.db.is_stopped_npub(event.author_ref()) {
                return;
            }
            let mut e = None;
            let mut to_ap = false;
            let mut emoji = None;
            for tag in &event.tags {
                match tag.as_standardized() {
                    Some(TagStandard::Event { event_id, .. }) => {
                        e = Some(*event_id);
                    }
                    Some(TagStandard::PublicKey {
                        public_key,
                        uppercase: false,
                        ..
                    }) => {
                        to_ap |= state.db.get_ap_id_of_npub(public_key).is_some();
                    }
                    Some(TagStandard::Emoji { shortcode, url }) => {
                        emoji = Some(NoteTagForSer::Emoji {
                            name: format!(":{shortcode}:"),
                            icon: ImageForSe {
                                url: url.to_string(),
                            },
                            id: format!(
                                "https://momostr.pink/emoji/{}",
                                utf8_percent_encode(&url.to_string(), NON_ALPHANUMERIC)
                            ),
                        });
                    }
                    _ => (),
                }
            }
            if !to_ap {
                return;
            }
            let Some(e) = e else { return };
            debug!("new reaction: {}", event.content);
            let state = state.clone();
            tokio::spawn(async move {
                let Some(reacted_event) = state.get_note(e).await else {
                    return;
                };
                let reacted_p = reacted_event.event.author_ref();
                let e = match get_ap_id_from_proxied_event(&reacted_event.event) {
                    Ok(a) => a,
                    Err(GetProxiedEventError::NotProxiedEvent) => {
                        format!("{NOTE_ID_PREFIX}{}", e.to_bech32().unwrap())
                    }
                    Err(GetProxiedEventError::ProxiedByOtherBridge(_)) => {
                        return;
                    }
                };
                let author = format!("{USER_ID_PREFIX}{}", event.author().to_bech32().unwrap());
                let followers = state.db.get_followers_of_nostr(reacted_p);
                let tmp: String;
                let p = state.db.get_ap_id_of_npub(reacted_p);
                let activity = ReactionForSer {
                    actor: &author,
                    id: &event.id.to_bech32().unwrap(),
                    object: &e,
                    content: if emoji.is_some() {
                        if event.content.starts_with(':') {
                            Some(&event.content)
                        } else {
                            tmp = format!(":{}:", event.content);
                            Some(&tmp)
                        }
                    } else if event.content == "-" {
                        Some("👎")
                    } else if event.content == "+" {
                        None
                    } else {
                        Some(&event.content)
                    },
                    tag: emoji,
                };
                broadcast_to_actors(
                    &state,
                    activity,
                    &author,
                    p.as_ref().map(|a| a.as_str()).into_iter().chain(
                        followers
                            .as_ref()
                            .iter()
                            .flat_map(|a| a.iter().map(|a| a.as_str()))
                            .collect_vec(),
                    ),
                    false,
                )
                .await;
            });
        }
        nostr_lib::Kind::EventDeletion => {
            info!("event deletion");
            let author = format!("{USER_ID_PREFIX}{}", event.author().to_bech32().unwrap());
            let id = event.id.to_bech32().unwrap();
            for tag in &event.tags {
                match tag.as_standardized() {
                    Some(TagStandard::Event { event_id, .. }) => {
                        let event_id = *event_id;
                        let state = state.clone();
                        let author = author.clone();
                        let id = id.clone();
                        tokio::spawn(async move {
                            let inboxes = state.db.delete_event_id(event_id.as_bytes()).await;
                            if !inboxes.is_empty() {
                                let body = activity_to_string(
                                    &DeleteForSer {
                                        actor: &author,
                                        id: &id,
                                        object: &event_id.to_bech32().unwrap(),
                                    },
                                    &author,
                                    true,
                                )
                                .await;
                                for i in inboxes {
                                    if let Err(e) = state
                                        .send_string_activity(
                                            &axum::http::Uri::from_str(&i).unwrap(),
                                            &author,
                                            body.clone(),
                                        )
                                        .await
                                    {
                                        error!("could not send activity: {e:?}");
                                    }
                                }
                            }
                        });
                    }
                    Some(TagStandard::Proxy {
                        protocol: Protocol::ActivityPub,
                        ..
                    }) => {
                        return;
                    }
                    _ => (),
                }
            }
        }
        nostr_lib::Kind::ContactList => {
            if !state.db.is_stopped_npub(event.author_ref()) {
                let state = state.clone();
                tokio::spawn(async move {
                    update_follow_list(&state, event).await;
                });
            }
        }
        nostr_lib::Kind::Repost => {
            if state.db.is_stopped_npub(event.author_ref()) {
                return;
            }
            let mut e = None;
            let mut p = None;
            for tag in &event.tags {
                match tag.as_standardized() {
                    Some(TagStandard::Event { event_id, .. }) => {
                        e = Some(*event_id);
                    }
                    Some(TagStandard::PublicKey {
                        public_key,
                        uppercase: false,
                        ..
                    }) => {
                        p = state.db.get_ap_id_of_npub(public_key);
                    }
                    _ => (),
                }
            }
            if let Some(e) = e {
                let state = state.clone();
                tokio::spawn(async move {
                    let e = match get_ap_id_from_id_of_proxied_event(&state, e).await {
                        Ok(a) | Err(GetProxiedEventError::ProxiedByOtherBridge(a)) => a,
                        Err(GetProxiedEventError::NotProxiedEvent) => {
                            format!("{NOTE_ID_PREFIX}{}", e.to_bech32().unwrap())
                        }
                    };
                    let author = format!("{USER_ID_PREFIX}{}", event.author().to_bech32().unwrap());
                    let followers = state.db.get_followers_of_nostr(event.author_ref());
                    broadcast_to_actors(
                        &state,
                        AnnounceForSer {
                            actor: &author,
                            id: &event.id.to_bech32().unwrap(),
                            object: &e,
                            published: &event.created_at.to_human_datetime(),
                        },
                        &author,
                        p.as_ref().map(|a| a.as_str()).into_iter().chain(
                            followers
                                .as_ref()
                                .iter()
                                .flat_map(|a| a.iter().map(|a| a.as_str()))
                                .collect_vec(),
                        ),
                        false,
                    )
                    .await;
                });
            }
        }
        nostr_lib::Kind::Metadata => {
            let followers = state.db.get_followers_of_nostr(event.author_ref());
            if !followers.as_ref().map_or(true, |a| a.is_empty()) {
                if let Ok(metadata) = Metadata::from_json(&event.content) {
                    debug!("metadata update");
                    let followers = followers.unwrap().clone();
                    let state = state.clone();
                    tokio::spawn(async move {
                        let metadata =
                            metadata_to_activity(&state, event.author(), &metadata).await;
                        let actor = format!(
                            "{USER_ID_PREFIX}{}",
                            event.author_ref().to_bech32().unwrap()
                        );
                        let published = event.created_at.to_human_datetime();
                        #[allow(clippy::mutable_key_type)]
                        broadcast_to_actors(
                            &state,
                            UpdateForSer {
                                actor: &actor,
                                id: &event.id.to_bech32().unwrap(),
                                published: &published,
                                object: metadata,
                            },
                            &actor,
                            followers.iter(),
                            true,
                        )
                        .await;
                    });
                }
            };
        }
        nostr_lib::Kind::EncryptedDirectMessage => {
            let state = state.clone();
            tokio::spawn(async move {
                handle_dm_message_to_bot(&state, event).await;
            });
        }
        _ => (),
    }
}

pub async fn watch(
    mut event_stream: EventStream<RelayId>,
    state: &Arc<AppState>,
) -> Result<(), Error> {
    while let Some(e) = event_stream.next().await {
        handle_event(state, e);
    }
    Err(Error::Internal(anyhow::anyhow!("unexpected").into()))
}

struct Quote {
    ap_id: String,
    author_npub: PublicKey,
}

#[tracing::instrument(skip_all)]
async fn media<'a>(
    state: &Arc<AppState>,
    content: &'a str,
    handle_cache: &mut FxHashMap<PublicKey, Arc<(String, String)>>,
) -> (Vec<Attachment>, Content, Option<Quote>) {
    pub static NON_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\S").unwrap());
    pub static NEVENT: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?:nostr:)?(nevent1[0-9a-z]{50,}|note1[0-9a-z]{50,})").unwrap());
    let mut link_finder = LinkFinder::new();
    link_finder.kinds(&[LinkKind::Url]);
    let mut attachments = Vec::new();
    #[derive(Debug)]
    enum Segment<'a> {
        Text(&'a str),
        Image(&'a str),
        Link(&'a str),
        Event(EventId, &'a str),
    }
    let mut segments = Vec::with_capacity(5);
    for l in content.line_spans() {
        let line_start = l.start();
        let mut pos = line_start;
        let mut links = link_finder
            .links(l.as_str())
            .collect::<Vec<_>>()
            .into_iter()
            .peekable();
        let mut nevents = NEVENT.captures_iter(l.as_str()).peekable();
        loop {
            #[allow(clippy::too_many_arguments)]
            async fn link_segment<'a>(
                link: &Link<'a>,
                segments: &mut Vec<Segment<'a>>,
                attachments: &mut Vec<Attachment>,
                content: &'a str,
                pos: &mut usize,
                client: &reqwest::Client,
                line_start: usize,
            ) {
                if let Some(i) = get_media_type(link.as_str(), client).await {
                    attachments.push(Attachment {
                        media_type: i,
                        url: link.as_str().to_string(),
                    });
                    let c = &content[*pos..line_start + link.start()];
                    segments.push(Segment::Text(c));
                    segments.push(Segment::Image(link.as_str()));
                } else {
                    let c = &content[*pos..line_start + link.start()];
                    segments.push(Segment::Text(c));
                    segments.push(Segment::Link(link.as_str()));
                }
                *pos = line_start + link.end();
            }
            fn nevent_segment<'a>(
                event: &regex::Captures<'a>,
                segments: &mut Vec<Segment<'a>>,
                content: &'a str,
                pos: &mut usize,
                line_start: usize,
            ) {
                let event_start = line_start + event.get(0).unwrap().start();
                if event_start < *pos {
                    return;
                }
                let event_s = event.get(1).unwrap().as_str();
                if let Ok(e) = Nip19Event::from_bech32(event_s) {
                    segments.push(Segment::Text(&content[*pos..event_start]));
                    segments.push(Segment::Event(e.event_id, event_s));
                } else if let Ok(e) = EventId::from_bech32(event_s) {
                    segments.push(Segment::Text(&content[*pos..event_start]));
                    segments.push(Segment::Event(e, event_s));
                } else {
                    segments.push(Segment::Text(&content[*pos..event_start]));
                }
                *pos = line_start + event.get(0).unwrap().end();
            }
            match (links.peek(), nevents.peek()) {
                (Some(link), Some(event)) => {
                    if link.start() < event.get(0).unwrap().start() {
                        link_segment(
                            link,
                            &mut segments,
                            &mut attachments,
                            content,
                            &mut pos,
                            &state.http_client,
                            line_start,
                        )
                        .await;
                        links.next();
                    } else {
                        nevent_segment(event, &mut segments, content, &mut pos, line_start);
                        nevents.next();
                    }
                }
                (None, Some(e)) => {
                    nevent_segment(e, &mut segments, content, &mut pos, line_start);
                    nevents.next();
                }
                (Some(link), None) => {
                    link_segment(
                        link,
                        &mut segments,
                        &mut attachments,
                        content,
                        &mut pos,
                        &state.http_client,
                        line_start,
                    )
                    .await;
                    links.next();
                }
                (None, None) => {
                    break;
                }
            }
        }
        segments.push(Segment::Text(&content[pos..l.ending()]));
    }
    let mut quote = None;
    let mut nostr_quoted = None;
    let mut unresolved_quote = None;
    let mut end_of_visible_segment = 0;
    for (i, s) in segments.iter().enumerate().rev() {
        match s {
            Segment::Text(t) | Segment::Link(t) => {
                if NON_SPACE.is_match(t) {
                    end_of_visible_segment = i + 1;
                    break;
                }
            }
            Segment::Image(_) => {}
            Segment::Event(event_id, s) => {
                if quote.is_none() {
                    if let Some(e) = state.get_note(*event_id).await {
                        if let Ok(url) | Err(GetProxiedEventError::ProxiedByOtherBridge(url)) =
                            get_ap_id_from_proxied_event(&e.event)
                        {
                            quote = Some(Quote {
                                ap_id: url,
                                author_npub: e.event.author(),
                            })
                        } else {
                            quote = Some(Quote {
                                ap_id: format!("{NOTE_ID_PREFIX}{}", event_id.to_bech32().unwrap()),
                                author_npub: e.event.author(),
                            });
                            nostr_quoted = Some(*s);
                        };
                    } else {
                        unresolved_quote = Some(*s);
                    }
                } else {
                    end_of_visible_segment = i + 1;
                    break;
                }
            }
        }
    }
    let mut content = Content {
        html: String::with_capacity(content.len()),
        misskey: String::with_capacity(content.len()),
    };
    for (i, s) in segments.iter().enumerate() {
        if end_of_visible_segment <= i {
            break;
        }
        match s {
            Segment::Text(t) => {
                replace_npub_with_ap_handle(&mut content, t, state, handle_cache)
                    .await
                    .unwrap();
            }
            Segment::Image(url) | Segment::Link(url) => {
                content.link(url);
            }
            Segment::Event(event_id, s) => {
                match get_ap_id_from_id_of_proxied_event(state, *event_id).await {
                    Ok(id) | Err(GetProxiedEventError::ProxiedByOtherBridge(id)) => {
                        content.link(get_url_from_ap_id(state, &id).await.as_ref());
                    }
                    Err(_) => content.link(&format!("https://coracle.social/{s}")),
                };
            }
        }
    }
    let end_with_newline = content.misskey.ends_with('\n');
    if let Some(q) = &quote {
        if !end_with_newline {
            write!(&mut content.html, "<br>").unwrap();
        }
        let tmp1: String;
        let tmp2: Cow<str>;
        let url = if let Some(nevent) = nostr_quoted {
            tmp1 = format!("https://coracle.social/{nevent}");
            encode_double_quoted_attribute(&tmp1)
        } else {
            tmp2 = get_url_from_ap_id(state, &q.ap_id).await;
            encode_double_quoted_attribute(tmp2.as_ref())
        };
        write!(
            &mut content.html,
            r#"<span><br>RE: </span><a href="{url}">{url}</a>"#
        )
        .unwrap();
    }
    if let Some(s) = unresolved_quote {
        let url = format!("https://coracle.social/{s}");
        content.link(&url);
    }
    (attachments, content, quote)
}

async fn get_url_from_ap_id<'a>(state: &AppState, id: &'a str) -> Cow<'a, str> {
    async fn get_url_from_ap_id_aux(state: &AppState, id: &str) -> Option<String> {
        let id = id.parse::<axum::http::Uri>().ok()?;
        let note = state
            .get_activity_json_with_retry::<NoteForDe>(&id)
            .await
            .ok()?;
        note.url.url
    }
    if let Some(url) = get_url_from_ap_id_aux(state, id).await {
        Cow::from(url)
    } else {
        Cow::from(id)
    }
}

#[tracing::instrument(skip_all)]
pub async fn replace_npub_with_ap_handle(
    content: &mut Content,
    t: &str,
    state: &Arc<AppState>,
    handle_cache: &mut FxHashMap<PublicKey, Arc<(String, String)>>,
) -> Result<(), std::fmt::Error> {
    async fn replacement(
        content: &mut Content,
        c: &regex::Captures<'_>,
        state: &Arc<AppState>,
        handle_cache: &mut FxHashMap<PublicKey, Arc<(String, String)>>,
    ) {
        if let Ok(p) = PublicKey::from_bech32(&c[1])
            .or_else(|_| Nip19Profile::from_bech32(&c[1]).map(|p| p.public_key))
        {
            let (id, s) = &*get_ap_id_and_handle_from_public_key(state, &p, handle_cache).await;
            content.mention(id, s);
        } else {
            content.span(&c[0]);
        }
    }
    let mut last_match = 0;
    for caps in NPUB_REG.captures_iter(t) {
        let m = caps.get(0).unwrap();
        content.span(&t[last_match..m.start()]);
        replacement(content, &caps, state, handle_cache).await;
        last_match = m.end();
    }
    content.span(&t[last_match..]);
    Ok(())
}

pub struct Content {
    pub html: String,
    pub misskey: String,
}

impl Content {
    pub fn span(&mut self, s: &str) {
        static HASHTAG: Lazy<Regex> = Lazy::new(|| {
            Regex::new(
                r"#([\p{XID_Continue}\p{Emoji_Component}\p{Extended_Pictographic}_+\-&&[^#]]+)",
            )
            .unwrap()
        });
        let html_s = encode_text(s);
        let html_s = HASHTAG.replace_all(&html_s, |c: &Captures| {
            let url_tag = utf8_percent_encode(&c[1], NON_ALPHANUMERIC);
            format!(
                r#"<a href="https://coracle.social/topics/{url_tag}" rel="tag">#{}</a>"#,
                &c[1]
            )
        });
        write!(
            &mut self.html,
            "<span>{}</span>",
            html_s.split('\n').format("<br>")
        )
        .unwrap();
        self.misskey.push_str(s);
    }

    pub fn mention(&mut self, url: &str, handle: &str) {
        write!(
            &mut self.html,
            "<a href=\"{}\" class=\"u-url mention\">{handle}</a>",
            encode_double_quoted_attribute(url)
        )
        .unwrap();
        self.misskey.push_str(handle);
    }

    pub fn link(&mut self, url: &str) {
        let html_url = encode_double_quoted_attribute(&url);
        write!(&mut self.html, r#"<a href="{html_url}">{html_url}</a>"#).unwrap();
        self.misskey.push_str(url);
    }
}

enum GetProxiedEventError {
    NotProxiedEvent,
    ProxiedByOtherBridge(String),
}

#[tracing::instrument(skip_all)]
fn get_ap_id_from_proxied_event(event: &Event) -> Result<String, GetProxiedEventError> {
    let mut proxy = None;
    let mut from_this_server = false;
    use nostr_lib::nips::nip48::Protocol;
    for tag in &event.tags {
        match tag.as_standardized() {
            Some(TagStandard::Proxy {
                id,
                protocol: Protocol::ActivityPub,
            }) => {
                proxy = Some(id.clone());
            }
            Some(TagStandard::LabelNamespace(values)) => {
                from_this_server |= *values == *REVERSE_DNS;
            }
            _ => (),
        }
    }
    if let Some(proxy) = proxy {
        if from_this_server {
            Ok(proxy)
        } else {
            Err(GetProxiedEventError::ProxiedByOtherBridge(proxy))
        }
    } else {
        Err(GetProxiedEventError::NotProxiedEvent)
    }
}

#[tracing::instrument(skip_all)]
async fn get_ap_id_from_id_of_proxied_event(
    state: &AppState,
    event_id: EventId,
) -> Result<String, GetProxiedEventError> {
    let event = state
        .get_note(event_id)
        .await
        .ok_or(GetProxiedEventError::NotProxiedEvent)?
        .event;
    get_ap_id_from_proxied_event(&event)
}

impl Note {
    #[tracing::instrument(skip_all)]
    pub async fn from_nostr_event(state: &Arc<AppState>, event: &Event) -> Option<Self> {
        let author_opt_outed = state.db.is_stopped_npub(event.author_ref());
        let id = event.id.to_bech32().unwrap();
        let published = event.created_at.to_human_datetime();
        let mut handle_cache = FxHashMap::default();
        let (attachment, content, quote) = media(state, &event.content, &mut handle_cache).await;
        let mut reply = None;
        let mut root = None;
        let mut reply_positional = None;
        let mut event_has_marker = false;
        let mut tag = Vec::new();
        let mut summary = None;
        for t in &event.tags {
            match t.as_standardized() {
                Some(TagStandard::Event {
                    event_id,
                    relay_url: _,
                    marker,
                }) => match marker {
                    Some(Marker::Reply) => reply = Some(*event_id),
                    Some(Marker::Root) => root = Some(*event_id),
                    Some(_) => event_has_marker = true,
                    None => reply_positional = Some(*event_id),
                },
                Some(TagStandard::Hashtag(hashtag)) => {
                    let name = format!("#{hashtag}");
                    let href = format!(
                        "https://nostr.band/?q={}",
                        utf8_percent_encode(&name, NON_ALPHANUMERIC)
                    );
                    tag.push(NoteTagForSer::Hashtag { name, href })
                }
                Some(TagStandard::Emoji { shortcode, url }) => tag.push(NoteTagForSer::Emoji {
                    name: format!(":{shortcode}:"),
                    icon: ImageForSe {
                        url: url.to_string(),
                    },
                    id: format!(
                        "https://momostr.pink/emoji/{}",
                        utf8_percent_encode(&url.to_string(), NON_ALPHANUMERIC)
                    ),
                }),
                Some(TagStandard::PublicKey {
                    public_key,
                    uppercase: false,
                    ..
                }) => {
                    if author_opt_outed && state.db.get_ap_id_of_npub(public_key).is_some() {
                        // TODO: notify the author that their mention doesn't mirrored
                        return None;
                    }
                    if !quote
                        .as_ref()
                        .map_or(false, |a| &a.author_npub == public_key)
                    {
                        let (href, name) = (*get_ap_id_and_handle_from_public_key(
                            state,
                            public_key,
                            &mut handle_cache,
                        )
                        .await)
                            .clone();
                        tag.push(NoteTagForSer::Mention { href, name });
                    }
                }
                Some(TagStandard::ContentWarning { reason }) => {
                    summary = Some(reason.clone().unwrap_or_default());
                }
                _ => (),
            }
        }
        if author_opt_outed {
            return None;
        }
        let mut in_reply_to = None;
        if let Some(e) = reply.or(root).or({
            if event_has_marker {
                None
            } else {
                reply_positional
            }
        }) {
            match get_ap_id_from_id_of_proxied_event(state, e).await {
                Ok(a) => {
                    in_reply_to = Some(a);
                }
                Err(GetProxiedEventError::NotProxiedEvent) => {
                    in_reply_to = Some(format!("{NOTE_ID_PREFIX}{}", e.to_bech32().unwrap()));
                }
                Err(GetProxiedEventError::ProxiedByOtherBridge(_)) => {
                    info!(
                        "{} is proxied event from other bridge",
                        e.to_bech32().unwrap()
                    );
                    return None;
                }
            }
        }
        let author = format!(
            "{USER_ID_PREFIX}{}",
            event.author_ref().to_bech32().unwrap()
        );
        let nevent = Nip19Event {
            event_id: event.id,
            author: None,
            relays: OUTBOX_RELAYS.iter().map(|s| s.to_string()).collect(),
        }
        .to_bech32()
        .unwrap();
        Some(Note {
            author,
            id,
            nevent,
            content: content.html,
            misskey_content: content.misskey,
            published,
            attachment,
            in_reply_to,
            quote: quote.map(|a| a.ap_id),
            tag,
            summary,
        })
    }
}

#[tracing::instrument(skip_all)]
async fn get_ap_id_and_handle_from_public_key(
    state: &Arc<AppState>,
    public_key: &PublicKey,
    cache: &mut FxHashMap<PublicKey, Arc<(String, String)>>,
) -> Arc<(String, String)> {
    if let Some(a) = cache.get(public_key) {
        return a.clone();
    }
    async fn get_proxied(
        state: &Arc<AppState>,
        public_key: &PublicKey,
    ) -> Option<(String, String)> {
        let a = &*get_nostr_user_data(state, *public_key).await;
        if let NostrUser::Proxied(id) = &a.as_ref().ok()? {
            match state.get_actor_data(id).await {
                Ok(ActorOrProxied::Actor(a)) => {
                    return Some((
                        a.id.clone(),
                        format!(
                            "@{}@{}",
                            a.preferred_username.as_ref().unwrap_or(&a.name),
                            axum::http::Uri::from_str(&a.id).ok()?.host()?
                        ),
                    ));
                }
                Err(e) => {
                    error!("could not get actor data from {id}: {e:?}");
                }
                _ => (),
            }
        }
        None
    }
    let a = Arc::new(get_proxied(state, public_key).await.unwrap_or_else(|| {
        let p = public_key.to_bech32().unwrap();
        (format!("{USER_ID_PREFIX}{}", p), format!("@{p}@{DOMAIN}"))
    }));
    cache.insert(*public_key, a.clone());
    a
}

pub async fn update_follow_list(state: &AppState, event: Arc<Event>) {
    let follow_list_old = state.db.get_followee_of_nostr(event.author_ref());
    let follow_list_new: FxHashSet<_> = event
        .tags
        .iter()
        .filter_map(|t| {
            if let Some(TagStandard::PublicKey {
                public_key,
                uppercase: false,
                ..
            }) = t.as_standardized()
            {
                state.db.get_ap_id_of_npub(public_key)
            } else {
                None
            }
        })
        .collect();
    let npub = event.author_ref().to_bech32().unwrap();
    let author = format!("{USER_ID_PREFIX}{npub}",);
    let mut changed = false;
    for actor_id in &follow_list_new {
        if follow_list_old
            .as_ref()
            .map(|a| a.contains(&**actor_id))
            .unwrap_or(false)
        {
            continue;
        }
        changed = true;
        if let Ok(ActorOrProxied::Actor(a)) = state.get_actor_data(actor_id).await {
            if let Actor {
                inbox: Some(inbox),
                id,
                ..
            } = &*a
            {
                if let Err(e) = state
                    .send_activity(
                        inbox,
                        &author,
                        FollowActivity {
                            actor: &author,
                            object: id,
                            id: Some(&format!(
                                "{HTTPS_DOMAIN}/follow/{npub}/{}",
                                utf8_percent_encode(id, NON_ALPHANUMERIC)
                            )),
                        },
                        false,
                    )
                    .await
                {
                    error!("could not send activity: {e:?}");
                }
            }
        }
    }
    for actor_id in follow_list_old.as_ref().into_iter().flat_map(|a| a.iter()) {
        if follow_list_new.contains(actor_id) {
            continue;
        }
        changed = true;
        if let Ok(ActorOrProxied::Actor(a)) = state.get_actor_data(actor_id).await {
            if let Actor {
                inbox: Some(inbox),
                id,
                ..
            } = &*a
            {
                let escaped_id = utf8_percent_encode(id, NON_ALPHANUMERIC);
                if let Err(e) = state
                    .send_activity(
                        inbox,
                        &author,
                        UndoFollowActivity {
                            object: FollowActivity {
                                actor: &author,
                                object: id,
                                id: Some(&format!("{HTTPS_DOMAIN}/follow/{author}/{escaped_id}")),
                            },
                            actor: &author,
                            id: &format!("{HTTPS_DOMAIN}/unfollow/{author}/{escaped_id}"),
                        },
                        false,
                    )
                    .await
                {
                    error!("could not send activity: {e:?}");
                }
            }
        }
    }
    if changed {
        debug!("updated follow list: {:?}", follow_list_new);
        state
            .db
            .insert_followee_of_nostr(event.author(), follow_list_new.into());
    }
}

#[cfg(test)]
mod tests {
    use super::media;
    use crate::db::Db;
    use crate::event_deletion_queue::EventDeletionQueue;
    use crate::server::AppState;
    use crate::util::RateLimiter;
    use crate::{RelayId, NOTE_ID_PREFIX, USER_AGENT};
    use cached::TimedSizedCache;
    use itertools::Itertools;
    use lru::LruCache;
    use nostr_lib::nips::nip19::Nip19Event;
    use nostr_lib::{FromBech32, ToBech32};
    use once_cell::sync::Lazy;
    use parking_lot::Mutex;
    use relay_pool::RelayPool;
    use rustc_hash::{FxHashMap, FxHashSet};
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::runtime::Runtime;

    static RT: Lazy<Runtime> = Lazy::new(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    });

    static APP_STATE: tokio::sync::OnceCell<Arc<AppState>> = tokio::sync::OnceCell::const_new();

    async fn get_state() -> &'static Arc<AppState> {
        APP_STATE
            .get_or_init(|| async {
                // use tracing_subscriber::layer::SubscriberExt;
                // use tracing_subscriber::util::SubscriberInitExt;
                // tracing_subscriber::registry()
                //     .with(
                //         tracing_subscriber::EnvFilter::try_from_default_env()
                //             .unwrap_or_else(|_| "debug,momostr=trace,relay_pool=trace".into()),
                //     )
                //     .with(tracing_subscriber::fmt::layer())
                //     .init();

                let relays = ["wss://relay.nostr.band", "wss://relay.momostr.pink"]
                    .iter()
                    .map(|l| url::Url::parse(l).unwrap())
                    .collect_vec();
                let nostr = RelayPool::new(USER_AGENT.to_string()).await;
                let mut main_relays = FxHashSet::default();
                for (i, l) in relays.iter().enumerate() {
                    nostr.add_relay(RelayId(i as u32), l.clone()).await.unwrap();
                    main_relays.insert(RelayId(i as u32));
                }
                let main_relays = Arc::new(main_relays);
                let http_client = reqwest::Client::new();

                Arc::new(AppState {
                    nostr,
                    nostr_send_rate: Mutex::new(RateLimiter::new(u32::MAX, Duration::from_secs(0))),
                    nostr_subscribe_rate: Mutex::new(RateLimiter::new(
                        u32::MAX,
                        Duration::from_secs(0),
                    )),
                    relay_url: relays.into_iter().map(|a| a.to_string()).collect(),
                    http_client: http_client.clone(),
                    note_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
                    actor_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
                    nostr_user_cache: Mutex::new(TimedSizedCache::with_size_and_lifespan(
                        1000,
                        60 * 10,
                    )),
                    db: Db::new().await,
                    metadata_relays: main_relays.clone(),
                    main_relays,
                    event_deletion_queue: EventDeletionQueue::new(Arc::new(http_client)),
                })
            })
            .await
    }

    #[test]
    fn media_test_1() {
        let s = r#"aaa!!

https://example.com

<img src="https://pbs.twimg.com/profile_banners/12/1688241283/1500x500"/>

https://example.com
        "#;
        RT.block_on(async {
            let (media, content, q) = media(get_state().await, s, &mut FxHashMap::default()).await;
            assert!(q.is_none());
            assert_eq!(content.misskey, s.trim());
            assert_eq!(media.len(), 1);
        });
    }

    #[test]
    fn media_test_2() {
        let s = r#"#あああ
いいい
https://pbs.twimg.com/profile_banners/12/1688241283/1500x500
https://pbs.twimg.com/profile_banners/12/1688241283/1500x500

https://pbs.twimg.com/profile_banners/12/1688241283/1500x500
https://pbs.twimg.com/profile_banners/12/1688241283/1500x500
"#;
        RT.block_on(async {
            let (media, content, q) = media(get_state().await, s, &mut FxHashMap::default()).await;
            assert!(q.is_none());
            assert_eq!(content.misskey, "#あああ\nいいい\n");
            assert_eq!(media.len(), 4);
        });
    }

    #[test]
    fn media_test_3() {
        let s = r#"#あああ
いいい
https://pbs.twimg.com/profile_banners/12/1688241283/1500x500
う
https://pbs.twimg.com/profile_banners/12/1688241283/1500x500
https://i.gyazo.com/09026f13790f738e9cd379354eefe6db.jpg
"#;
        RT.block_on(async {
            let (media, content, q) = media(get_state().await, s, &mut FxHashMap::default()).await;
            assert!(q.is_none());
            assert_eq!(
                content.misskey,
                "#あああ
いいい
https://pbs.twimg.com/profile_banners/12/1688241283/1500x500
う\n"
            );
            assert_eq!(media.len(), 3);
        })
    }

    #[test]
    fn media_test_4() {
        let s = r#"

        https://pbs.twimg.com/profile_banners/12/1688241283/1500x500

        "#;
        RT.block_on(async {
            let (media, content, q) = media(get_state().await, s, &mut FxHashMap::default()).await;
            assert!(q.is_none());
            assert_eq!(content.html, "");
            assert_eq!(media.len(), 1);
        })
    }

    #[test]
    fn media_test_5() {
        let event = "nevent1qqs2036kav4rsz3javzuf349vzmtwdnq47xqz3yj2jtu97lckzsgkvqzyp78yctwuy65cpyujmqnun68qw0qxe560p5c7khpszf0h5emtlghsqgewaehxw309aex2mrp0yhx6mmddaehgu3wwp5ku6e0gtcgdg";
        let s = format!(r#": nostr:{event}"#);
        RT.block_on(async {
            let state = get_state().await;
            let (media, content, q) = media(state, &s, &mut FxHashMap::default()).await;
            assert_eq!(content.html, format!("<span>: </span><br><span><br>RE: </span><a href=\"https://coracle.social/{event}\">https://coracle.social/{event}</a>"));
            assert_eq!(media.len(), 0);
            assert_eq!(
                q.unwrap().ap_id.strip_prefix(NOTE_ID_PREFIX).unwrap(),
                Nip19Event::from_bech32(event)
                    .unwrap()
                    .event_id
                    .to_bech32()
                    .unwrap()
            )
        })
    }

    #[test]
    fn media_test_6() {
        let event = "nevent1qqs2036kav4rsz3javzuf349vzmtwdnq47xqz3yj2jtu97lckzsgkvqzyp78yctwuy65cpyujmqnun68qw0qxe560p5c7khpszf0h5emtlghsqgewaehxw309aex2mrp0yhx6mmddaehgu3wwp5ku6e0gtcgdg";
        let s = format!(": \nnostr:{event}");
        RT.block_on(async {
            let (media, content, q) = media(get_state().await, &s, &mut FxHashMap::default()).await;
            assert_eq!(content.misskey, ": \n");
            assert_eq!(media.len(), 0);
            assert_eq!(
                q.unwrap().ap_id.strip_prefix(NOTE_ID_PREFIX).unwrap(),
                Nip19Event::from_bech32(event)
                    .unwrap()
                    .event_id
                    .to_bech32()
                    .unwrap()
            )
        })
    }

    #[test]
    fn media_test_7() {
        let event = "nevent1qqs2036kav4rsz3javzuf349vzmtwdnq47xqz3yj2jtu97lckzsgkvqzyp78yctwuy65cpyujmqnun68qw0qxe560p5c7khpszf0h5emtlghsqgewaehxw309aex2mrp0yhx6mmddaehgu3wwp5ku6e0gtcgdg";
        let s = format!(": \nnostr:{event}\nnostr:{event}");
        RT.block_on(async {
            let (media, content, q) = media(get_state().await, &s, &mut FxHashMap::default()).await;
            assert_eq!(
                content.misskey,
                format!(": \nhttps://coracle.social/{event}")
            );
            assert_eq!(media.len(), 0);
            assert_eq!(
                q.unwrap().ap_id.strip_prefix(NOTE_ID_PREFIX).unwrap(),
                Nip19Event::from_bech32(event)
                    .unwrap()
                    .event_id
                    .to_bech32()
                    .unwrap()
            )
        })
    }

    #[test]
    fn media_test_8() {
        let s = "aa, https://example.com/#/aaaaa";
        RT.block_on(async {
            let (media, content, q) = media(get_state().await, s, &mut FxHashMap::default()).await;
            assert_eq!(content.html, "<span>aa, </span><a href=\"https://example.com/#/aaaaa\">https://example.com/#/aaaaa</a>");
            assert!(media.is_empty());
            assert!(q.is_none());
        })
    }

    #[test]
    fn media_test_9() {
        let event = "nevent1qqsw3p5kfjs3gs78wnqgv6t2xzz37c9sdk3evw6vfnz9a8xdcjzturqzyql76e79w7mv28zmrgvmccr74lnta63a8h9fmeewjua0lzqmjrcfsqgewaehxw309aex2mrp0yhx6mmddaehgu3wwp5ku6e0vgencg";
        RT.block_on(async {
            let s = format!(": \nnostr:{event}");
            let (_, content, _) = media(get_state().await, &s, &mut FxHashMap::default()).await;
            assert_eq!(content.html, "<span>: <br></span><span><br>RE: </span><a href=\"https://misskey.io/notes/9swud4noy1r50c79\">https://misskey.io/notes/9swud4noy1r50c79</a>");
        })
    }

    #[test]
    fn media_test_10() {
        let s = "test🍆\nnostr:nevent1qvzqqqqqqypzqqlr9c8my0tp8z4r83wqs4gga3pec99579l2nu5hwf5tjr0zvk42qyvhwumn8ghj7un9d3shjtnddakk7um5wgh8q6twdvhsz9mhwden5te0wfjkccte9ec8y6tdv9kzumn9wshsqgx0jrrkyheuqneh6xstum5dyt86eea2k9s2ct8e0l2s07lz0vvrcvapatx2";
        RT.block_on(async {
            let (_, content, _) = media(get_state().await, s, &mut FxHashMap::default()).await;
            assert_eq!(content.html, "<span>test🍆<br></span><span><br>RE: </span><a href=\"https://mastodon.social/@pixelfed/112342975213580101\">https://mastodon.social/@pixelfed/112342975213580101</a>");
        })
    }
}
