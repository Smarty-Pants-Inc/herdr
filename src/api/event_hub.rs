#[derive(Clone, Default)]
pub struct EventHub {
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
    media: std::sync::Arc<std::sync::Mutex<MediaEventHubState>>,
    media_interest_notify: std::sync::Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    events: Vec<(u64, crate::api::schema::EventEnvelope)>,
}

type MediaHistoryEntry = (
    u64,
    crate::layout::PaneId,
    crate::api::schema::SubscriptionEventEnvelope,
);

#[derive(Default)]
struct MediaEventHubState {
    next_sequence: u64,
    evicted_through: u64,
    // The raw producer identity is private; the public envelope is unchanged.
    events: std::collections::VecDeque<MediaHistoryEntry>,
    interest: std::collections::HashMap<crate::layout::PaneId, MediaPaneInterest>,
    dirty: std::collections::HashSet<crate::layout::PaneId>,
    enabled: std::collections::HashSet<crate::layout::PaneId>,
}

struct MediaPaneInterest {
    count: usize,
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Captured together under App authority and the media-state lock. Owning the
/// reply already owns interest, even before the socket worker receives it.
pub(crate) struct MediaSubscriptionBinding {
    hub: EventHub,
    pub(crate) pane_id: crate::layout::PaneId,
    pub(crate) start_sequence: u64,
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MediaSubscriptionBinding {
    pub(crate) fn is_ended(&self) -> bool {
        !self.live.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for MediaSubscriptionBinding {
    fn drop(&mut self) {
        let Ok(mut state) = self.hub.media.lock() else {
            return;
        };
        let Some(interest) = state.interest.get_mut(&self.pane_id) else {
            return;
        };
        if !std::sync::Arc::ptr_eq(&interest.live, &self.live) {
            return;
        }
        interest.count -= 1;
        if interest.count == 0 {
            self.live.store(false, std::sync::atomic::Ordering::Release);
            state.interest.remove(&self.pane_id);
            state.dirty.insert(self.pane_id);
            drop(state);
            self.hub.media_interest_notify.notify_one();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EventHistoryError {
    Lost,
    Unavailable,
}

impl EventHub {
    const MAX_EVENTS: usize = 512;

    pub fn push(&self, event: crate::api::schema::EventEnvelope) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state.events.push((sequence, event));
        let overflow = state.events.len().saturating_sub(Self::MAX_EVENTS);
        if overflow > 0 {
            state.events.drain(0..overflow);
        }
    }

    pub fn events_after(&self, sequence: u64) -> Vec<(u64, crate::api::schema::EventEnvelope)> {
        let Ok(state) = self.inner.lock() else {
            return Vec::new();
        };
        state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect()
    }

    pub(super) fn events_after_checked(
        &self,
        sequence: u64,
    ) -> Result<Vec<(u64, crate::api::schema::EventEnvelope)>, EventHistoryError> {
        let state = self
            .inner
            .lock()
            .map_err(|_| EventHistoryError::Unavailable)?;
        if state
            .events
            .first()
            .is_some_and(|(first, _)| sequence < first.saturating_sub(1))
        {
            return Err(EventHistoryError::Lost);
        }
        Ok(state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect())
    }

    /// A separate bounded history: media envelopes are deliberately not the
    /// tagged general EventEnvelope and levels must not evict lifecycle events.
    pub(crate) fn push_media(
        &self,
        pane_id: crate::layout::PaneId,
        event: crate::api::schema::SubscriptionEventEnvelope,
    ) {
        let Ok(mut state) = self.media.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state.events.push_back((sequence, pane_id, event));
        if state.events.len() > Self::MAX_EVENTS {
            if let Some((sequence, _, _)) = state.events.pop_front() {
                state.evicted_through = sequence;
            }
        }
    }

    pub(super) fn current_media_sequence(&self) -> u64 {
        self.media.lock().map_or(0, |state| state.next_sequence)
    }

    pub(super) fn media_events_after_checked(
        &self,
        binding: &MediaSubscriptionBinding,
        sequence: u64,
    ) -> Result<Option<Vec<MediaHistoryEntry>>, EventHistoryError> {
        // An ended entry wins over retention loss, including loss that happened
        // before its close. Both decisions share the lifetime/interest lock.
        if binding.is_ended() {
            return Ok(None);
        }
        let state = self
            .media
            .lock()
            .map_err(|_| EventHistoryError::Unavailable)?;
        if binding.is_ended() {
            return Ok(None);
        }
        if sequence < state.evicted_through {
            return Err(EventHistoryError::Lost);
        }
        Ok(Some(
            state
                .events
                .iter()
                .filter(|(seq, _, _)| *seq > sequence)
                .cloned()
                .collect(),
        ))
    }

    /// Only App bootstrap may call this in production: resolution and acquiring
    /// this RAII binding must be one App-owned operation, not two worker steps.
    pub(crate) fn subscribe_media(
        &self,
        pane_id: crate::layout::PaneId,
    ) -> Result<MediaSubscriptionBinding, crate::api::schema::ErrorBody> {
        let mut state = self
            .media
            .lock()
            .map_err(|_| crate::api::schema::ErrorBody {
                close_code: None,
                code: "server_unavailable".into(),
                message: "event history is unavailable".into(),
            })?;
        let interest = state
            .interest
            .entry(pane_id)
            .or_insert_with(|| MediaPaneInterest {
                count: 0,
                live: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            });
        interest.count += 1;
        let live = interest.live.clone();
        let first = interest.count == 1;
        let start_sequence = state.next_sequence;
        if first {
            state.dirty.insert(pane_id);
            self.media_interest_notify.notify_one();
        }
        Ok(MediaSubscriptionBinding {
            hub: self.clone(),
            pane_id,
            start_sequence,
            live,
        })
    }

    /// Called only at App topology boundaries. Laziness gives zero pane walks
    /// when there are no bindings; otherwise one O(P+B) pass, not B pane scans.
    pub(crate) fn retain_live_media_panes(
        &self,
        live_panes: impl FnOnce() -> std::collections::HashSet<crate::layout::PaneId>,
    ) {
        let Ok(mut state) = self.media.lock() else {
            return;
        };
        if state.interest.is_empty() {
            return;
        }
        let live_panes = live_panes();
        let mut ended = std::collections::HashSet::new();
        state.interest.retain(|pane, interest| {
            if live_panes.contains(pane) {
                return true;
            }
            interest
                .live
                .store(false, std::sync::atomic::Ordering::Release);
            ended.insert(*pane);
            false
        });
        if ended.is_empty() {
            return;
        }
        // Drop queued history atomically with ending the lifetime and interest.
        // Removing records is not overflow and must not manufacture history loss.
        state.events.retain(|(_, pane, _)| !ended.contains(pane));
        state.dirty.extend(ended);
        drop(state);
        self.media_interest_notify.notify_one();
    }

    pub(crate) fn media_interest_notify(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.media_interest_notify.clone()
    }

    pub(crate) fn take_media_interest_transitions(&self) -> Vec<(crate::layout::PaneId, bool)> {
        let Ok(mut state) = self.media.lock() else {
            return Vec::new();
        };
        let mut transitions = Vec::new();
        // Reconcile the final desired state, coalescing close-before-enable and
        // repeated first/last changes without ever resolving a public alias.
        let dirty = std::mem::take(&mut state.dirty);
        for pane in dirty {
            let enabled = state.interest.contains_key(&pane);
            if enabled && state.enabled.insert(pane) {
                transitions.push((pane, true));
            } else if !enabled && state.enabled.remove(&pane) {
                transitions.push((pane, false));
            }
        }
        transitions
    }

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventData, EventEnvelope, EventKind};

    fn event() -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: EventData::WorkspaceFocused {
                workspace_id: "workspace_1".into(),
            },
        }
    }

    #[test]
    fn checked_history_distinguishes_retained_boundary_from_lost_events() {
        let hub = EventHub::default();
        assert!(hub.events_after_checked(0).unwrap().is_empty());
        for _ in 0..EventHub::MAX_EVENTS {
            hub.push(event());
        }
        assert_eq!(
            hub.events_after_checked(0).unwrap().len(),
            EventHub::MAX_EVENTS
        );
        hub.push(event());
        assert_eq!(hub.events_after_checked(0), Err(EventHistoryError::Lost));
        let retained = hub.events_after_checked(1).unwrap();
        assert_eq!(retained.len(), EventHub::MAX_EVENTS);
        assert_eq!(retained.first().unwrap().0, 2);
        assert_eq!(retained.last().unwrap().0, hub.current_sequence());
        assert!(hub
            .events_after_checked(hub.current_sequence())
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn media_interest_counts_first_last_and_wakes_without_runtime_polling() {
        let hub = EventHub::default();
        let pane = crate::layout::PaneId::from_raw(91);
        let notify = hub.media_interest_notify();
        let first = hub.subscribe_media(pane).unwrap();
        let second = hub.subscribe_media(pane).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), notify.notified())
            .await
            .unwrap();
        assert_eq!(hub.take_media_interest_transitions(), vec![(pane, true)]);
        drop(first);
        assert!(hub.take_media_interest_transitions().is_empty());
        // A move/renumber does not change the raw pane's interest.
        let moved = hub.subscribe_media(pane).unwrap();
        assert!(hub.take_media_interest_transitions().is_empty());
        drop(second);
        assert!(hub.take_media_interest_transitions().is_empty());
        drop(moved);
        tokio::time::timeout(std::time::Duration::from_secs(1), notify.notified())
            .await
            .unwrap();
        assert_eq!(hub.take_media_interest_transitions(), vec![(pane, false)]);
        let state = hub.media.lock().unwrap();
        assert!(state.interest.is_empty());
        assert!(state.dirty.is_empty());
        assert!(state.enabled.is_empty());
    }

    #[test]
    fn media_history_is_bounded_independent_and_reports_loss_and_poison() {
        use crate::api::schema::{
            MediaLevelsEvent, SubscriptionEventData, SubscriptionEventEnvelope,
            SubscriptionEventKind,
        };
        let hub = EventHub::default();
        let pane = crate::layout::PaneId::from_raw(92);
        let binding = hub.subscribe_media(pane).unwrap();
        hub.push(event());
        for _ in 0..=EventHub::MAX_EVENTS {
            hub.push_media(
                pane,
                SubscriptionEventEnvelope {
                    event: SubscriptionEventKind::MediaLevels,
                    data: SubscriptionEventData::MediaLevels(MediaLevelsEvent {
                        pane_id: "w1:p1".into(),
                        session_id: "session".into(),
                        input_peak: 0.25,
                        output_peak: 0.5,
                    }),
                },
            );
        }
        assert_eq!(hub.events_after_checked(0).unwrap().len(), 1);
        assert_eq!(hub.current_sequence(), 1);
        assert_eq!(
            hub.media_events_after_checked(&binding, 0),
            Err(EventHistoryError::Lost)
        );
        assert_eq!(
            hub.media_events_after_checked(&binding, 1)
                .unwrap()
                .unwrap()
                .len(),
            EventHub::MAX_EVENTS
        );
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = hub.media.lock().unwrap();
            panic!("poison media history");
        }))
        .is_err());
        assert_eq!(
            hub.media_events_after_checked(&binding, 1),
            Err(EventHistoryError::Unavailable)
        );
    }

    fn levels(pane_id: &str) -> crate::api::schema::SubscriptionEventEnvelope {
        serde_json::from_value(serde_json::json!({
            "event": "media.levels", "data": {
                "pane_id": pane_id, "session_id": "session", "input_peak": 0.25, "output_peak": 0.5
            }
        }))
        .unwrap()
    }

    // Trust boundary: alias reuse, queued enables, and bootstrap/close races
    // must never activate or deliver media for another pane incarnation.
    #[test]
    fn media_binding_captures_cursor_before_reply_and_close_wins_over_loss() {
        let hub = EventHub::default();
        let old = crate::layout::PaneId::from_raw(93);
        let replacement = crate::layout::PaneId::from_raw(94);
        hub.push_media(old, levels("w1:p1"));
        let binding = hub.subscribe_media(replacement).unwrap();
        assert_eq!(binding.start_sequence, 1);
        assert!(hub
            .media_events_after_checked(&binding, binding.start_sequence)
            .unwrap()
            .unwrap()
            .is_empty());
        // Capture is already registered, even while a typed reply is queued.
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(tx.send(binding).is_ok());
        for _ in 0..=EventHub::MAX_EVENTS {
            hub.push_media(replacement, levels("w1:p1"));
        }
        hub.retain_live_media_panes(std::collections::HashSet::new);
        let binding = rx.recv().unwrap();
        assert!(binding.is_ended());
        assert_eq!(hub.media_events_after_checked(&binding, 0).unwrap(), None);
        assert!(hub.take_media_interest_transitions().is_empty());
        assert!(hub
            .media
            .lock()
            .unwrap()
            .events
            .iter()
            .all(|(_, pane, _)| *pane != replacement));
    }

    #[test]
    fn abandoned_typed_reply_releases_interest_before_runtime_enable() {
        let hub = EventHub::default();
        let pane = crate::layout::PaneId::from_raw(97);
        let (tx, rx) = std::sync::mpsc::channel::<
            Result<MediaSubscriptionBinding, crate::api::schema::ErrorBody>,
        >();
        let binding = hub.subscribe_media(pane).unwrap();
        drop(rx);
        let failed = tx.send(Ok(binding)).expect_err("worker is gone");
        drop(failed);
        assert!(hub.media.lock().unwrap().interest.is_empty());
        assert!(hub.take_media_interest_transitions().is_empty());
    }

    #[test]
    fn media_close_coalesces_interest_and_preserves_unrelated_history() {
        let hub = EventHub::default();
        hub.retain_live_media_panes(|| panic!("no bindings must not walk panes"));
        let old = crate::layout::PaneId::from_raw(95);
        let new = crate::layout::PaneId::from_raw(96);
        let old_binding = hub.subscribe_media(old).unwrap();
        let old_duplicate = hub.subscribe_media(old).unwrap();
        assert_eq!(hub.take_media_interest_transitions(), vec![(old, true)]);
        let new_binding = hub.subscribe_media(new).unwrap();
        hub.push_media(old, levels("same-public-id"));
        hub.push_media(new, levels("same-public-id"));
        hub.retain_live_media_panes(|| [new].into_iter().collect());
        assert!(old_binding.is_ended());
        assert!(old_duplicate.is_ended());
        assert!(!new_binding.is_ended());
        let transitions = hub.take_media_interest_transitions();
        assert_eq!(transitions.len(), 2);
        assert!(transitions.contains(&(old, false)));
        assert!(transitions.contains(&(new, true)));
        let history = hub
            .media_events_after_checked(&new_binding, 0)
            .unwrap()
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1, new);
        drop(old_binding);
        drop(old_duplicate);
        assert!(hub.take_media_interest_transitions().is_empty());
        // Coalesce repeated last-drop/new-interest transitions for the same live
        // incarnation without disabling another subscriber's levels.
        drop(new_binding);
        let again = hub.subscribe_media(new).unwrap();
        assert!(hub.take_media_interest_transitions().is_empty());
        drop(again);
        assert_eq!(hub.take_media_interest_transitions(), vec![(new, false)]);
        let unopened = hub.subscribe_media(old).unwrap();
        drop(unopened);
        assert!(hub.take_media_interest_transitions().is_empty());
    }

    #[test]
    fn checked_history_reports_unavailable_instead_of_empty_after_poison() {
        let hub = EventHub::default();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = hub.inner.lock().unwrap();
            panic!("poison the test event history");
        }))
        .is_err());
        assert_eq!(
            hub.events_after_checked(0),
            Err(EventHistoryError::Unavailable)
        );
    }
}
