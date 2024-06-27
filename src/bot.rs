use crate::nostr_to_ap::update_follow_list;
use crate::server::AppState;
use crate::{ADMIN_PUB, BOT_PUB, BOT_SEC, NPUB_REG};
use nostr_lib::event::TagStandard;
use nostr_lib::key::PublicKey;
use nostr_lib::nips::nip04;
use nostr_lib::nips::nip10::Marker;
use nostr_lib::nips::nip19::FromBech32;
use nostr_lib::types::{Filter, Timestamp};
use nostr_lib::{Event, EventBuilder, Keys, Kind, Tag};
use once_cell::sync::Lazy;
use regex::Regex;
use std::sync::Arc;
use std::time::Duration;

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
    let command = NPUB_REG.replace_all(&event.content, "");
    let command = command.trim().to_lowercase();
    let l = state.db.get_ap_id_of_npub(event.author_ref());
    let response = if let Some(id) = l {
        handle_command_from_fediverse_account(state, &id, &command).await
    } else {
        let npub = event.author_ref();
        handle_command_from_nostr_account(state, npub, &command).await
    };

    let mut tags = vec![Tag::public_key(event.author())];
    let mut root = None;
    for t in &event.tags {
        if let Some(TagStandard::Event {
            event_id,
            relay_url: _,
            marker: Some(Marker::Root),
        }) = t.as_standardized()
        {
            root = Some(event_id);
        }
    }
    if let Some(e) = root {
        tags.push(
            TagStandard::Event {
                event_id: *e,
                relay_url: None,
                marker: Some(Marker::Root),
            }
            .into(),
        );
        tags.push(
            TagStandard::Event {
                event_id: event.id,
                relay_url: None,
                marker: Some(Marker::Reply),
            }
            .into(),
        );
    } else {
        tags.push(
            TagStandard::Event {
                event_id: event.id,
                relay_url: None,
                marker: Some(Marker::Root),
            }
            .into(),
        );
    }
    let e = EventBuilder::text_note(response, tags)
        .to_event(&Keys::new(BOT_SEC.clone()))
        .unwrap();
    state.nostr_send(Arc::new(e)).await;
}

pub async fn handle_dm_message_to_bot(state: &Arc<AppState>, event: Arc<Event>) {
    static MENTION_REG: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^(?:nostr:)?(npub1[0-9a-z]{50,}|nprofile1[0-9a-z]{50,})\s*").unwrap()
    });
    let author = event.author_ref();
    let Ok(command) = nip04::decrypt(&BOT_SEC, author, event.content.clone()) else {
        return;
    };
    let command = command.trim().to_lowercase();
    let command = MENTION_REG.replace(&command, "");
    let response = if author == &*BOT_PUB || Some(author) == ADMIN_PUB.as_ref() {
        if command.starts_with("response:") {
            return;
        }
        format!(
            "response: {}",
            handle_command_from_admin(state, &command).await
        )
    } else {
        handle_command_from_nostr_account(state, author, &command).await
    };
    let bot_keys = Keys::new(BOT_SEC.clone());
    let e = EventBuilder::encrypted_direct_msg(&bot_keys, *author, response, Some(event.id))
        .unwrap()
        .custom_created_at(Timestamp::now().clamp(event.created_at + 1, event.created_at + 600))
        .to_event(&bot_keys)
        .unwrap();
    state.nostr_send(Arc::new(e)).await;
}
