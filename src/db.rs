use crate::server::InternalApId;
use actor_store::ActivityCache;
use lru::LruCache;
use nostr_lib::key::PublicKey;
use parking_lot::Mutex;
use rocksdb::DB as Rocks;
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::create_dir_all;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{self, AtomicU32};
use std::sync::Arc;
use url::Url;

#[derive(Debug, Serialize, Deserialize)]
pub struct NostrEventMetadataForDb {
    pub kind: u32,
    pub inboxes: Vec<u32>,
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<PublicKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
}

#[derive(Debug)]
pub struct NostrEventMetadata {
    pub kind: nostr_lib::Kind,
    pub inboxes: Vec<Url>,
    pub author: Option<PublicKey>,
    pub object: Option<String>,
}

#[derive(Debug)]
pub struct Db {
    inbox_to_id: Rocks,
    id_to_inbox: Rocks,
    inbox_counter: AtomicU32,
    event_id_to_metadata: Rocks,
    nostr_to_followee: Rocks,
    nostr_to_followee_cache: Mutex<LruCache<nostr_lib::PublicKey, Arc<FxHashSet<Arc<String>>>>>,
    ap_id_to_event_id: Rocks,
    ap_id_to_event_id_cache: Mutex<LruCache<InternalApId<'static>, Option<nostr_lib::EventId>>>,
    stopped_npub: Rocks,
    stopped_npub_on_memory: Mutex<FxHashSet<PublicKey>>,
    stopped_ap: Rocks,
    stopped_ap_on_memory: Mutex<FxHashSet<String>>,
    nostr_to_followers: Rocks,
    nostr_to_followers_cache: Mutex<LruCache<nostr_lib::PublicKey, Arc<HashSet<String>>>>,
    ap_to_followees: Rocks,
    ap_to_followees_cache: Mutex<LruCache<String, Arc<FxHashSet<nostr_lib::PublicKey>>>>,
    npub_to_ap_id: Rocks,
    npub_to_ap_id_cache: Mutex<LruCache<nostr_lib::PublicKey, Option<Arc<String>>>>,
    pub activity_cache: ActivityCache,
}

