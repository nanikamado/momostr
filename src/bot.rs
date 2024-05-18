use crate::nostr_to_ap::update_follow_list;
use crate::server::AppState;
use crate::{BOT_SEC, NPUB_REG};
use nostr_lib::event::TagStandard;
use nostr_lib::nips::nip10::Marker;
use nostr_lib::types::Filter;
use nostr_lib::{Event, EventBuilder, Keys, Kind, Tag};
use std::sync::Arc;
use std::time::Duration;

pub async fn handle_message_to_bot(state: &Arc<AppState>, event: Arc<Event>) {
    let command = NPUB_REG.replace_all(&event.content, "");
    let command = command.trim().to_lowercase();
    let l = state
        .activitypub_accounts
        .lock()
        .get(event.author_ref())
        .cloned();
    let command = if let Some(id) = l {
        let stopped = state.db.is_stopped_ap(&id);
        if command == "stop my mirror" {
            if stopped {
                "We have already stopped your mirror.".to_string()
            } else {
                state.db.stop_ap(id.to_string());
                "Stopped. \
                    Send `restart my mirror` to this bot \
                    if you want to restart your mirror account on the Fediverse."
                    .to_string()
            }
        } else if command == "restart my mirror" {
            if stopped {
                state.db.restart_ap(&id);
                "Restarted.".to_string()
            } else {
                "Your mirror is not stopped. Your mirror is already working.".to_string()
            }
        } else {
            format!("Command `{command}` is not supported.")
        }
    } else {
        let npub = event.author_ref();
        let stopped = state.db.is_stopped_npub(npub);
        if command == "stop my mirror" {
            if stopped {
                "We have already stopped your mirror.".to_string()
            } else {
                state.db.stop_npub(npub);
                "Stopped. \
                    Send `restart my mirror` to this bot \
                    if you want to restart your mirror account on the Fediverse."
                    .to_string()
            }
        } else if command == "restart my mirror" {
            if stopped {
                state.db.restart_npub(npub);
                if let Some(e) = state
                    .get_nostr_event_with_timeout(
                        Filter {
                            authors: Some([*npub].into_iter().collect()),
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
                    .nostr_account_to_followers
                    .lock()
                    .get(npub)
                    .map_or(false, |a| !a.is_empty())
                {
                    "Restarted.".to_string()
                } else {
                    "Restarted but, \
                    but we are currently not mirroring your account because \
                    you don't have any followers on the Fediverse. \
                    We will start mirroring your account once someone on the Fediverse follows you."
                        .to_string()
                }
            } else if state
                .nostr_account_to_followers
                .lock()
                .get(npub)
                .map_or(false, |a| !a.is_empty())
            {
                "Your mirror is not stopped. Your mirror is already working.".to_string()
            } else {
                "Your mirror is not stopped, \
                    but we are currently not mirroring your account because \
                    you don't have any followers on the Fediverse. \
                    We will start mirroring your account once someone on the Fediverse follows you."
                    .to_string()
            }
        } else {
            format!("Command `{command}` is not supported.")
        }
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
    let e = EventBuilder::text_note(command, tags)
        .to_event(&Keys::new(BOT_SEC.clone()))
        .unwrap();
    state.nostr_send(Arc::new(e)).await;
}
