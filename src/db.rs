use crate::server::InternalApId;
use lru::LruCache;
use nostr_lib::key::PublicKey;
use parking_lot::Mutex;
use rocksdb::DB as Rocks;
use rustc_hash::FxHashSet;
use std::fs::create_dir_all;
use std::num::NonZeroUsize;
use std::sync::atomic::{self, AtomicU32};
use std::sync::Arc;

#[derive(Debug)]
pub struct Db {
    inbox_to_id: Rocks,
    id_to_inbox: Rocks,
    inbox_counter: AtomicU32,
    event_id_to_inboxes: Rocks,
    nostr_to_followee: Rocks,
    nostr_to_followee_cache: Mutex<LruCache<nostr_lib::PublicKey, Arc<FxHashSet<Arc<String>>>>>,
    ap_id_to_event_id: Rocks,
    ap_id_to_event_id_cache: Mutex<LruCache<InternalApId<'static>, Option<nostr_lib::EventId>>>,
    stopped_npub: Rocks,
    stopped_npub_on_memory: Mutex<FxHashSet<PublicKey>>,
    stopped_ap: Rocks,
    stopped_ap_on_memory: Mutex<FxHashSet<String>>,
    event_counter: AtomicU32,
}

impl Db {
    pub async fn new() -> Self {
        let config_dir = dirs::config_dir().unwrap().join("momostr");
        create_dir_all(&config_dir).unwrap();
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_max_log_file_size(0);
        let inbox_to_id =
            Rocks::open(&opts, config_dir.join(env!("ROCKS_DB_INBOX_TO_ID"))).unwrap();
        let id_to_inbox =
            Rocks::open(&opts, config_dir.join(env!("ROCKS_DB_ID_TO_INBOX"))).unwrap();
        let inbox_to_id_len = inbox_to_id.iterator(rocksdb::IteratorMode::Start).count();
        let id_to_inbox_len = id_to_inbox.iterator(rocksdb::IteratorMode::Start).count();
        assert_eq!(inbox_to_id_len, id_to_inbox_len);
        let event_id_to_inboxes =
            Rocks::open(&opts, config_dir.join(env!("ROCKS_DB_EVENT_ID_TO_INBOXES"))).unwrap();
        let event_id_to_inboxes_len = event_id_to_inboxes
            .iterator(rocksdb::IteratorMode::Start)
            .count();
        let nostr_to_followee =
            Rocks::open(&opts, config_dir.join(env!("ROCKS_DB_NOSTR_TO_FOLLOWEE"))).unwrap();
        let stopped_npub =
            Rocks::open(&opts, config_dir.join(env!("ROCKS_DB_STOPPED_NPUB"))).unwrap();
        let stopped_npub_on_memory = Mutex::new(
            stopped_npub
                .iterator(rocksdb::IteratorMode::Start)
                .map(|a| PublicKey::from_slice(&a.unwrap().0).unwrap())
                .collect(),
        );
        let ap_id_to_event_id =
            Rocks::open(&opts, config_dir.join(env!("ROCKS_DB_AP_ID_TO_EVENT_ID"))).unwrap();
        let stopped_ap = Rocks::open(&opts, config_dir.join(env!("ROCKS_DB_STOPPED_AP"))).unwrap();
        let stopped_ap_on_memory = Mutex::new(
            stopped_ap
                .iterator(rocksdb::IteratorMode::Start)
                .map(|a| String::from_utf8(a.unwrap().0.to_vec()).unwrap())
                .collect(),
        );
        Self {
            inbox_to_id,
            id_to_inbox,
            event_id_to_inboxes,
            inbox_counter: AtomicU32::new(id_to_inbox_len as u32),
            event_counter: AtomicU32::new(event_id_to_inboxes_len as u32),
            nostr_to_followee,
            nostr_to_followee_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
            ap_id_to_event_id,
            ap_id_to_event_id_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1000).unwrap())),
            stopped_npub,
            stopped_npub_on_memory,
            stopped_ap,
            stopped_ap_on_memory,
        }
    }

    pub async fn insert_event_id_to_inbox(
        &self,
        event_id: &[u8],
        inboxes: impl Iterator<Item = String>,
    ) {
        let mut ids = Vec::new();
        ids.push(self.event_counter.fetch_add(1, atomic::Ordering::Relaxed));
        for i in inboxes {
            let id = match self.inbox_to_id.get(i.as_bytes()).unwrap() {
                Some(id) => u32::from_be_bytes(id.try_into().unwrap()),
                None => {
                    let id = self.inbox_counter.fetch_add(1, atomic::Ordering::Relaxed);
                    self.inbox_to_id
                        .put(i.as_bytes(), id.to_be_bytes())
                        .unwrap();
                    self.id_to_inbox.put(id.to_be_bytes(), i).unwrap();
                    id
                }
            };
            ids.push(id);
        }

        self.event_id_to_inboxes
            .put(event_id, rmp_serde::to_vec(&ids).unwrap())
            .unwrap();
    }

    pub async fn delete_event_id(&self, event_id: &[u8]) -> Vec<String> {
        match self.event_id_to_inboxes.get(event_id).unwrap() {
            Some(ids) => {
                let ids: Vec<u32> = rmp_serde::from_slice(&ids).unwrap();
                let inboxes: Vec<_> = ids
                    .into_iter()
                    .skip(1)
                    .map(|i: u32| {
                        String::from_utf8(self.id_to_inbox.get(i.to_be_bytes()).unwrap().unwrap())
                            .unwrap()
                    })
                    .collect();
                self.event_id_to_inboxes.delete(event_id).unwrap();
                inboxes
            }
            None => Vec::new(),
        }
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