impl Db {
    pub async fn new() -> Self {
        let p = shellexpand::tilde(env!("DB"));
        let config_dir = Path::new(p.as_ref());
        create_dir_all(config_dir).unwrap();
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_max_log_file_size(0);
        let inbox_to_id = Rocks::open(&opts, config_dir.join("inbox_to_id.rocksdb")).unwrap();
        let id_to_inbox = Rocks::open(&opts, config_dir.join("id_to_inbox.rocksdb")).unwrap();
        let inbox_to_id_len = inbox_to_id.iterator(rocksdb::IteratorMode::Start).count();
        let id_to_inbox_len = id_to_inbox.iterator(rocksdb::IteratorMode::Start).count();
        assert_eq!(inbox_to_id_len, id_to_inbox_len);
        let event_id_to_metadata =
            Rocks::open(&opts, config_dir.join("event_id_to_metadata.rocksdb")).unwrap();
        let nostr_to_followee = Rocks::open(&opts, config_dir.join("follow_list.rocksdb")).unwrap();
        let stopped_npub = Rocks::open(&opts, config_dir.join("stopped_npub.rocksdb")).unwrap();
        let stopped_npub_on_memory = Mutex::new(
            stopped_npub
                .iterator(rocksdb::IteratorMode::Start)
                .map(|a| PublicKey::from_slice(&a.unwrap().0).unwrap())
                .collect(),
        );
        let ap_id_to_event_id =
            Rocks::open(&opts, config_dir.join("ap_id_to_event_id.rocksdb")).unwrap();
        let stopped_ap = Rocks::open(&opts, config_dir.join("stopped_ap.rocksdb")).unwrap();
        let stopped_ap_on_memory = Mutex::new(
            stopped_ap
                .iterator(rocksdb::IteratorMode::Start)
                .map(|a| String::from_utf8(a.unwrap().0.to_vec()).unwrap())
                .collect(),
        );
        let nostr_to_followers =
            Rocks::open(&opts, config_dir.join("nostr_to_followers.rocksdb")).unwrap();
        let ap_to_followees =
            Rocks::open(&opts, config_dir.join("ap_to_followee.rocksdb")).unwrap();
        let npub_to_ap_id = Rocks::open(&opts, config_dir.join("npub_to_ap_id.rocksdb")).unwrap();
        let event_id_to_inboxes_path = config_dir.join("event_id_to_inboxes.rocksdb");
        if event_id_to_inboxes_path.exists() {
            let event_id_to_inboxes = Rocks::open(&opts, &event_id_to_inboxes_path).unwrap();
            for (k, v) in event_id_to_inboxes
                .iterator(rocksdb::IteratorMode::Start)
                .flatten()
            {
                let inboxes: Vec<u32> = rmp_serde::from_slice(&v).unwrap();
                event_id_to_metadata
                    .put(
                        k,
                        serde_json::to_vec(&NostrEventMetadataForDb {
                            kind: 1,
                            inboxes: inboxes[1..].to_vec(),
                            deleted: false,
                            author: None,
                            object: None,
                        })
                        .unwrap(),
                    )
                    .unwrap();
            }
            std::fs::rename(
                event_id_to_inboxes_path,
                config_dir.join("event_id_to_inboxes_archive.rocksdb"),
            )
            .unwrap();
        }
        Self {
            inbox_to_id,
            id_to_inbox,
            event_id_to_metadata,
            inbox_counter: AtomicU32::new(id_to_inbox_len as u32),
            nostr_to_followee,
            nostr_to_followee_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
            ap_id_to_event_id,
            ap_id_to_event_id_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
            stopped_npub,
            stopped_npub_on_memory,
            stopped_ap,
            stopped_ap_on_memory,
            nostr_to_followers,
            nostr_to_followers_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
            ap_to_followees,
            ap_to_followees_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
            npub_to_ap_id,
            npub_to_ap_id_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
            activity_cache: ActivityCache::new(config_dir, &opts),
        }
    }

    pub async fn insert_event_id_to_inbox(
        &self,
        event_id: &[u8],
        kind: nostr_lib::Kind,
        author: PublicKey,
        object: Option<String>,
        inboxes: impl Iterator<Item = String>,
    ) {
        let ids = inboxes
            .into_iter()
            .map(|i| match self.inbox_to_id.get(i.as_bytes()).unwrap() {
                Some(id) => u32::from_be_bytes(id.try_into().unwrap()),
                None => {
                    let id = self.inbox_counter.fetch_add(1, atomic::Ordering::Relaxed);
                    self.inbox_to_id
                        .put(i.as_bytes(), id.to_be_bytes())
                        .unwrap();
                    self.id_to_inbox.put(id.to_be_bytes(), i).unwrap();
                    id
                }
            })
            .collect();
        let s = serde_json::to_string(&NostrEventMetadataForDb {
            kind: kind.as_u32(),
            inboxes: ids,
            deleted: false,
            author: Some(author),
            object,
        })
        .unwrap();
        self.event_id_to_metadata.put(event_id, s).unwrap();
    }

    pub async fn delete_event_id(&self, event_id: &[u8]) -> Option<NostrEventMetadata> {
        let s = self.event_id_to_metadata.get(event_id).unwrap()?;
        let mut m: NostrEventMetadataForDb = serde_json::from_slice(&s).unwrap();
        m.deleted = true;
        self.event_id_to_metadata
            .put(event_id, serde_json::to_vec(&m).unwrap())
            .unwrap();
        Some(NostrEventMetadata {
            kind: nostr_lib::Kind::from(m.kind as u16),
            inboxes: m
                .inboxes
                .into_iter()
                .map(|i: u32| {
                    String::from_utf8(self.id_to_inbox.get(i.to_be_bytes()).unwrap().unwrap())
                        .unwrap()
                        .parse()
                        .unwrap()
                })
                .collect(),
            author: m.author,
            object: m.object,
        })
    }

