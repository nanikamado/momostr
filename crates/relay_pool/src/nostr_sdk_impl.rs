use crate::{NostrEvent, NostrFilter};
use nostr::event::EventId;
use std::borrow::Borrow;

impl NostrEvent for nostr::Event {
    type EventId = EventId;

    fn verify(&self) -> bool {
        self.verify().is_ok()
    }

    fn id_as_bytes(&self) -> &[u8; 32] {
        self.id.as_bytes()
    }

    fn id(&self) -> &Self::EventId {
        &self.id
    }
}

impl<T: Borrow<nostr::Event>> NostrFilter<T> for nostr::Filter {
    fn match_event(&self, event: &T) -> bool {
        self.match_event(event.borrow())
    }
}
