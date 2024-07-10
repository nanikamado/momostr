mod activity;
mod bot;
mod db;
mod error;
mod event_deletion_queue;
mod memory_debug;
mod nostr;
mod nostr_to_ap;
mod rsa_keys;
mod server;
mod util;

use cached::TimedSizedCache;
use db::Db;
use event_deletion_queue::EventDeletionQueue;
use html_to_md::FmtHtmlToMd;
use itertools::Itertools;
use lru::LruCache;
use nostr_lib::types::Filter;
use nostr_lib::{FromBech32, Kind, PublicKey, SecretKey, Timestamp, ToBech32};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use relay_pool::RelayPool;
use rustc_hash::{FxHashMap, FxHashSet};
use server::{listen, AppState};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use util::RateLimiter;

const DOMAIN: &str = env!("DOMAIN");
static REVERSE_DNS: Lazy<String> = Lazy::new(|| DOMAIN.split('.').rev().join("."));
const HTTPS_DOMAIN: &str = env!("HTTPS_DOMAIN");
const NOTE_ID_PREFIX: &str = env!("NOTE_ID_PREFIX");
const USER_ID_PREFIX: &str = env!("USER_ID_PREFIX");
const BIND_ADDRESS: &str = env!("BIND_ADDRESS");
const SECRET_KEY: &str = env!("SECRET_KEY");
const STACK_SIZE: usize = 8 * 1024 * 1024;
static RELAYS_EXTERNAL: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("EXTERNAL_MAIN_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static INBOX_RELAYS: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("INBOX_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static OUTBOX_RELAYS: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("OUTBOX_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static INBOX_RELAYS_FOR_10002: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("INBOX_RELAYS_FOR_10002")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static OUTBOX_RELAYS_FOR_10002: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("OUTBOX_RELAYS_FOR_10002")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static METADATA_RELAYS: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("METADATA_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static AP_RELAYS: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("AP_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
const CONTACT_LIST_LEN_LIMIT: usize = 500;
static BOT_SEC: Lazy<SecretKey> = Lazy::new(|| SecretKey::from_bech32(env!("BOT_NSEC")).unwrap());
static BOT_PUB: Lazy<PublicKey> =
    Lazy::new(|| nostr_lib::key::Keys::new(BOT_SEC.clone()).public_key());
static ADMIN_PUB: Lazy<Option<PublicKey>> =
    Lazy::new(|| Some(PublicKey::from_bech32(option_env!("ADMIN_NPUB")?).unwrap()));
static USER_AGENT: Lazy<String> =
    Lazy::new(|| format!("Momostr/{} ({HTTPS_DOMAIN})", env!("CARGO_PKG_VERSION")));
static NPUB_REG: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:nostr:)?(npub1[0-9a-z]{50,}|nprofile1[0-9a-z]{50,})").unwrap());
static KEY_ID: Lazy<String> =
    Lazy::new(|| format!("{USER_ID_PREFIX}{}", BOT_PUB.to_bech32().unwrap()));

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RelayId(u32);

const MAIN_RELAY: RelayId = RelayId(0);

fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,momostr=debug,relay_pool=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    assert!(SECRET_KEY.len() > 10);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(STACK_SIZE)
        .build()
        .unwrap()
        .block_on(run())
}

async fn relay_to_id(
    nostr: &RelayPool<RelayId>,
    url: &str,
    relay_to_id_map: &mut FxHashMap<url::Url, RelayId>,
) -> RelayId {
    let l = url::Url::parse(url).unwrap();
    if let Some(id) = relay_to_id_map.get(&l) {
        *id
    } else {
        let id = RelayId(relay_to_id_map.len() as u32);
        relay_to_id_map.insert(l.clone(), id);
        nostr.add_relay(id, l).await.unwrap();
        id
    }
}

async fn run() {
    let db = Db::new().await;
    let nostr = RelayPool::new(USER_AGENT.to_string()).await;
    let mut relay_to_id_map = FxHashMap::default();
    for (i, l) in INBOX_RELAYS.iter().chain(OUTBOX_RELAYS.iter()).enumerate() {
        nostr
            .add_relay(RelayId(i as u32), url::Url::parse(l).unwrap())
            .await
            .unwrap();
    }
    let mut inbox_relays = FxHashSet::default();
    for l in &*INBOX_RELAYS {
        inbox_relays.insert(relay_to_id(&nostr, l, &mut relay_to_id_map).await);
    }
    let mut outbox_relays = FxHashSet::default();
    for l in &*OUTBOX_RELAYS {
        outbox_relays.insert(relay_to_id(&nostr, l, &mut relay_to_id_map).await);
    }
    let mut metadata_relays = FxHashSet::default();
    for l in &*METADATA_RELAYS {
        metadata_relays.insert(relay_to_id(&nostr, l, &mut relay_to_id_map).await);
    }
    let inbox_relays: Arc<FxHashSet<RelayId>> = Arc::new(inbox_relays);
    let filter = get_filter();
    let event_stream = nostr.subscribe(filter, inbox_relays.clone()).await;
    let http_client = reqwest::Client::new();
    let state = Arc::new(AppState {
        nostr,
        nostr_send_rate: Mutex::new(RateLimiter::new(5, Duration::from_secs(1))),
        nostr_subscribe_rate: Mutex::new(RateLimiter::new(50, Duration::from_secs(1))),
        relay_url: RELAYS_EXTERNAL.iter().map(|a| a.to_string()).collect(),
        http_client: http_client.clone(),
        note_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1_000).unwrap())),
        actor_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1_000).unwrap())),
        nostr_user_cache: Mutex::new(TimedSizedCache::with_size_and_lifespan(1_000, 60 * 10)),
        db,
        inbox_relays,
        outbox_relays: Arc::new(outbox_relays),
        metadata_relays: Arc::new(metadata_relays),
        event_deletion_queue: EventDeletionQueue::new(Arc::new(http_client)),
    });

    tokio::try_join!(
        listen(state.clone()),
        nostr_to_ap::watch(event_stream, &state),
        dead_lock_detection(),
    )
    .unwrap();
}

fn get_filter() -> Vec<Filter> {
    vec![
        Filter {
            since: Some(Timestamp::now() - Duration::from_secs(60 * 3)),
            kinds: Some(
                [
                    Kind::ContactList,
                    Kind::TextNote,
                    Kind::EventDeletion,
                    Kind::Reaction,
                    Kind::Repost,
                    Kind::Metadata,
                ]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        },
        Filter {
            since: Some(Timestamp::now() - Duration::from_secs(60 * 3)),
            kinds: Some([Kind::EncryptedDirectMessage].into_iter().collect()),
            ..Default::default()
        }
        .pubkey(*BOT_PUB),
    ]
}

fn html_to_text(html: &str) -> String {
    FmtHtmlToMd(html).to_string()
}

async fn dead_lock_detection() -> Result<(), error::Error> {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60 * 2)).await;
        for deadlock in parking_lot::deadlock::check_deadlock() {
            if let Some(d) = deadlock.first() {
                return Err(error::Error::Internal(
                    anyhow::anyhow!(format!(
                        "found deadlock {}:\n{:?}",
                        d.thread_id(),
                        d.backtrace()
                    ))
                    .into(),
                ));
            }
        }
    }
}
