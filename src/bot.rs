use crate::nostr_to_ap::update_follow_list;
use crate::server::AppState;
use crate::{RelayId, ADMIN_PUB, BOT_KEYPAIR, BOT_PUB, BOT_SEC, NPUB_REG, USER_ID_PREFIX};
use cached::Cached;
use nostr_lib::event::TagStandard;
use nostr_lib::key::PublicKey;
use nostr_lib::nips::nip10::Marker;
use nostr_lib::nips::nip19::FromBech32;
use nostr_lib::nips::nip59::UnwrappedGift;
use nostr_lib::types::Filter;
use nostr_lib::{Event, EventBuilder, Keys, Kind, Tag};
use regex::Regex;
use rustc_hash::FxHashSet;
use std::str::FromStr;
use std::sync::{Arc, LazyLock as Lazy};
use std::time::Duration;
use tracing::info;
use url::Url;

async fn handle_command_from_nostr_account(
    state: &Arc<AppState>,
    author: &PublicKey,
    command: &str,
) -> String {
    let stopped = state.db.is_stopped_npub(author);
    if command == "stop" {
        if stopped {
            "We have already stopped bridging your account.".to_string()
        } else {
            state.db.stop_npub(author);
            "Stopped. \
                Send `restart` to this bot \
                if you want us to restart bridging your account."
                .to_string()
        }
    } else if command == "restart" {
        if stopped {
            state.db.restart_npub(author);
            if let Some(e) = state
                .get_nostr_event_with_timeout(
                    Filter {
                        authors: Some([*author].into_iter().collect()),
                        kinds: Some([Kind::ContactList].into_iter().collect()),
                        ..Default::default()
                    },
                    Duration::from_secs(10),
                )
                .await
            {
                update_follow_list(state, e.event).await;
            }
            if state
                .db
                .get_followers_of_nostr(author)
                .map_or(false, |a| !a.is_empty())
            {
                "Restarted.".to_string()
            } else {
                "Restarted but, \
                we are currently not bridging your account because \
                you don't have any followers on the Fediverse. \
                We will start bridging your account once someone on the Fediverse follows you."
                    .to_string()
            }
        } else if state
            .db
            .get_followers_of_nostr(author)
            .map_or(false, |a| !a.is_empty())
        {
            "We are already bridging your account.".to_string()
        } else {
            "We are currently not bridging your account because \
            you don't have any followers on the Fediverse. \
            We will start bridging your account once someone on the Fediverse follows you."
                .to_string()
        }
    } else {
        format!("Command `{command}` is not supported.")
    }
}

async fn handle_command_from_admin(state: &Arc<AppState>, command: &str) -> String {
    if let Some(npub) = command.strip_prefix("stop ") {
        if let Ok(npub) = PublicKey::from_bech32(npub) {
            let stopped = state.db.is_stopped_npub(&npub);
            if stopped {
                "already stopped".to_string()
            } else {
                state.db.stop_npub(&npub);
                "stopped".to_string()
            }
        } else {
            "error".to_string()
        }
    } else if let Some(npub) = command.strip_prefix("restart ") {
        if let Ok(npub) = PublicKey::from_bech32(npub) {
            let stopped = state.db.is_stopped_npub(&npub);
            if stopped {
                state.db.restart_npub(&npub);
                if let Some(e) = state
                    .get_nostr_event_with_timeout(
                        Filter {
                            authors: Some([npub].into_iter().collect()),
                            kinds: Some([Kind::ContactList].into_iter().collect()),
                            ..Default::default()
                        },
                        Duration::from_secs(10),
                    )
                    .await
                {
                    update_follow_list(state, e.event).await;
                }
                "restarted".to_string()
            } else {
                "already restarted".to_string()
            }
        } else {
            "error".to_string()
        }
    } else if let Some(s) = command.strip_prefix("activity ") {
        async fn send_activity(state: &Arc<AppState>, command: &str) -> Option<String> {
            let r = Regex::new(r"(npub1[0-9a-z]{50,}) (https\S+) (.*)").unwrap();
            let cs = r.captures(command)?;
            let npub = cs.get(1)?.as_str();
            let inbox = url::Url::from_str(cs.get(2)?.as_str()).ok()?;
            let activity = cs.get(3)?.as_str();
            let author = format!("{USER_ID_PREFIX}{npub}");

            if let Err(e) = state
                .send_string_activity(&inbox, author, activity.to_string())
                .await
            {
                Some(format!("could not send activity: {e:?}"))
            } else {
                Some("ok".to_string())
            }
        }
        send_activity(state, s)
            .await
            .unwrap_or_else(|| "error".to_string())
    } else {
        format!("Command `{command}` is not supported.")
    }
}

async fn handle_command_from_fediverse_account(
    state: &Arc<AppState>,
    id: &str,
    command: &str,
) -> String {
    let stopped = state.db.is_stopped_ap(id);
    if command.contains("stop") {
        if stopped {
            "We have already stopped bridging you.".to_string()
        } else {
            state.db.stop_ap(id.to_string());
            "Stopped. \
                Send `restart` to this bot \
                if you want us to restart bridging your account."
                .to_string()
        }
    } else if command.contains("restart") {
        if stopped {
            state.db.restart_ap(id);
            "Restarted.".to_string()
        } else {
            "We are already bridging your account.".to_string()
        }
    } else {
        format!("Command `{command}` is not supported.")
    }
}