    pub fn get_followee_of_nostr(
        &self,
        p: &nostr_lib::PublicKey,
    ) -> Option<Arc<FxHashSet<Arc<String>>>> {
        if let Some(l) = self.nostr_to_followee_cache.lock().get(p) {
            return Some(l.clone());
        }
        self.nostr_to_followee
            .get_pinned(p.to_bytes())
            .unwrap()
            .map(|a| rmp_serde::from_slice(&a).unwrap())
    }

    pub fn insert_followee_of_nostr(
        &self,
        p: nostr_lib::PublicKey,
        followee: Arc<FxHashSet<Arc<String>>>,
    ) {
        self.nostr_to_followee
            .put(p.to_bytes(), rmp_serde::to_vec(&followee).unwrap())
            .unwrap();
        self.nostr_to_followee_cache.lock().put(p, followee);
    }

    pub fn get_followers_of_nostr(&self, p: &nostr_lib::PublicKey) -> Option<Arc<HashSet<String>>> {
        if let Some(l) = self.nostr_to_followers_cache.lock().get(p) {
            return Some(l.clone());
        }
        self.nostr_to_followers
            .get_pinned(p.to_bytes())
            .unwrap()
            .map(|a| rmp_serde::from_slice(&a).unwrap())
    }

    pub fn insert_follower_of_nostr(&self, p: nostr_lib::PublicKey, follower: String) {
        let mut c = self.nostr_to_followers_cache.lock();
        let mut followers = c.get(&p).map_or_else(
            || {
                self.nostr_to_followers
                    .get_pinned(p.to_bytes())
                    .unwrap()
                    .map(|a| rmp_serde::from_slice(&a).unwrap())
                    .unwrap_or_default()
            },
            |a| (**a).clone(),
        );
        followers.insert(follower);
        self.nostr_to_followers
            .put(p.to_bytes(), rmp_serde::to_vec(&followers).unwrap())
            .unwrap();
        c.put(p, Arc::new(followers));
    }

    pub fn remove_follower_of_nostr(&self, p: nostr_lib::PublicKey, follower: &String) {
        let mut c = self.nostr_to_followers_cache.lock();
        let mut followers = c.get(&p).map_or_else(
            || {
                self.nostr_to_followers
                    .get_pinned(p.to_bytes())
                    .unwrap()
                    .map(|a| rmp_serde::from_slice(&a).unwrap())
                    .unwrap_or_default()
            },
            |a| (**a).clone(),
        );
        followers.remove(follower);
        self.nostr_to_followers
            .put(p.to_bytes(), rmp_serde::to_vec(&followers).unwrap())
            .unwrap();
        c.put(p, Arc::new(followers));
    }

    pub fn insert_followee_of_ap(
        &self,
        p: String,
        followee: nostr_lib::PublicKey,
    ) -> Arc<FxHashSet<PublicKey>> {
        let mut c = self.ap_to_followees_cache.lock();
        let mut followers = c.get(&p).map_or_else(
            || {
                self.ap_to_followees
                    .get_pinned(p.as_bytes())
                    .unwrap()
                    .map(|a| rmp_serde::from_slice(&a).unwrap())
                    .unwrap_or_default()
            },
            |a| (**a).clone(),
        );
        followers.insert(followee);
        self.ap_to_followees
            .put(p.as_bytes(), rmp_serde::to_vec(&followers).unwrap())
            .unwrap();
        let l = Arc::new(followers);
        c.put(p, l.clone());
        l
    }

    pub fn remove_followee_of_ap(
        &self,
        p: String,
        followee: &nostr_lib::PublicKey,
    ) -> Arc<FxHashSet<PublicKey>> {
        let mut c = self.ap_to_followees_cache.lock();
        let mut followers = c.get(&p).map_or_else(
            || {
                self.ap_to_followees
                    .get_pinned(p.as_bytes())
                    .unwrap()
                    .map(|a| rmp_serde::from_slice(&a).unwrap())
                    .unwrap_or_default()
            },
            |a| (**a).clone(),
        );
        followers.remove(followee);
        self.ap_to_followees
            .put(p.as_bytes(), rmp_serde::to_vec(&followers).unwrap())
            .unwrap();
        let l = Arc::new(followers);
        c.put(p, l.clone());
        l
    }

