// The following code is partially copied and edited from
// https://github.com/rust-nostr/nostr/blob/c6e0a6b7845c5ecc9504d8d2c3816f2c7786946c/crates/nostr/src/types/filter.rs
// which is licensed under https://github.com/rust-nostr/nostr/blob/c6e0a6b7845c5ecc9504d8d2c3816f2c7786946c/LICENSE

use nostr::event::{Event, EventId, Kind};
use nostr::key::PublicKey;
use nostr::types::{GenericTagValue, SingleLetterTag, Timestamp};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};

type GenericTags = BTreeMap<SingleLetterTag, BTreeSet<GenericTagValue>>;

/// Subscription filters
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Hash)]
pub struct Filter {
    /// List of [`EventId`]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<BTreeSet<EventId>>,
    /// List of [`PublicKey`]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authors: Option<BTreeSet<PublicKey>>,
    /// List of a kind numbers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kinds: Option<BTreeSet<Kind>>,
    /// It's a string describing a query in a human-readable form, i.e. "best nostr apps"
    ///
    /// <https://github.com/nostr-protocol/nips/blob/master/50.md>
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// An integer unix timestamp, events must be newer than this to pass
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<Timestamp>,
    /// An integer unix timestamp, events must be older than this to pass
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    /// Maximum number of events to be returned in the initial query
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Generic tag queries
    #[serde(flatten, serialize_with = "serialize_generic_tags")]
    #[serde(default)]
    pub generic_tags: GenericTags,
}

fn serialize_generic_tags<S>(generic_tags: &GenericTags, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut map = serializer.serialize_map(Some(generic_tags.len()))?;
    for (tag, values) in generic_tags.iter() {
        map.serialize_entry(&format!("#{tag}"), values)?;
    }
    map.end()
}

impl Filter {
    fn ids_match(&self, event: &Event) -> bool {
        self.ids
            .as_ref()
            .map_or(true, |ids| ids.is_empty() || ids.contains(&event.id))
    }

    fn authors_match(&self, event: &Event) -> bool {
        self.authors.as_ref().map_or(true, |authors| {
            authors.is_empty() || authors.contains(&event.pubkey)
        })
    }

    fn tag_match(&self, event: &Event) -> bool {
        if self.generic_tags.is_empty() {
            return true;
        }

        if event.tags.is_empty() {
            return false;
        }

        // Build tags indexes
        let mut idx: FxHashMap<SingleLetterTag, FxHashSet<GenericTagValue>> = FxHashMap::default();
        for (single_letter_tag, content) in event
            .iter_tags()
            .filter_map(|t| Some((t.single_letter_tag()?, t.content()?)))
        {
            idx.entry(single_letter_tag).or_default().insert(content);
        }

        // Match
        self.generic_tags.iter().all(|(tag_name, set)| {
            if let Some(val_set) = idx.get(tag_name) {
                set.iter().any(|t| val_set.contains(t))
            } else {
                false
            }
        })
    }

    fn kind_match(&self, event: &Event) -> bool {
        self.kinds.as_ref().map_or(true, |kinds| {
            kinds.is_empty() || kinds.contains(&event.kind)
        })
    }

    /// Determine if [Filter] match given [Event].
    ///
    /// The `search` filed is not supported yet!
    #[inline]
    pub fn match_event(&self, event: &Event) -> bool {
        self.ids_match(event)
            && self.authors_match(event)
            && self.kind_match(event)
            && self.since.map_or(true, |t| event.created_at >= t)
            && self.until.map_or(true, |t| event.created_at <= t)
            && self.tag_match(event)
    }
}
