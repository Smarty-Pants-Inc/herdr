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

#[derive(Default)]
struct MediaEventHubState {
    next_sequence: u64,
    events: std::collections::VecDeque<(u64, crate::api::schema::SubscriptionEventEnvelope)>,
    interest: std::collections::HashMap<String, usize>,
    transitions: std::collections::VecDeque<(String, bool)>,
    resolved: std::collections::HashMap<String, crate::layout::PaneId>,
    pane_interest: std::collections::HashMap<crate::layout::PaneId, usize>,
}

/// Connection-owned interest: every setup/write/transport failure releases it.
pub(super) struct MediaInterest {
    hub: EventHub,
    pane: String,
}

impl Drop for MediaInterest {
    fn drop(&mut self) {
        let Ok(mut state) = self.hub.media.lock() else {
            return;
        };
        let Some(count) = state.interest.get_mut(&self.pane) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            state.interest.remove(&self.pane);
            state.transitions.push_back((self.pane.clone(), false));
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
    pub(crate) fn push_media(&self, event: crate::api::schema::SubscriptionEventEnvelope) {
        let Ok(mut state) = self.media.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state.events.push_back((sequence, event));
        if state.events.len() > Self::MAX_EVENTS {
            state.events.pop_front();
        }
    }

    pub(super) fn current_media_sequence(&self) -> u64 {
        self.media.lock().map_or(0, |state| state.next_sequence)
    }

    pub(super) fn media_events_after_checked(
        &self,
        sequence: u64,
    ) -> Result<Vec<(u64, crate::api::schema::SubscriptionEventEnvelope)>, EventHistoryError> {
        let state = self
            .media
            .lock()
            .map_err(|_| EventHistoryError::Unavailable)?;
        if state
            .events
            .front()
            .is_some_and(|(first, _)| sequence < first.saturating_sub(1))
        {
            return Err(EventHistoryError::Lost);
        }
        Ok(state
            .events
            .iter()
            .filter(|(seq, _)| *seq > sequence)
            .cloned()
            .collect())
    }

    pub(super) fn subscribe_media(&self, pane: String) -> Result<MediaInterest, EventHistoryError> {
        let mut state = self
            .media
            .lock()
            .map_err(|_| EventHistoryError::Unavailable)?;
        let count = state.interest.entry(pane.clone()).or_default();
        *count += 1;
        if *count == 1 {
            state.transitions.push_back((pane.clone(), true));
            self.media_interest_notify.notify_one();
        }
        Ok(MediaInterest {
            hub: self.clone(),
            pane,
        })
    }

    pub(crate) fn media_interest_notify(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.media_interest_notify.clone()
    }

    pub(crate) fn take_media_interest_transitions(
        &self,
        mut resolve: impl FnMut(&str) -> Option<crate::layout::PaneId>,
    ) -> Vec<(crate::layout::PaneId, bool)> {
        let Ok(mut state) = self.media.lock() else {
            return Vec::new();
        };
        let mut transitions = Vec::new();
        while let Some((canonical, enabled)) = state.transitions.pop_front() {
            if enabled {
                let Some(pane) = resolve(&canonical) else {
                    continue;
                };
                state.resolved.insert(canonical, pane);
                let count = state.pane_interest.entry(pane).or_default();
                *count += 1;
                if *count == 1 {
                    transitions.push((pane, true));
                }
            } else if let Some(pane) = state.resolved.remove(&canonical) {
                if let Some(count) = state.pane_interest.get_mut(&pane) {
                    *count -= 1;
                    if *count == 0 {
                        state.pane_interest.remove(&pane);
                        transitions.push((pane, false));
                    }
                }
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
        let first = hub.subscribe_media("w1:p1".into()).unwrap();
        let second = hub.subscribe_media("w1:p1".into()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), notify.notified())
            .await
            .unwrap();
        assert_eq!(
            hub.take_media_interest_transitions(|_| Some(pane)),
            vec![(pane, true)]
        );
        drop(first);
        assert!(hub
            .take_media_interest_transitions(|_| Some(pane))
            .is_empty());
        // A move changes the canonical id but not the raw pane's interest.
        let moved = hub.subscribe_media("w2:p1".into()).unwrap();
        assert!(hub
            .take_media_interest_transitions(|_| Some(pane))
            .is_empty());
        drop(second);
        assert!(hub.take_media_interest_transitions(|_| None).is_empty());
        drop(moved);
        tokio::time::timeout(std::time::Duration::from_secs(1), notify.notified())
            .await
            .unwrap();
        assert_eq!(
            hub.take_media_interest_transitions(|_| None),
            vec![(pane, false)]
        );
        let state = hub.media.lock().unwrap();
        assert!(state.interest.is_empty());
        assert!(state.resolved.is_empty());
        assert!(state.pane_interest.is_empty());
    }

    #[test]
    fn media_history_is_bounded_independent_and_reports_loss_and_poison() {
        use crate::api::schema::{
            MediaLevelsEvent, SubscriptionEventData, SubscriptionEventEnvelope,
            SubscriptionEventKind,
        };
        let hub = EventHub::default();
        hub.push(event());
        for _ in 0..=EventHub::MAX_EVENTS {
            hub.push_media(SubscriptionEventEnvelope {
                event: SubscriptionEventKind::MediaLevels,
                data: SubscriptionEventData::MediaLevels(MediaLevelsEvent {
                    pane_id: "w1:p1".into(),
                    session_id: "session".into(),
                    input_peak: 0.25,
                    output_peak: 0.5,
                }),
            });
        }
        assert_eq!(hub.events_after_checked(0).unwrap().len(), 1);
        assert_eq!(hub.current_sequence(), 1);
        assert_eq!(
            hub.media_events_after_checked(0),
            Err(EventHistoryError::Lost)
        );
        assert_eq!(
            hub.media_events_after_checked(1).unwrap().len(),
            EventHub::MAX_EVENTS
        );
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = hub.media.lock().unwrap();
            panic!("poison media history");
        }))
        .is_err());
        assert_eq!(
            hub.media_events_after_checked(1),
            Err(EventHistoryError::Unavailable)
        );
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