    pub fn insert_ap_id_to_event_id(
        &self,
        ap_id: InternalApId<'static>,
        event_id: nostr_lib::EventId,
    ) {
        self.ap_id_to_event_id
            .put(ap_id.as_bytes(), event_id.to_bytes())
            .unwrap();
        self.ap_id_to_event_id_cache
            .lock()
            .push(ap_id, Some(event_id));
    }

    pub fn get_event_id_from_ap_id(
        &self,
        ap_id: &InternalApId<'static>,
    ) -> Option<nostr_lib::EventId> {
        {
            if let Some(a) = self.ap_id_to_event_id_cache.lock().get(ap_id) {
                return *a;
            }
        }
        let r = Some(
            nostr_lib::EventId::from_slice(&self.ap_id_to_event_id.get(ap_id.as_bytes()).unwrap()?)
                .unwrap(),
        );
        self.ap_id_to_event_id_cache
            .lock()
            .put(ap_id.clone().into_owned(), r);
        r
    }

    pub fn insert_ap_id_of_npub(&self, p: &PublicKey, ap_id: Arc<String>) {
        self.npub_to_ap_id
            .put(p.to_bytes(), ap_id.as_bytes())
            .unwrap();
        self.npub_to_ap_id_cache.lock().push(*p, Some(ap_id));
    }

    pub fn get_ap_id_of_npub(&self, p: &PublicKey) -> Option<Arc<String>> {
        {
            if let Some(a) = self.npub_to_ap_id_cache.lock().get(p) {
                return a.clone();
            }
        }
        let r = self
            .npub_to_ap_id
            .get(p.to_bytes())
            .unwrap()
            .map(|a| Arc::new(String::from_utf8(a).unwrap()));
        self.npub_to_ap_id_cache.lock().put(*p, r.clone());
        r
    }

    pub fn is_stopped_npub(&self, npub: &PublicKey) -> bool {
        self.stopped_npub_on_memory.lock().contains(npub)
    }

    pub fn stop_npub(&self, npub: &PublicKey) {
        self.stopped_npub_on_memory.lock().insert(*npub);
        self.stopped_npub.put(npub.to_bytes(), []).unwrap();
    }

    pub fn restart_npub(&self, npub: &PublicKey) {
        self.stopped_npub_on_memory.lock().remove(npub);
        self.stopped_npub.delete(npub.to_bytes()).unwrap();
    }

    pub fn is_stopped_ap(&self, id: &str) -> bool {
        self.stopped_ap_on_memory.lock().contains(id)
    }

    pub fn stop_ap(&self, id: String) {
        self.stopped_ap.put(id.as_bytes(), []).unwrap();
        self.stopped_ap_on_memory.lock().insert(id);
    }

    pub fn restart_ap(&self, id: &str) {
        self.stopped_ap_on_memory.lock().remove(id);
        self.stopped_ap.delete(id.as_bytes()).unwrap();
    }
}

mod actor_store {
    use rocksdb::{Options, DB as Rocks};
    use std::path::Path;
    use std::time::Duration;

    #[derive(Debug)]
    pub struct ActivityCache {
        id_to_activity: Rocks,
    }

    impl ActivityCache {
        pub fn new(config_dir: &Path, opts: &Options) -> Self {
            let id_to_activity = Rocks::open_with_ttl(
                opts,
                config_dir.join("id_to_actor.rocksdb"),
                Duration::from_secs(60 * 60),
            )
            .unwrap();
            Self { id_to_activity }
        }

        pub fn insert(&self, id: &str, activity: &str) {
            self.id_to_activity.put(id, activity.as_bytes()).unwrap();
        }

        pub fn get(&self, id: &str) -> Option<String> {
            String::from_utf8(self.id_to_activity.get(id).unwrap()?).ok()
        }
    }
}