pub async fn handle_message_to_bot(state: &Arc<AppState>, event: Arc<Event>) {
    if state
        .handled_commands
        .lock()
        .cache_set(event.id, ())
        .is_some()
    {
        info!("this command is already handled");
        return;
    }
    let command = NPUB_REG.replace_all(&event.content, "");
    let command = command.trim().to_lowercase();
    let l = state.db.get_ap_id_of_npub(&event.author());
    let response = if let Some(id) = l {
        handle_command_from_fediverse_account(state, &id, &command).await
    } else {
        let npub = &event.author();
        handle_command_from_nostr_account(state, npub, &command).await
    };

    let mut tags = vec![Tag::public_key(event.author())];
    let mut root = None;
    for t in &event.tags {
        if let Some(TagStandard::Event {
            event_id,
            relay_url: _,
            marker: Some(Marker::Root),
            public_key,
        }) = t.as_standardized()
        {
            root = Some((event_id, *public_key));
        }
    }
    if let Some((e, public_key)) = root {
        tags.push(
            TagStandard::Event {
                event_id: *e,
                relay_url: None,
                marker: Some(Marker::Root),
                public_key,
            }
            .into(),
        );
        tags.push(
            TagStandard::Event {
                event_id: event.id,
                relay_url: None,
                marker: Some(Marker::Reply),
                public_key: Some(event.author()),
            }
            .into(),
        );
    } else {
        tags.push(
            TagStandard::Event {
                event_id: event.id,
                relay_url: None,
                marker: Some(Marker::Root),
                public_key: Some(event.pubkey),
            }
            .into(),
        );
    }
    let keys = Keys::new(BOT_SEC.clone());
    let e = EventBuilder::text_note(response, tags)
        .custom_created_at(event.created_at)
        .to_event(&keys)
        .unwrap();
    state.nostr_send(Arc::new(e)).await;
}

#[tracing::instrument(skip_all)]
pub async fn handle_dm_message_to_bot(state: &Arc<AppState>, event: Arc<Event>) {
    if state
        .handled_commands
        .lock()
        .cache_set(event.id, ())
        .is_some()
    {
        info!("this command is already handled");
        return;
    }
    static MENTION_REG: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^(?:nostr:)?(npub1[0-9a-z]{50,}|nprofile1[0-9a-z]{50,})\s*").unwrap()
    });
    let bot_keys = Keys::new(BOT_SEC.clone());
    let Ok(r) = UnwrappedGift::from_gift_wrap(&bot_keys, &event) else {
        return;
    };
    let dm = r.rumor;
    let author = dm.pubkey;
    info!("dm = {dm:?}");
    if author == *BOT_PUB {
        return;
    }
    let command = &dm.content.trim();
    info!("command: {command}");
    let command = MENTION_REG.replace(command, "");
    let response = if Some(&author) == ADMIN_PUB.as_ref() {
        handle_command_from_admin(state, &command).await
    } else {
        handle_command_from_nostr_account(state, &author, &command).await
    };
    let e = state
        .get_nostr_event_with_timeout_from_relays(
            Filter::new().author(author).kind(Kind::from(10050)),
            Duration::from_secs(10),
            (*state.metadata_relays).clone(),
        )
        .await;
    info!("e = {e:?}");
    let inbox_relay_urls: Vec<_> = e
        .map(|a| {
            a.event
                .tags
                .iter()
                .filter_map(|a| {
                    if let Some(TagStandard::Relay(r)) = a.as_standardized() {
                        Url::parse(&r.to_string()).ok()
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    info!("inbox_relay_urls = {inbox_relay_urls:?}");
    let mut inbox_relays = FxHashSet::default();
    for l in inbox_relay_urls {
        inbox_relays.insert(relay_to_id(state, l).await);
    }
    let rumor = EventBuilder::private_msg_rumor(author, response, None)
        .custom_created_at(dm.created_at + 1)
        .to_unsigned_event(bot_keys.public_key());
    let e = Arc::new(EventBuilder::gift_wrap(&bot_keys, &author, rumor.clone(), None).unwrap());
    state.nostr.send(e.clone(), None, inbox_relays.into()).await;
    state.nostr_send(e.clone()).await;
    let e = EventBuilder::gift_wrap(&bot_keys, &bot_keys.public_key(), rumor, None).unwrap();
    state.nostr_send(Arc::new(e)).await;
    info!("sent");
}

async fn relay_to_id(state: &Arc<AppState>, url: Url) -> RelayId {
    let id = {
        let mut relay_to_id_map = state.relay_to_id_map.lock();
        if let Some(id) = relay_to_id_map.get(&url) {
            return *id;
        }
        let id = RelayId(relay_to_id_map.len() as u32);
        relay_to_id_map.insert(url.clone(), id);
        id
    };
    state
        .nostr
        .add_relay(id, url, Some(BOT_KEYPAIR.clone()))
        .await
        .unwrap();
    id
}
