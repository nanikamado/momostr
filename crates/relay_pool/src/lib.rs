#[cfg(feature = "nostr")]
pub mod nostr_sdk_impl;
#[cfg(feature = "nostr-types")]
pub mod nostr_types_impl;

use cached::{Cached, TimedCache};
use futures_util::{SinkExt, Stream};
use id_pool::IdPool;
use itertools::Itertools;
use lru::LruCache;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::HashMap;
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::atomic::{self, AtomicU32};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::net::TcpStream;
use tokio::sync::mpsc::Sender;
use tokio::sync::{broadcast, oneshot};
use tokio::time::error::Elapsed;
use tokio_stream::StreamExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::USER_AGENT;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, trace, warn};

pub trait PoolTypes {
    type RelayId: Copy + Send + Sync + Eq + Hash + Debug + 'static;
    type Event: Send + Sync + Clone + NostrEvent<EventId = Self::EventId> + 'static;
    type Filter: NostrFilter<Self::Event>;
    type EventId: for<'a> Deserialize<'a> + Serialize + Clone + Debug + Eq + Send + Sync + Hash;
}

pub trait NostrEvent: Serialize + for<'a> Deserialize<'a> + Debug {
    type EventId;
    fn verify(&self) -> bool;
    fn id(&self) -> &Self::EventId;
    fn id_as_bytes(&self) -> &[u8; 32];
}

impl<T: NostrEvent> NostrEvent for Arc<T> {
    type EventId = T::EventId;

    fn verify(&self) -> bool {
        self.as_ref().verify()
    }

    fn id(&self) -> &Self::EventId {
        self.as_ref().id()
    }

    fn id_as_bytes(&self) -> &[u8; 32] {
        self.as_ref().id_as_bytes()
    }
}

pub trait NostrFilter<Event>: Serialize + Debug + Send + Sync + Clone {
    fn match_event(&self, event: &Event) -> bool;
}

#[derive(Debug, Clone)]
struct SendEvent<T: PoolTypes> {
    event: T::Event,
    keys: Option<Arc<lnostr::Keypair>>,
    relays: Arc<FxHashSet<T::RelayId>>,
}

#[derive(Debug)]
pub struct RelayPool<T: PoolTypes> {
    queue_sender: Sender<RelayPoolOp<T>>,
    counter: AtomicU32,
}

struct SenderWithId<T: PoolTypes> {
    sender: Sender<RelayPoolOp<T>>,
    id: T::RelayId,
}

enum RelayMessage<T: PoolTypes> {
    Event(String, T::Event),
    Ok(T::EventId, bool, String),
    Eose(String),
}

struct RelayMessageWithId<T: PoolTypes> {
    relay_message: RelayMessage<T>,
    id: T::RelayId,
}

impl<T: PoolTypes> SenderWithId<T> {
    async fn send(&self, message: RelayMessage<T>) {
        self.sender
            .send(RelayPoolOp::Receive(RelayMessageWithId {
                relay_message: message,
                id: self.id,
            }))
            .await
            .unwrap()
    }
}

#[derive(Debug)]
struct RelayOp<T: PoolTypes> {
    msg: ClientMessage<T>,
    relays: Arc<FxHashSet<T::RelayId>>,
}

impl<T: PoolTypes> Clone for RelayOp<T> {
    fn clone(&self) -> Self {
        Self {
            msg: self.msg.clone(),
            relays: self.relays.clone(),
        }
    }
}

struct ReceiverWithId<T: PoolTypes> {
    receiver: broadcast::Receiver<RelayOp<T>>,
    id: T::RelayId,
}

