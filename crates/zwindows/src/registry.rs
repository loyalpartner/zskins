//! Pure in-memory state aggregation for wlr-foreign-toplevel events.
//!
//! Kept free of Wayland types so the de-dup / lifecycle logic is unit-testable
//! without spinning up a compositor. The Wayland dispatcher in `client` owns a
//! `Mutex<Registry>` and calls these methods from the event loop thread.

use std::collections::HashMap;

use crate::{Toplevel, ToplevelEvent};

#[derive(Default)]
pub(crate) struct Registry {
    inner: HashMap<u64, Entry>,
}

struct Entry {
    toplevel: Toplevel,
    // `published` flips to true on the first field update after `ensure`.
    // Until then we hold Added back — the wlr protocol creates a handle first
    // and fills fields asynchronously; surfacing an empty Toplevel as Added
    // would force every consumer to filter noise.
    published: bool,
}

impl Registry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reserve a slot for a freshly-announced handle. No event emitted — the
    /// first populated field will upgrade this slot to Added.
    pub(crate) fn ensure(&mut self, id: u64) {
        self.inner.entry(id).or_insert_with(|| Entry {
            toplevel: Toplevel {
                id,
                app_id: None,
                title: None,
                activated: false,
                minimized: false,
            },
            published: false,
        });
    }

    pub(crate) fn on_app_id(&mut self, id: u64, app_id: String) -> Option<ToplevelEvent> {
        let entry = self.inner.get_mut(&id)?;
        if entry.toplevel.app_id.as_deref() == Some(app_id.as_str()) {
            return None;
        }
        entry.toplevel.app_id = Some(app_id);
        Some(Self::publish(entry))
    }

    pub(crate) fn on_title(&mut self, id: u64, title: String) -> Option<ToplevelEvent> {
        let entry = self.inner.get_mut(&id)?;
        if entry.toplevel.title.as_deref() == Some(title.as_str()) {
            return None;
        }
        entry.toplevel.title = Some(title);
        Some(Self::publish(entry))
    }

    pub(crate) fn on_state(
        &mut self,
        id: u64,
        activated: bool,
        minimized: bool,
    ) -> Option<ToplevelEvent> {
        let entry = self.inner.get_mut(&id)?;
        if entry.published
            && entry.toplevel.activated == activated
            && entry.toplevel.minimized == minimized
        {
            return None;
        }
        entry.toplevel.activated = activated;
        entry.toplevel.minimized = minimized;
        Some(Self::publish(entry))
    }

    pub(crate) fn on_closed(&mut self, id: u64) -> Option<ToplevelEvent> {
        self.inner.remove(&id).map(|_| ToplevelEvent::Removed(id))
    }

    fn publish(entry: &mut Entry) -> ToplevelEvent {
        if entry.published {
            ToplevelEvent::Updated(entry.toplevel.clone())
        } else {
            entry.published = true;
            ToplevelEvent::Added(entry.toplevel.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registry {
        Registry::new()
    }

    #[test]
    fn ensure_alone_emits_nothing() {
        let mut r = reg();
        r.ensure(1);
        // No public method on Registry to "poll" — the contract is that
        // on_* methods are the only event sources. Smoke-test via a follow-up.
        let ev = r.on_title(1, "t".into()).unwrap();
        assert!(matches!(ev, ToplevelEvent::Added(_)));
    }

    #[test]
    fn first_field_update_emits_added() {
        let mut r = reg();
        r.ensure(42);
        let ev = r.on_app_id(42, "firefox".into()).unwrap();
        match ev {
            ToplevelEvent::Added(t) => {
                assert_eq!(t.id, 42);
                assert_eq!(t.app_id.as_deref(), Some("firefox"));
            }
            _ => panic!("expected Added, got {ev:?}"),
        }
    }

    #[test]
    fn subsequent_update_emits_updated() {
        let mut r = reg();
        r.ensure(7);
        let _ = r.on_app_id(7, "a".into()); // Added
        let ev = r.on_title(7, "hello".into()).unwrap();
        assert!(matches!(ev, ToplevelEvent::Updated(_)));
    }

    #[test]
    fn duplicate_title_returns_none() {
        let mut r = reg();
        r.ensure(1);
        let _ = r.on_title(1, "foo".into());
        assert!(r.on_title(1, "foo".into()).is_none());
    }

    #[test]
    fn duplicate_app_id_returns_none() {
        let mut r = reg();
        r.ensure(1);
        let _ = r.on_app_id(1, "x".into());
        assert!(r.on_app_id(1, "x".into()).is_none());
    }

    #[test]
    fn state_change_emits_updated_with_flags() {
        let mut r = reg();
        r.ensure(3);
        let _ = r.on_app_id(3, "a".into()); // publish
        let ev = r.on_state(3, true, false).unwrap();
        match ev {
            ToplevelEvent::Updated(t) => {
                assert!(t.activated);
                assert!(!t.minimized);
            }
            _ => panic!("expected Updated, got {ev:?}"),
        }
    }

    #[test]
    fn repeated_identical_state_returns_none() {
        let mut r = reg();
        r.ensure(3);
        let _ = r.on_app_id(3, "a".into());
        let _ = r.on_state(3, true, false); // first time: Updated
        assert!(r.on_state(3, true, false).is_none());
    }

    #[test]
    fn first_state_before_any_field_emits_added() {
        let mut r = reg();
        r.ensure(9);
        let ev = r.on_state(9, false, false).unwrap();
        assert!(matches!(ev, ToplevelEvent::Added(_)));
    }

    #[test]
    fn closed_emits_removed() {
        let mut r = reg();
        r.ensure(5);
        let _ = r.on_app_id(5, "a".into());
        let ev = r.on_closed(5).unwrap();
        assert!(matches!(ev, ToplevelEvent::Removed(5)));
    }

    #[test]
    fn closed_twice_second_is_none() {
        let mut r = reg();
        r.ensure(5);
        let _ = r.on_closed(5);
        assert!(r.on_closed(5).is_none());
    }

    #[test]
    fn unknown_id_title_rejected() {
        let mut r = reg();
        assert!(r.on_title(999, "x".into()).is_none());
    }

    #[test]
    fn unknown_id_state_rejected() {
        let mut r = reg();
        assert!(r.on_state(999, true, false).is_none());
    }

    #[test]
    fn unknown_id_app_id_rejected() {
        let mut r = reg();
        assert!(r.on_app_id(999, "x".into()).is_none());
    }
}
