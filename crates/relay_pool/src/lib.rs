use futures_util::stream::FuturesUnordered;
use futures_util::{SinkExt, Stream};
use id_pool::IdPool;
use itertools::Itertools;
use lru::LruCache;
use nostr::types::Filter;
use nostr::{Event, JsonUtil, RelayMessage};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Serialize, Serializer};
use std::collections::HashMap;
use std::fmt::Display;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::atomic::{self, AtomicU32};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::sync::mpsc::Sender;
use tokio::time::error::Elapsed;
use tokio_stream::StreamExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::USER_AGENT;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, trace, warn};

enum FilterOp<RelayId> {
    Subscribe(
        u32,
        Arc<Vec<Filter>>,
        Sender<EventWithRelayId<RelayId>>,
        Arc<FxHashSet<RelayId>>,
    ),
    Unsubscribe(u32),
    ChangeFilter(u32, Vec<Filter>, Arc<FxHashSet<RelayId>>),
}

#[derive(Debug, Clone)]
struct SendEvent<RelayId> {
    event: Arc<nostr::Event>,
    keys: Option<Arc<nostr::Keys>>,
    relays: Arc<FxHashSet<RelayId>>,
}

#[derive(Debug)]
pub struct RelayPool<RelayId> {
    tx_for_filter_ops: Sender<FilterOp<RelayId>>,
    tx_for_send_event: Sender<SendEvent<RelayId>>,
    tx_for_add_relay: Sender<(RelayId, url::Url, Option<nostr::Keys>)>,
    counter: AtomicU32,
}

struct SenderWithId<RelayId> {
    sender: Sender<RelayMessageWithId<RelayId>>,
    id: RelayId,
}

struct RelayMessageWithId<RelayId> {
    relay_message: RelayMessage,
    id: RelayId,
}

impl<RelayId: Copy> SenderWithId<RelayId> {
    async fn send(&self, message: RelayMessage) {
        self.sender
            .send(RelayMessageWithId {
                relay_message: message,
                id: self.id,
            })
            .await
            .unwrap()
    }
}

#[derive(Debug, Clone)]
struct RelayOp<RelayId> {
    msg: ClientMessage,
    relays: Arc<FxHashSet<RelayId>>,
}

struct ReceiverWithId<RelayId> {
    receiver: broadcast::Receiver<RelayOp<RelayId>>,
    id: RelayId,
}