impl<T: PoolTypes> ReceiverWithId<T> {
    async fn recv(&mut self) -> Result<ClientMessage<T>, broadcast::error::RecvError> {
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

enum RelayPoolOp<T: PoolTypes> {
    AddRelay(T::RelayId, url::Url, Option<Arc<lnostr::Keypair>>),
    Subscribe(SubConfig<T>),
    Unsubscribe(u32),
    Send(SendEvent<T>),
    Receive(RelayMessageWithId<T>),
    SetOkTracker(T::EventId, oneshot::Sender<(bool, String)>),
}

struct SubConfig<T: PoolTypes> {
    sub_id: u32,
    filters: Arc<Vec<T::Filter>>,
    sender: Sender<EventWithRelayId<T>>,
    relays: FxHashSet<T::RelayId>,
    until_eose: bool,
}

impl<T: PoolTypes + 'static> RelayPool<T> {
    pub async fn new(user_agent: String) -> Self {
        let user_agent = Arc::new(user_agent);
        let broadcast_sender: tokio::sync::broadcast::Sender<RelayOp<T>> =
            tokio::sync::broadcast::Sender::new(1_000);
        let (queue_sender, mut queue_receiver) = tokio::sync::mpsc::channel(100);
        let mut subs = SubscriptionState::new(broadcast_sender, queue_sender.clone(), user_agent);
        tokio::spawn(async move {
            while let Some(op) = queue_receiver.recv().await {
                subs.handle_op(op).await;
            }
        });
        Self {
            queue_sender,
            counter: AtomicU32::new(0),
        }
    }

    pub async fn add_relay(
        &self,
        relay_id: T::RelayId,
        url: url::Url,
        auth_master_key: Option<Arc<lnostr::Keypair>>,
    ) -> Result<(), ()> {
        self.queue_sender
            .send(RelayPoolOp::AddRelay(relay_id, url, auth_master_key))
            .await
            .map_err(|_| ())
    }

    pub async fn subscribe(
        &self,
        filters: Arc<Vec<T::Filter>>,
        relays: FxHashSet<T::RelayId>,
    ) -> EventStream<T> {
        self.subscribe_aux(filters, relays, false).await
    }

    pub async fn get_events(
        &self,
        filters: Arc<Vec<T::Filter>>,
        relays: FxHashSet<T::RelayId>,
    ) -> EventStream<T> {
        self.subscribe_aux(filters, relays, true).await
    }

    pub async fn subscribe_aux(
        &self,
        filters: Arc<Vec<T::Filter>>,
        relays: FxHashSet<T::RelayId>,
        until_eose: bool,
    ) -> EventStream<T> {
        let (tx, rx) = tokio::sync::mpsc::channel(1_000);
        let id = self.counter.fetch_add(1, atomic::Ordering::Relaxed);
        self.queue_sender
            .send(RelayPoolOp::Subscribe(SubConfig {
                sub_id: id,
                filters,
                sender: tx,
                relays,
                until_eose,
            }))
            .await
            .unwrap();
        EventStream {
            stream: rx,
            tx_for_ops: self.queue_sender.clone(),
            id,
            cache: LruCache::new(NonZeroUsize::new(100).unwrap()),
        }
    }

    pub async fn get_event_with_timeout(
        &self,
        f: T::Filter,
        timeout: Duration,
        relays: FxHashSet<T::RelayId>,
    ) -> Option<EventWithRelayId<T>> {
        tokio::time::timeout(
            timeout,
            self.subscribe_aux(vec![f].into(), relays, true)
                .await
                .next(),
        )
        .await
        .ok()
        .flatten()
    }

    pub async fn send(
        &self,
        event: T::Event,
        keys: Option<Arc<lnostr::Keypair>>,
        relays: Arc<FxHashSet<T::RelayId>>,
    ) {
        self.queue_sender
            .send(RelayPoolOp::Send(SendEvent {
                event,
                keys,
                relays,
            }))
            .await
            .unwrap();
    }

    pub async fn send_and_get_first_ok(
        &self,
        event: T::Event,
        keys: Option<Arc<lnostr::Keypair>>,
        relays: Arc<FxHashSet<T::RelayId>>,
    ) -> oneshot::Receiver<(bool, String)> {
        let (tx, rx) = oneshot::channel();
        self.queue_sender
            .send(RelayPoolOp::SetOkTracker(event.id().clone(), tx))
            .await
            .unwrap();
        self.queue_sender
            .send(RelayPoolOp::Send(SendEvent {
                event,
                keys,
                relays,
            }))
            .await
            .unwrap();
        rx
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
struct FilterId(u32);

#[derive(Debug, PartialEq, PartialOrd, Clone)]
pub struct EventWithRelayId<T: PoolTypes> {
    pub event: T::Event,
    pub relay_id: T::RelayId,
}

pub enum EventOrEose<T: PoolTypes> {
    Event(EventWithRelayId<T>),
    Eose,
}

struct SubscriptionState<T: PoolTypes> {
    sub_id_to_filter_id: HashMap<u32, FilterId>,
    filter_id_to_senders: FxHashMap<FilterId, SubConfig<T>>,
    ok_subscription: TimedCache<T::EventId, oneshot::Sender<(bool, String)>>,
    id_pool: IdPool,
    broadcast_sender: tokio::sync::broadcast::Sender<RelayOp<T>>,
    op_sender: Sender<RelayPoolOp<T>>,
    user_agent: Arc<String>,
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

impl<T: PoolTypes + 'static> SubscriptionState<T> {
    fn new(
        broadcast_sender: tokio::sync::broadcast::Sender<RelayOp<T>>,
        op_sender: Sender<RelayPoolOp<T>>,
        user_agent: Arc<String>,
    ) -> Self {
        Self {
            sub_id_to_filter_id: Default::default(),
            filter_id_to_senders: Default::default(),
            ok_subscription: TimedCache::with_lifespan(60),
            id_pool: IdPool::new_ranged(0..u32::MAX),
            broadcast_sender,
            op_sender,
            user_agent,
        }
    }

    fn handle_event(&mut self, m: RelayMessageWithId<T>) {
        match m.relay_message {
            RelayMessage::Event(subscription_id, event) => {
                if !event.verify() {
                    return;
                }
                if let Ok(id) = subscription_id.to_string().parse() {
                    if let Some(f) = self.filter_id_to_senders.get(&id) {
                        if f.filters.iter().any(|f| f.match_event(&event)) {
                            let _ = f.sender.try_send(EventWithRelayId {
                                event: event.clone(),
                                relay_id: m.id,
                            });
                        } else {
                            warn!(
                                "received event {} did not match the filter {}.",
                                serde_json::to_string(&event).unwrap(),
                                serde_json::to_string(&f.filters).unwrap()
                            );
                        }
                    } else {
                        debug!("subscription id {id} not found");
                    }
                }
            }
            RelayMessage::Ok(id, status, message) => {
                if let Some(a) = self.ok_subscription.cache_remove(&id) {
                    let _ = a.send((status, message));
                }
            }
            RelayMessage::Eose(f_id) => {
                if let Ok(f_id) = f_id.to_string().parse() {
                    if let Some(f) = self.filter_id_to_senders.get_mut(&f_id) {
                        if f.until_eose && f.relays.remove(&m.id) {
                            broadcast(
                                &self.broadcast_sender,
                                RelayOp {
                                    msg: ClientMessage::Close(f_id),
                                    relays: Arc::new([m.id].into_iter().collect()),
                                },
                            );
                            if f.relays.is_empty() {
                                let id = f.sub_id;
                                self.unsub(id);
                            }
                        }
                    } else {
                        debug!("subscription id {f_id} not found");
                    }
                }
            }
        }
    }

    // common code among subscribe and filter change
    #[tracing::instrument(skip_all)]
    fn sub(&mut self, sub_config: SubConfig<T>) {
        let filter_id = FilterId(self.id_pool.request_id().unwrap());
        debug!(
            "starting connection with id {filter_id}, filters = [{}]",
            sub_config
                .filters
                .iter()
                .format_with(", ", |a, f| f(&serde_json::to_string(a).unwrap()))
        );
        broadcast(
            &self.broadcast_sender,
            RelayOp {
                msg: ClientMessage::Req {
                    subscription_id: filter_id,
                    filters: sub_config.filters.clone(),
                },
                relays: Arc::new(sub_config.relays.clone()),
            },
        );
        self.sub_id_to_filter_id
            .insert(sub_config.sub_id, filter_id);
        self.filter_id_to_senders.insert(filter_id, sub_config);
    }

    // common code among unsubscribe and filter change
    #[tracing::instrument(skip_all)]
    fn unsub(&mut self, id: u32) -> Option<()> {
        let filter_id = self.sub_id_to_filter_id.remove(&id)?;
        let f = self.filter_id_to_senders.remove(&filter_id)?;
        self.id_pool.return_id(filter_id.0).unwrap();
        broadcast(
            &self.broadcast_sender,
            RelayOp {
                msg: ClientMessage::Close(filter_id),
                relays: Arc::new(f.relays),
            },
        );
        debug!(
            "closed the connection of id {filter_id}: {}",
            serde_json::to_string(&f.filters).unwrap()
        );
        Some(())
    }

    async fn handle_op(&mut self, op: RelayPoolOp<T>) {
        match op {
            RelayPoolOp::AddRelay(id, url, auth_master_key) => {
                let receiver = self.broadcast_sender.subscribe();
                let sender = self.op_sender.clone();
                let user_agent = self.user_agent.clone();
                tokio::spawn(async move {
                    if let Err(e) = subscribe_relay(
                        url,
                        ReceiverWithId { receiver, id },
                        SenderWithId { sender, id },
                        user_agent,
                        auth_master_key,
                    )
                    .await
                    {
                        error!("{e}");
                    }
                });
            }
            RelayPoolOp::Subscribe(s) => {
                self.sub(s);
            }
            RelayPoolOp::Unsubscribe(id) => {
                self.unsub(id);
            }
            RelayPoolOp::Send(op) => {
                broadcast(
                    &self.broadcast_sender,
                    RelayOp {
                        msg: ClientMessage::Event(op.event, op.keys),
                        relays: op.relays,
                    },
                );
            }
            RelayPoolOp::Receive(e) => self.handle_event(e),
            RelayPoolOp::SetOkTracker(id, sender) => {
                self.ok_subscription.cache_set(id, sender);
            }
        }
    }
}

pub struct EventStream<T: PoolTypes> {
    stream: tokio::sync::mpsc::Receiver<EventWithRelayId<T>>,
    tx_for_ops: Sender<RelayPoolOp<T>>,
    id: u32,
    cache: LruCache<[u8; 32], ()>,
}

impl<T: PoolTypes> Stream for EventStream<T> {
    type Item = EventWithRelayId<T>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll::*;
        loop {
            match self.stream.poll_recv(cx) {
                Ready(Some(a)) => {
                    if check_event(&mut self, a.event.id_as_bytes()) {
                        break Ready(Some(a));
                    }
                }
                Ready(None) => break Ready(None),
                Pending => break Pending,
            }
        }
    }
}

fn check_event<T: PoolTypes>(event_stream: &mut EventStream<T>, event_id: &[u8; 32]) -> bool {
    if event_stream.cache.contains(event_id) {
        false
    } else {
        event_stream.cache.put(*event_id, ());
        true
    }
}

impl<T: PoolTypes> Drop for EventStream<T> {
    fn drop(&mut self) {
        self.tx_for_ops
            .try_send(RelayPoolOp::Unsubscribe(self.id))
            .unwrap();
    }
}

impl<T: PoolTypes> EventStream<T> {
    pub fn id(&self) -> u32 {
        self.id
    }
}

fn broadcast<T: PoolTypes>(tx: &broadcast::Sender<RelayOp<T>>, message: RelayOp<T>) {
    if let Err(e) = tx.send(message) {
        error!(
            "failed to connect to relays: {e}, count = {}",
            tx.receiver_count()
        )
    }
}

#[tracing::instrument(skip_all)]
async fn first_request<T: PoolTypes>(
    message: &Option<ClientMessage<T>>,
    cs: &mut ConnectionState<T>,
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
        let m = req_as_json::<T>(*id, filters);
        debug!("{} <== {m}", cs.url);
        ws.send(Message::Text(m)).await?;
    }
    if let Some(m) = message {
        send_event(m, cs, &mut ws).await?;
    }
    Ok(ws)
}

const TIMEOUT_DURATION: Duration = Duration::from_secs(60 * 3);

struct UnhandledMessage<T: PoolTypes>(T::Event, Option<Arc<lnostr::Keypair>>);

struct ConnectionState<T: PoolTypes> {
    url: url::Url,
    user_agent: Arc<String>,
    auth_master_key: Option<Arc<lnostr::Keypair>>,
    subs: HashMap<FilterId, Arc<Vec<T::Filter>>>,
    auth: Option<String>,
}

async fn subscribe_relay<T: PoolTypes>(
    url: url::Url,
    mut rx_for_ops: ReceiverWithId<T>,
    tx_for_events: SenderWithId<T>,
    user_agent: Arc<String>,
    auth_master_key: Option<Arc<lnostr::Keypair>>,
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
async fn handle_ops<T: PoolTypes>(
    message: ClientMessage<T>,
    cs: &mut ConnectionState<T>,
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<(), Option<UnhandledMessage<T>>> {
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

async fn send_event<T: PoolTypes>(
    message: &ClientMessage<T>,
    cs: &mut ConnectionState<T>,
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let m = match message {
        ClientMessage::Req {
            subscription_id: id,
            filters,
        } => {
            cs.subs.insert(*id, filters.clone());
            req_as_json::<T>(*id, filters)
        }
        ClientMessage::Close(id) => {
            cs.subs.remove(id);
            format!(r#"["CLOSE",{id}]"#)
        }
        ClientMessage::Event(e, keys) => {
            if let (Some(auth), Some(keys)) = (&cs.auth, keys) {
                let e = lnostr::EventBuilder::auth(auth, cs.url.as_str()).to_event(keys);
                let m = format!(r#"["AUTH",{}]"#, serde_json::to_string(&e).unwrap());
                debug!("{} <== {m}", cs.url);
                ws.send(Message::Text(m)).await?;
            }
            format!(r#"["EVENT",{}]"#, serde_json::to_string(e).unwrap())
        }
    };
    debug!("{} <== {m}", cs.url);
    ws.send(Message::Text(m)).await
}

fn req_as_json<T: PoolTypes>(id: FilterId, filters: &[T::Filter]) -> String {
    format!(
        r#"["REQ",{id},{}]"#,
        filters
            .iter()
            .format_with(",", |a, f| f(&serde_json::to_string(a).unwrap()))
    )
}

#[tracing::instrument(skip_all)]
async fn handle_message<T: PoolTypes>(
    m: Result<Option<Result<Message, tokio_tungstenite::tungstenite::Error>>, Elapsed>,
    cs: &mut ConnectionState<T>,
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    tx_for_events: &SenderWithId<T>,
    waiting_for_pong: &mut bool,
) -> bool {
    match m {
        Ok(Some(Ok(m))) => {
            use lnostr::RelayMessage;
            *waiting_for_pong = false;
            match m {
                Message::Text(t) => match serde_json::from_str::<RelayMessage<_, _>>(&t) {
                    Ok(m) => {
                        match m {
                            RelayMessage::Notice(message) => {
                                info!("notice from {}: {message}", cs.url);
                            }
                            RelayMessage::Event(subscription_id, event) => {
                                tx_for_events
                                    .send(crate::RelayMessage::Event(subscription_id, event))
                                    .await;
                            }
                            RelayMessage::Auth(challenge) => {
                                if let Some(keys) = &cs.auth_master_key {
                                    let e = lnostr::EventBuilder::auth(&challenge, cs.url.as_str())
                                        .to_event(keys);
                                    let m = format!(
                                        r#"["AUTH",{}]"#,
                                        serde_json::to_string(&e).unwrap()
                                    );
                                    debug!("{} <== {m}", cs.url);
                                    let _ = ws.send(Message::Text(m)).await;
                                } else {
                                    cs.auth = Some(challenge.clone())
                                }
                            }
                            RelayMessage::Ok(id, status, m) => {
                                tx_for_events
                                    .send(crate::RelayMessage::Ok(id, status, m))
                                    .await;
                            }
                            RelayMessage::Eose(sub_id) => {
                                tx_for_events.send(crate::RelayMessage::Eose(sub_id)).await;
                            }
                            _ => {
                                debug!("ignored {} ==> {t}", cs.url);
                                // tx_for_events.send(m).await;
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
#[derive(Debug, PartialEq, Eq)]
enum ClientMessage<T: PoolTypes> {
    /// Event
    Event(T::Event, Option<Arc<lnostr::Keypair>>),
    /// Req
    Req {
        /// Subscription ID
        subscription_id: FilterId,
        /// Filters
        filters: Arc<Vec<T::Filter>>,
    },
    /// Close
    Close(FilterId),
}

impl<T: PoolTypes> Clone for ClientMessage<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Event(arg0, arg1) => Self::Event(arg0.clone(), arg1.clone()),
            Self::Req {
                subscription_id,
                filters,
            } => Self::Req {
                subscription_id: *subscription_id,
                filters: filters.clone(),
            },
            Self::Close(arg0) => Self::Close(*arg0),
        }
    }
}
