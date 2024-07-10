use crate::error::Error;
use crate::server::AppState;
use crate::RelayId;
use cached::Cached;
use futures_util::StreamExt;
use nostr_lib::event::{Event, TagStandard};
use nostr_lib::types::Filter;
use nostr_lib::{EventId, JsonUtil, Kind, Metadata, PublicKey};
use relay_pool::EventWithRelayId;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio::time::{timeout_at, Instant};
use tracing::debug;

// allowed because `Proxied` is rare
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum NostrUser {
    // TODO: distinguish user proxied by other bridge to prevent duplication
    // of replies
    Proxied(String),
    Metadata(Metadata),
}

#[tracing::instrument(skip_all)]
pub async fn get_nostr_user_data(
    state: &Arc<AppState>,
    public_key: PublicKey,
) -> Arc<Result<NostrUser, Error>> {
    let c = {
        let mut m = state.nostr_user_cache.lock();
        m.cache_get_or_set_with(public_key, || Arc::new(OnceCell::new()))
            .clone()
    };
    c.get_or_init(|| async { Arc::new(get_nostr_user_data_without_cache(state, public_key).await) })
        .await
        .clone()
}

pub async fn get_nostr_user_data_without_cache(
    state: &Arc<AppState>,
    public_key: PublicKey,
) -> Result<NostrUser, Error> {
    debug!("public_key = {}", public_key);
    let f = Filter {
        kinds: Some([Kind::Metadata].into_iter().collect()),
        authors: Some([public_key].into_iter().collect()),
        limit: Some(1),
        ..Default::default()
    };
    let mut sub = state.subscribe_filter(vec![f]).await;
    let mut e = tokio::time::timeout(Duration::from_secs(10), sub.next())
        .await
        .ok()
        .flatten()
        .ok_or(Error::NotFound)?;
    let n = if let Some(id) = e.event.tags.iter().find_map(|tag| {
        if let Some(TagStandard::Proxy {
            id,
            protocol: nostr_lib::nips::nip48::Protocol::ActivityPub,
        }) = tag.as_standardized()
        {
            Some(id.as_str())
        } else if let nostr_lib::event::TagKind::Custom(t) = tag.kind() {
            if t == "mostr" {
                tag.content()
            } else {
                None
            }
        } else {
            None
        }
    }) {
        NostrUser::Proxied(id.to_string())
    } else {
        let metadata = Metadata::from_json(&e.event.content).map_err(|_| Error::NotFound)?;
        let state = state.clone();
        tokio::spawn(async move {
            let mut outdated_metadata = Vec::new();
            let dead_line = Instant::now() + Duration::from_secs(10);
            while let Ok(Some(new_e)) = timeout_at(dead_line, sub.next()).await {
                match new_e.event.created_at.cmp(&e.event.created_at) {
                    std::cmp::Ordering::Less => {
                        outdated_metadata.push(new_e);
                    }
                    std::cmp::Ordering::Greater => {
                        outdated_metadata.push(e);
                        e = new_e;
                    }
                    std::cmp::Ordering::Equal => (),
                }
            }
            if !outdated_metadata.is_empty() {
                if let Ok(metadata) = Metadata::from_json(&e.event.content) {
                    state
                        .nostr
                        .send(
                            e.event,
                            Arc::new(outdated_metadata.into_iter().map(|m| m.relay_id).collect()),
                        )
                        .await;
                    state.nostr_user_cache.lock().cache_set(
                        public_key,
                        Arc::new(OnceCell::const_new_with(Arc::new(Ok(NostrUser::Metadata(
                            metadata,
                        ))))),
                    );
                }
            }
        });
        NostrUser::Metadata(metadata)
    };
    Ok(n)
}

impl AppState {
    #[tracing::instrument(skip_all)]
    pub async fn get_note(&self, note_id: EventId) -> Option<EventWithRelayId<RelayId>> {
        let c = {
            let mut m = self.note_cache.lock();
            m.get_or_insert(note_id, || Arc::new(OnceCell::new()))
                .clone()
        };
        c.get_or_init(|| async {
            let f = Filter {
                ids: Some([note_id].into_iter().collect()),
                kinds: Some([Kind::TextNote].into_iter().collect()),
                ..Default::default()
            };
            self.get_nostr_event_with_timeout(f, Duration::from_secs(10))
                .await
        })
        .await
        .clone()
    }

    #[tracing::instrument(skip_all)]
    pub async fn subscribe_filter(&self, filters: Vec<Filter>) -> relay_pool::EventStream<RelayId> {
        debug!("filter = {}", serde_json::to_string(&filters).unwrap());
        let w = self.nostr_subscribe_rate.lock().wait();
        w.await;
        self.nostr
            .subscribe(filters, self.inbox_relays.clone())
            .await
    }

    pub async fn nostr_send(&self, event: Arc<Event>) {
        let s = self.nostr_send_rate.lock().wait();
        s.await;
        self.nostr.send(event, self.outbox_relays.clone()).await
    }

    #[tracing::instrument(skip_all)]
    pub async fn get_nostr_event_with_timeout(
        &self,
        f: Filter,
        timeout: Duration,
    ) -> Option<EventWithRelayId<RelayId>> {
        tokio::time::timeout(timeout, self.subscribe_filter(vec![f]).await.next())
            .await
            .ok()
            .flatten()
    }
}