impl<Id: Clone + Eq + Hash> ReceiverWithId<Id> {
    async fn recv(&mut self) -> Result<ClientMessage, broadcast::error::RecvError> {
        loop {
            match self.receiver.recv().await {
                Ok(op) => {
                    if op.relays.contains(&self.id) {
                        return Ok(op.msg);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl<RelayId: Copy + Send + Sync + Eq + Hash + 'static> RelayPool<RelayId> {
    pub async fn new(user_agent: String) -> Self {
        let user_agent = Arc::new(user_agent);
        let broadcast_sender = tokio::sync::broadcast::Sender::new(1_000);
        let (tx_for_events, mut rx_for_events) = tokio::sync::mpsc::channel(10);
        let (tx_for_filter_ops, mut rx_for_filter_ops) = tokio::sync::mpsc::channel(10);
        let (tx_for_send_event, mut rx_for_send_event) = tokio::sync::mpsc::channel(10);
        let (tx_for_add_relay, mut rx_for_add_relay) = tokio::sync::mpsc::channel(10);
        let mut relay_pool = FuturesUnordered::new();
        let broadcast_sender_cloned = broadcast_sender.clone();
        let subscription_loop = async move {
            loop {
                if relay_pool.is_empty() {
                    if let Some((id, url, auth_master_key)) = rx_for_add_relay.recv().await {
                        relay_pool.push(subscribe_relay(
                            url,
                            ReceiverWithId {
                                receiver: broadcast_sender_cloned.subscribe(),
                                id,
                            },
                            SenderWithId {
                                sender: tx_for_events.clone(),
                                id,
                            },
                            user_agent.clone(),
                            auth_master_key,
                        ));
                    } else {
                        break;
                    }
                }
                tokio::select! {
                    e = relay_pool.next() => {
                        tracing::error!("{:?}", e.unwrap());
                    }
                    Some((id, url, auth_master_key)) = rx_for_add_relay.recv() => {
                        relay_pool.push(subscribe_relay(
                            url,
                            ReceiverWithId {
                                receiver: broadcast_sender_cloned.subscribe(),
                                id,
                            },
                            SenderWithId {
                                sender: tx_for_events.clone(),
                                id,
                            },
                            user_agent.clone(),
                            auth_master_key,
                        ));
                    }
                    else => break,
                }
            }
        };
        let collect_events = async move {
            let mut subs = SubscriptionState::new(broadcast_sender);
            loop {
                tokio::select! {
                    Some(op) = rx_for_filter_ops.recv() => {
                         subs.handle_filter_op(op).await;
                    }
                    Some(op) = rx_for_send_event.recv() => {
                         subs.handle_send_event(op).await;
                    }
                    Some(e) = rx_for_events.recv() => subs.handle_event(e),
                    else => break,
                }
            }
        };
        tokio::spawn(async {
            tokio::select! {
                _ = subscription_loop => (),
                _ = collect_events => (),
            }
        });
        Self {
            tx_for_filter_ops,
            tx_for_send_event,
            tx_for_add_relay,
            counter: AtomicU32::new(0),
        }
    }

    pub async fn add_relay(
        &self,
        relay_id: RelayId,
        url: url::Url,
        auth_master_key: Option<nostr::Keys>,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<(RelayId, url::Url, Option<nostr::Keys>)>>
    {
        self.tx_for_add_relay
            .send((relay_id, url, auth_master_key))
            .await
    }

    pub async fn subscribe(
        &self,
        filters: Vec<Filter>,
        relays: Arc<FxHashSet<RelayId>>,
    ) -> EventStream<RelayId> {
        let (tx, rx) = tokio::sync::mpsc::channel(1_000);
        let id = self.counter.fetch_add(1, atomic::Ordering::Relaxed);
        let filters = Arc::new(filters);
        self.tx_for_filter_ops
            .send(FilterOp::Subscribe(id, filters, tx, relays))
            .await
            .unwrap();
        EventStream {
            stream: rx,
            tx_for_ops: self.tx_for_filter_ops.clone(),
            id,
            cache: LruCache::new(NonZeroUsize::new(100).unwrap()),
        }
    }

    pub async fn change_filter(
        &self,
        id: u32,
        filters: Vec<Filter>,
        relays: Arc<FxHashSet<RelayId>>,
    ) {
        self.tx_for_filter_ops
            .send(FilterOp::ChangeFilter(id, filters, relays))
            .await
            .unwrap();
    }

    pub async fn get_event_with_timeout(
        &self,
        f: Filter,
        timeout: Duration,
        relays: Arc<FxHashSet<RelayId>>,
    ) -> Option<EventWithRelayId<RelayId>> {
        tokio::time::timeout(timeout, self.subscribe(vec![f], relays).await.next())
            .await
            .ok()
            .flatten()
    }

    pub async fn send(
        &self,
        event: Arc<nostr::Event>,
        keys: Option<Arc<nostr::Keys>>,
        relays: Arc<FxHashSet<RelayId>>,
    ) {
        self.tx_for_send_event
            .send(SendEvent {
                event,
                keys,
                relays,
            })
            .await
            .unwrap();
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
struct FilterId(u32);

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub struct EventWithRelayId<RelayId> {
    pub event: Arc<nostr::Event>,
    pub relay_id: RelayId,
}

struct FilterAndSenders<RelayId> {
    filter: Arc<Vec<Filter>>,
    sender: Sender<EventWithRelayId<RelayId>>,
}

struct SubscriptionState<RelayId> {
    sub_id_to_filter_id: HashMap<u32, (FilterId, Arc<FxHashSet<RelayId>>)>,
    filter_id_to_senders: FxHashMap<FilterId, FilterAndSenders<RelayId>>,
    id_pool: IdPool,
    broadcast_sender: tokio::sync::broadcast::Sender<RelayOp<RelayId>>,
}

impl Display for FilterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"{}\"", self.0)
    }
}

impl FromStr for FilterId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(FilterId(u32::from_str(s)?))
    }
}

impl Serialize for FilterId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<RelayId: Copy> SubscriptionState<RelayId> {
    fn new(broadcast_sender: tokio::sync::broadcast::Sender<RelayOp<RelayId>>) -> Self {
        Self {
            sub_id_to_filter_id: Default::default(),
            filter_id_to_senders: Default::default(),
            id_pool: IdPool::new_ranged(0..u32::MAX),
            broadcast_sender,
        }
    }

    fn handle_event(&mut self, e: RelayMessageWithId<RelayId>) {
        if let RelayMessageWithId {
            relay_message:
                RelayMessage::Event {
                    subscription_id,
                    event,
                },
            id: relay_id,
        } = e
        {
            if event.verify().is_err() {
                return;
            }
            let event = Arc::new(*event);
            if let Ok(id) = subscription_id.to_string().parse() {
                if let Some(f) = self.filter_id_to_senders.get(&id) {
                    if f.filter.iter().any(|f| f.match_event(&event)) {
                        let _ = f.sender.try_send(EventWithRelayId {
                            event: event.clone(),
                            relay_id,
                        });
                    } else {
                        error!(
                            "received event {} did not match the filter {}.",
                            serde_json::to_string(&event).unwrap(),
                            serde_json::to_string(&f.filter).unwrap()
                        );
                    }
                } else {
                    debug!("subscription id {id} not found");
                }
            }
        }
    }

    // common code among subscribe and filter change
    fn sub(
        &mut self,
        id: u32,
        filters: Arc<Vec<Filter>>,
        tx: Sender<EventWithRelayId<RelayId>>,
        relays: Arc<FxHashSet<RelayId>>,
    ) {
        let filter_id = FilterId(self.id_pool.request_id().unwrap());
        let fs: Vec<_> = filters
            .iter()
            .filter(|f| {
                !f.ids.as_ref().map(|l| l.is_empty()).unwrap_or(false)
                    && !f.authors.as_ref().map(|l| l.is_empty()).unwrap_or(false)
                    && !f.kinds.as_ref().map(|l| l.is_empty()).unwrap_or(false)
                    && f.generic_tags.iter().all(|(_, l)| !l.is_empty())
            })
            .cloned()
            .collect();
        if !fs.is_empty() {
            debug!(
                "starting connection with id {filter_id}, filters = [{}]",
                fs.iter()
                    .format_with(", ", |a, f| f(&serde_json::to_string(a).unwrap()))
            );
            broadcast(
                &self.broadcast_sender,
                RelayOp {
                    msg: ClientMessage::Req {
                        subscription_id: filter_id,
                        filters: fs,
                    },
                    relays: relays.clone(),
                },
            )
        }
        self.sub_id_to_filter_id.insert(id, (filter_id, relays));
        self.filter_id_to_senders.insert(
            filter_id,
            FilterAndSenders {
                filter: filters,
                sender: tx,
            },
        );
    }

    // common code among unsubscribe and filter change
    fn unsub(&mut self, id: u32) -> Sender<EventWithRelayId<RelayId>> {
        let (filter_id, relays) = self.sub_id_to_filter_id.remove(&id).unwrap();
        let f = self.filter_id_to_senders.remove(&filter_id).unwrap();
        self.id_pool.return_id(filter_id.0).unwrap();
        broadcast(
            &self.broadcast_sender,
            RelayOp {
                msg: ClientMessage::Close(filter_id),
                relays,
            },
        );
        debug!(
            "closed the connection of id {filter_id}: {}",
            serde_json::to_string(&f.filter).unwrap()
        );
        f.sender
    }

    async fn handle_filter_op(&mut self, op: FilterOp<RelayId>) {
        match op {
            FilterOp::Subscribe(id, filters, tx, relays) => {
                self.sub(id, filters, tx, relays);
            }
            FilterOp::Unsubscribe(id) => {
                self.unsub(id);
            }
            FilterOp::ChangeFilter(id, filters, relays) => {
                let tx = self.unsub(id);
                self.sub(id, Arc::new(filters), tx, relays);
            }
        }
    }

    async fn handle_send_event(&mut self, op: SendEvent<RelayId>) {
        broadcast(
            &self.broadcast_sender,
            RelayOp {
                msg: ClientMessage::Event(op.event, op.keys),
                relays: op.relays,
            },
        );
    }
}

pub struct EventStream<RelayId> {
    stream: tokio::sync::mpsc::Receiver<EventWithRelayId<RelayId>>,
    tx_for_ops: Sender<FilterOp<RelayId>>,
    id: u32,
    cache: LruCache<Arc<Event>, ()>,
}

impl<RelayId> Stream for EventStream<RelayId> {
    type Item = EventWithRelayId<RelayId>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll::*;
        loop {
            match self.stream.poll_recv(cx) {
                Ready(Some(a)) => {
                    if check_event(&mut self, &a.event) {
                        break Ready(Some(a));
                    }
                }
                Ready(None) => break Ready(None),
                Pending => break Pending,
            }
        }
    }
}

fn check_event<RelayId>(
    event_stream: &mut EventStream<RelayId>,
    event: &Arc<nostr::Event>,
) -> bool {
    if event_stream.cache.contains(event) {
        false
    } else {
        event_stream.cache.put(event.clone(), ());
        true
    }
}

impl<RelayId> Drop for EventStream<RelayId> {
    fn drop(&mut self) {
        self.tx_for_ops
            .try_send(FilterOp::Unsubscribe(self.id))
            .unwrap();
    }
}

impl<RelayId> EventStream<RelayId> {
    pub fn id(&self) -> u32 {
        self.id
    }
}

fn broadcast<RelayId>(tx: &broadcast::Sender<RelayOp<RelayId>>, message: RelayOp<RelayId>) {
    if let Err(e) = tx.send(message) {
        error!("failed to connect to relays: {e}")
    }
}

#[tracing::instrument(skip_all)]
async fn first_request(
    message: &Option<ClientMessage>,
    cs: &mut ConnectionState,
    last_connection_time: &mut SystemTime,
    connection_delay: &mut Duration,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, tokio_tungstenite::tungstenite::Error> {
    let (mut ws, r) = loop {
        if SystemTime::now() < *last_connection_time + Duration::from_secs(60) {
            error!("{} is unstable. sleeping {connection_delay:?}", cs.url);
            tokio::time::sleep(*connection_delay).await;
            *connection_delay = (*connection_delay * 2).min(Duration::from_secs(24 * 60 * 60 * 4));
        } else {
            *connection_delay = Duration::from_secs(5);
        }
        let mut req = cs.url.as_str().into_client_request()?;
        let headers = req.headers_mut();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(cs.user_agent.as_str()).unwrap(),
        );
        let r = connect_async(req).await;
        *last_connection_time = SystemTime::now();
        match r {
            Ok((ws, r)) => break (ws, r),
            Err(e) => {
                error!("failed to connect to {}: {e}", cs.url);
            }
        }
    };
    debug!("connected to {}: r = {r:?}, sub = {:?}", cs.url, cs.subs);
    for (id, filters) in cs.subs.iter() {
        let m = req_as_json(*id, filters);
        debug!("{} <== {m}", cs.url);
        ws.send(Message::Text(m)).await?;
    }
    if let Some(m) = message {
        send_event(m, cs, &mut ws).await?;
    }
    Ok(ws)
}

const TIMEOUT_DURATION: Duration = Duration::from_secs(60 * 3);

struct UnhandledMessage(Arc<Event>, Option<Arc<nostr::Keys>>);

struct ConnectionState {
    url: url::Url,
    user_agent: Arc<String>,
    auth_master_key: Option<nostr::Keys>,
    subs: HashMap<FilterId, Vec<Filter>>,
    auth: Option<String>,
}

async fn subscribe_relay<RelayId: Clone + Copy + Eq + Hash>(
    url: url::Url,
    mut rx_for_ops: ReceiverWithId<RelayId>,
    tx_for_events: SenderWithId<RelayId>,
    user_agent: Arc<String>,
    auth_master_key: Option<nostr::Keys>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let mut last_connection_time = SystemTime::UNIX_EPOCH;
    let mut connection_delay = Duration::from_secs(5);
    let mut cs = ConnectionState {
        url,
        user_agent,
        auth_master_key,
        subs: HashMap::with_capacity(10),
        auth: None,
    };
    let mut ws = loop {
        match rx_for_ops.recv().await {
            Ok(m) => {
                break first_request(
                    &Some(m),
                    &mut cs,
                    &mut last_connection_time,
                    &mut connection_delay,
                )
                .await?;
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("{} is too slow. skipped {n} ops.", cs.url);
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    };
    let mut waiting_for_pong = false;
    loop {
        let unhandled_message = loop {
            tokio::select! {
                r = rx_for_ops.recv() => {
                    match r {
                        Ok(r) => {
                            if let Err(unhandled_message) =
                                handle_ops(r, &mut cs, &mut ws).await {
                                break unhandled_message;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("{} is too slow. skipped {n} ops.", cs.url);
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
                m = tokio::time::timeout(TIMEOUT_DURATION, ws.next()) => {
                    trace!("{} ==> {m:?}",cs.url);
                    if handle_message(
                        m,
                        &mut cs,
                        &mut ws,
                        &tx_for_events,
                        &mut waiting_for_pong,
                    ).await {
                        break None;
                    }
                }
                else => return Ok(()),
            }
        };
        if unhandled_message.is_none() && cs.subs.is_empty() {
            ws = loop {
                match rx_for_ops.recv().await {
                    Ok(m) => {
                        break first_request(
                            &Some(m),
                            &mut cs,
                            &mut last_connection_time,
                            &mut connection_delay,
                        )
                        .await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("{} is too slow. skipped {n} ops.", cs.url);
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            };
        } else {
            ws = first_request(
                &unhandled_message.map(|UnhandledMessage(e, s)| ClientMessage::Event(e, s)),
                &mut cs,
                &mut last_connection_time,
                &mut connection_delay,
            )
            .await?;
        }
    }
}

#[tracing::instrument(skip_all)]
async fn handle_ops(
    message: ClientMessage,
    cs: &mut ConnectionState,
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<(), Option<UnhandledMessage>> {
    if let Err(e) = send_event(&message, cs, ws).await {
        use tokio_tungstenite::tungstenite::Error::*;
        match e {
            ConnectionClosed | AlreadyClosed | WriteBufferFull(_) => {
                if let ClientMessage::Event(e, s) = message {
                    Err(Some(UnhandledMessage(e, s)))
                } else {
                    Err(None)
                }
            }
            AttackAttempt => {
                warn!("AttackAttempt error");
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(())
            }
            e => {
                warn!("unknown error: {e}");
                Ok(())
            }
        }
    } else {
        Ok(())
    }
}

async fn send_event(
    message: &ClientMessage,
    cs: &mut ConnectionState,
    // url: &url::Url,
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    // subs: &mut HashMap<FilterId, Vec<Filter>>,
    // auth: &Option<String>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let m = match message {
        ClientMessage::Req {
            subscription_id: id,
            filters,
        } => {
            cs.subs.insert(*id, filters.clone());
            req_as_json(*id, filters)
        }
        ClientMessage::Close(id) => {
            cs.subs.remove(id);
            format!(r#"["CLOSE",{id}]"#)
        }
        ClientMessage::Event(e, keys) => {
            if let (Some(auth), Some(keys)) = (&cs.auth, keys) {
                let e = nostr::EventBuilder::auth(auth, cs.url.clone())
                    .to_event(keys)
                    .unwrap();
                let m = format!(r#"["AUTH",{}]"#, e.as_json());
                debug!("{} <== {m}", cs.url);
                ws.send(Message::Text(m)).await?;
            }
            format!(r#"["EVENT",{}]"#, e.as_json())
        }
    };
    debug!("{} <== {m}", cs.url);
    ws.send(Message::Text(m)).await
}

fn req_as_json(id: FilterId, filters: &[Filter]) -> String {
    format!(
        r#"["REQ",{id},{}]"#,
        filters.iter().format_with(",", |a, f| f(&a.as_json()))
    )
}

#[tracing::instrument(skip_all)]
async fn handle_message<RelayId: Clone + Copy + PartialEq>(
    m: Result<Option<Result<Message, tokio_tungstenite::tungstenite::Error>>, Elapsed>,
    cs: &mut ConnectionState,
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    tx_for_events: &SenderWithId<RelayId>,
    waiting_for_pong: &mut bool,
) -> bool {
    match m {
        Ok(Some(Ok(m))) => {
            *waiting_for_pong = false;
            match m {
                Message::Text(t) => match RelayMessage::from_json(&t) {
                    Ok(m) => {
                        match &m {
                            RelayMessage::Notice { message } => {
                                info!("notice from {}: {message}", cs.url);
                            }
                            RelayMessage::Event { .. } => {
                                tx_for_events.send(m).await;
                            }
                            RelayMessage::Auth { challenge } => {
                                if let Some(keys) = &cs.auth_master_key {
                                    let e = nostr::EventBuilder::auth(challenge, cs.url.clone())
                                        .to_event(keys)
                                        .unwrap();
                                    let m = format!(r#"["AUTH",{}]"#, e.as_json());
                                    debug!("{} <== {m}", cs.url);
                                    let _ = ws.send(Message::Text(m)).await;
                                } else {
                                    cs.auth = Some(challenge.clone())
                                }
                            }
                            _ => {
                                debug!("{} ==> {t}", cs.url);
                                tx_for_events.send(m).await;
                            }
                        }
                        false
                    }
                    Err(e) => {
                        debug!("could not deserialize message from relay: {e}");
                        debug!("message = {t}");
                        false
                    }
                },
                Message::Ping(payload) => {
                    debug!("{} <== pong {:?}", cs.url, payload);
                    let _ = ws.send(Message::Pong(payload)).await;
                    false
                }
                Message::Pong(_) => false,
                m => {
                    warn!("error: {m:?}");
                    false
                }
            }
        }
        Ok(None) => {
            warn!("connection is closed: {}", cs.url);
            true
        }
        Ok(Some(Err(e))) => {
            warn!("error: {e:?}");
            false
        }
        Err(e) => {
            debug!("timeout: {e}");
            if !cs.subs.is_empty() {
                if *waiting_for_pong {
                    *waiting_for_pong = false;
                    let _ = ws.close(None).await;
                    true
                } else {
                    debug!("{} <== ping", cs.url);
                    let _ = ws.send(Message::Ping(Vec::new())).await;
                    *waiting_for_pong = true;
                    false
                }
            } else {
                false
            }
        }
    }
}

/// Messages sent by clients, received by relays
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClientMessage {
    /// Event
    Event(Arc<Event>, Option<Arc<nostr::Keys>>),
    /// Req
    Req {
        /// Subscription ID
        subscription_id: FilterId,
        /// Filters
        filters: Vec<Filter>,
    },
    /// Close
    Close(FilterId),
}
