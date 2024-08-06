use crate::{NostrEvent, NostrFilter};
use nostr_types::{Event, Filter};
use std::borrow::Borrow;

impl NostrEvent for Event {
    fn verify(&self) -> bool {
        self.verify(None).is_ok()
    }

    fn id_as_bytes(&self) -> &[u8; 32] {
        &self.id.0
    }
}

impl<T: Borrow<Event>> NostrFilter<T> for Filter {
    fn match_event(&self, event: &T) -> bool {
        self.event_matches(event.borrow())
    }
}
