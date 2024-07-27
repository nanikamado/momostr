use crate::error::Error;
use crate::{REVERSE_DNS, USER_AGENT};
use axum::http::HeaderValue;
use futures_util::{SinkExt, StreamExt};
use itertools::Itertools;
use nostr_lib::event::TagStandard;
use nostr_lib::{Event, EventBuilder, EventId, Keys, SecretKey};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Serialize, Serializer};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};

#[derive(Debug)]
pub struct EventDeletionQueue(tokio::sync::mpsc::Sender<(EventId, SecretKey)>);

impl EventDeletionQueue {
    pub fn new(http_client: Arc<reqwest::Client>) -> Self {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1_000);
        tokio::spawn(async move {
            loop {
                let mut buff = Vec::with_capacity(10);
                let n = receiver.recv_many(&mut buff, 100).await;
                if n == 0 {
                    break;
                }
                let ids = buff
                    .iter()
                    .format_with(", ", |(e, _), f| f(&format_args!("{e}")))
                    .to_string();
                debug!("start deletion of {ids}");
                if let Err(e) = delete_async(buff, &http_client, &ids).await {
                    error!("{e:?}");
                }
                debug!("deleted {ids}");
            }
        });
        Self(sender)
    }

    pub fn delete(&self, event_id: EventId, nsec: SecretKey) {
        if let Err(e) = self.0.try_send((event_id, nsec)) {
            error!("{e}")
        }
    }
}

#[tracing::instrument(skip_all)]
async fn delete_async(
    es: Vec<(EventId, SecretKey)>,
    http_client: &reqwest::Client,
    ids_for_log: &str,
) -> Result<(), Error> {
    let relays: Vec<url::Url> = http_client
        .get("https://api.nostr.watch/v1/nip/1")
        .send()
        .await?
        .json()
        .await?;
    let mut m: FxHashMap<_, (_, FxHashSet<_>)> = FxHashMap::default();
    for (event_id, nsec) in es {
        m.entry(*nsec.as_ref())
            .or_insert((nsec, FxHashSet::default()))
            .1
            .insert(event_id);
    }
    let es = m
        .into_iter()
        .map(|(_, (nsec, ids))| {
            serde_json::to_string(&ClientMessage(
                EventBuilder::delete(ids)
                    .add_tags([TagStandard::LabelNamespace(REVERSE_DNS.to_string()).into()])
                    .to_event(&Keys::new(nsec))
                    .unwrap(),
            ))
            .unwrap()
        })
        .collect_vec();
    let relays_len = relays.len();
    for (i, r) in relays.into_iter().enumerate() {
        let mut req = r.as_str().into_client_request()?;
        let headers = req.headers_mut();
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_str(USER_AGENT.as_str()).unwrap(),
        );
        let f = async {
            match connect_async(req).await {
                Ok((ws, _)) => {
                    let (mut sender, mut receiver) = ws.split();
                    let r_cloned = r.clone();
                    tokio::spawn(async move {
                        while let Some(msg) = receiver.next().await {
                            match msg {
                                Ok(m) => match m {
                                    Message::Text(m) => {
                                        debug!("{r_cloned} ==> {m}")
                                    }
                                    Message::Binary(_) => {
                                        debug!("{r_cloned} ==> <binary>")
                                    }
                                    Message::Close(None) | Message::Ping(_) | Message::Pong(_) => {}
                                    Message::Close(Some(frame)) => {
                                        if !frame.reason.is_empty() {
                                            debug!("{r_cloned} ==> close: {}", frame.reason)
                                        }
                                    }
                                    Message::Frame(frame) => {
                                        debug!("{r_cloned} ==> frame: {:?}", frame.to_text())
                                    }
                                },
                                Err(e) => info!("{r_cloned} ==> {e}"),
                            }
                        }
                    });
                    debug!("[{:>3}/{relays_len}] {r}: {ids_for_log}", i + 1);
                    for e in es.iter() {
                        if let Err(e) = sender.feed(Message::Text(e.clone())).await {
                            info!("{r} ==> {e}")
                        }
                    }
                    if let Err(e) = sender.close().await {
                        info!("{r} ==> {e}")
                    }
                }
                Err(e) => {
                    info!("{r} ==> {e}")
                }
            }
        };
        if tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .is_err()
        {
            info!("{r}: timeout")
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientMessage(Event);

impl Serialize for ClientMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(2))?;
        seq.serialize_element("EVENT")?;
        seq.serialize_element(&self.0)?;
        seq.end()
    }
}
