use std::collections::{HashMap, HashSet};

use crate::app::{InputSourceId, TerminalInputContext, TerminalInputTarget};
use crate::input::{KeyIdentity, TerminalKey};

const MAX_INPUT_LEASES_PER_SOURCE: usize = 256;
const MAX_INPUT_LEASES_TOTAL: usize = 4096;
pub(crate) const MAX_INPUT_SOURCES: usize = 4097;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct InputLeaseKey {
    source_id: InputSourceId,
    identity: KeyIdentity,
}

impl InputLeaseKey {
    pub(crate) fn new(source_id: InputSourceId, key: &TerminalKey) -> Self {
        Self {
            source_id,
            identity: key.identity(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ForwardedInputLease {
    pub(crate) target: TerminalInputTarget,
    pub(crate) key: TerminalKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConsumedInputLease {
    OmpReplyNavigation(TerminalInputContext),
    ReprocessRepeats(TerminalInputContext),
    SuppressRepeats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ConsumedRepeatPolicy {
    #[default]
    StableContext,
    ResultingContext,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputLease {
    Forwarded(ForwardedInputLease),
    Consumed(ConsumedInputLease),
}

pub(crate) enum RepeatPlan {
    Forwarded(TerminalInputTarget),
    Reprocess {
        context: TerminalInputContext,
        repetitions: u16,
        tracked: bool,
    },
    OmpReplyNavigation(TerminalInputContext),
    Ignore,
}

#[derive(Default)]
pub(crate) struct InputLeaseTable {
    leases: HashMap<InputLeaseKey, InputLease>,
    overflowed_sources: HashSet<InputSourceId>,
}

impl InputLeaseTable {
    pub(crate) fn source_overflowed(&self, source_id: InputSourceId) -> bool {
        self.overflowed_sources.contains(&source_id)
    }

    pub(crate) fn source_suppressed(&self, source_id: InputSourceId) -> bool {
        self.source_overflowed(source_id) || self.overflowed_sources.len() >= MAX_INPUT_SOURCES
    }

    pub(crate) fn suppress_key(&self, lease_key: &InputLeaseKey) -> bool {
        self.source_overflowed(lease_key.source_id)
            || (self.overflowed_sources.len() >= MAX_INPUT_SOURCES
                && !self.leases.contains_key(lease_key))
    }

    pub(crate) fn prepare_press(
        &mut self,
        lease_key: &InputLeaseKey,
    ) -> Option<Vec<ForwardedInputLease>> {
        if self.leases.contains_key(lease_key) {
            return None;
        }
        if self.source_suppressed(lease_key.source_id) {
            return Some(Vec::new());
        }
        let source_len = self
            .leases
            .keys()
            .filter(|key| key.source_id == lease_key.source_id)
            .count();
        if source_len < MAX_INPUT_LEASES_PER_SOURCE && self.leases.len() < MAX_INPUT_LEASES_TOTAL {
            return None;
        }
        let releases = self.remove_source_leases(lease_key.source_id);
        let inserted = self.overflowed_sources.insert(lease_key.source_id);
        debug_assert!(inserted && self.overflowed_sources.len() <= MAX_INPUT_SOURCES);
        Some(releases)
    }

    fn insertion_allowed(&self, key: &InputLeaseKey) -> bool {
        self.leases.contains_key(key)
            || (!self.source_suppressed(key.source_id)
                && self.leases.len() < MAX_INPUT_LEASES_TOTAL
                && self
                    .leases
                    .keys()
                    .filter(|existing| existing.source_id == key.source_id)
                    .count()
                    < MAX_INPUT_LEASES_PER_SOURCE)
    }
    pub(crate) fn normalize_press(
        &mut self,
        lease_key: &InputLeaseKey,
        key: TerminalKey,
    ) -> TerminalKey {
        // Generated text is normally stateless, but a native physical key still owns
        // press/repeat/release lifecycle even when it carries a layout result.
        if key.kind != crossterm::event::KeyEventKind::Press
            || (key.generated_text.is_some() && !key.has_physical_identity())
        {
            return key;
        }
        if key.has_physical_identity() && self.leases.contains_key(lease_key) {
            key.with_kind(crossterm::event::KeyEventKind::Repeat)
        } else {
            self.leases.remove(lease_key);
            key
        }
    }

    pub(crate) fn complete_press(
        &mut self,
        lease_key: InputLeaseKey,
        key: &TerminalKey,
        initial_context: Option<&TerminalInputContext>,
        resulting_context: Option<&TerminalInputContext>,
        consumed_repeat_policy: ConsumedRepeatPolicy,
        target: Option<TerminalInputTarget>,
        handled_omp_reply_navigation: bool,
    ) -> RepeatPlan {
        if key.generated_text.is_some() && !key.has_physical_identity() {
            return RepeatPlan::Ignore;
        }
        if let Some(target) = target {
            self.insert_forwarded(lease_key, target, key.clone());
            return RepeatPlan::Ignore;
        }
        if !self.leases.contains_key(&lease_key) {
            let disposition = if handled_omp_reply_navigation {
                match (initial_context, resulting_context) {
                    (Some(initial), Some(resulting)) if initial == resulting => {
                        ConsumedInputLease::OmpReplyNavigation(initial.clone())
                    }
                    _ => ConsumedInputLease::SuppressRepeats,
                }
            } else {
                match (consumed_repeat_policy, initial_context, resulting_context) {
                    (ConsumedRepeatPolicy::ResultingContext, _, Some(resulting)) => {
                        ConsumedInputLease::ReprocessRepeats(resulting.clone())
                    }
                    (ConsumedRepeatPolicy::StableContext, Some(initial), Some(resulting))
                        if initial == resulting =>
                    {
                        ConsumedInputLease::ReprocessRepeats(initial.clone())
                    }
                    _ => ConsumedInputLease::SuppressRepeats,
                }
            };
            self.insert_consumed(lease_key, disposition);
        }
        match self.leases.get(&lease_key) {
            Some(InputLease::Consumed(ConsumedInputLease::ReprocessRepeats(context)))
                if key.repeat_count > 1 && !handled_omp_reply_navigation =>
            {
                RepeatPlan::Reprocess {
                    context: context.clone(),
                    repetitions: key.repeat_count - 1,
                    tracked: true,
                }
            }
            _ => RepeatPlan::Ignore,
        }
    }

    pub(crate) fn plan_repeat(
        &mut self,
        lease_key: InputLeaseKey,
        key: &TerminalKey,
        current_context: Option<&TerminalInputContext>,
    ) -> RepeatPlan {
        match self.leases.get(&lease_key) {
            Some(InputLease::Forwarded(lease)) => RepeatPlan::Forwarded(lease.target.clone()),
            Some(InputLease::Consumed(ConsumedInputLease::OmpReplyNavigation(context)))
                if current_context == Some(context) =>
            {
                RepeatPlan::OmpReplyNavigation(context.clone())
            }
            Some(InputLease::Consumed(ConsumedInputLease::OmpReplyNavigation(_))) => {
                self.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);
                RepeatPlan::Ignore
            }
            Some(InputLease::Consumed(ConsumedInputLease::ReprocessRepeats(context)))
                if current_context == Some(context) =>
            {
                RepeatPlan::Reprocess {
                    context: context.clone(),
                    repetitions: key.repeat_count,
                    tracked: true,
                }
            }
            Some(InputLease::Consumed(ConsumedInputLease::ReprocessRepeats(_))) => {
                self.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);
                RepeatPlan::Ignore
            }
            Some(InputLease::Consumed(ConsumedInputLease::SuppressRepeats)) => RepeatPlan::Ignore,
            None => match current_context {
                Some(context) => RepeatPlan::Reprocess {
                    context: context.clone(),
                    repetitions: key.repeat_count,
                    tracked: false,
                },
                None => RepeatPlan::Ignore,
            },
        }
    }

    pub(crate) fn reprocess_allowed(
        &mut self,
        lease_key: InputLeaseKey,
        expected_context: &TerminalInputContext,
        current_context: Option<&TerminalInputContext>,
        tracked: bool,
    ) -> bool {
        let allowed = current_context == Some(expected_context);
        if tracked && !allowed {
            self.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);
        }
        allowed
    }

    pub(crate) fn remove_forwarded(&mut self, key: &InputLeaseKey) -> Option<ForwardedInputLease> {
        match self.leases.remove(key) {
            Some(InputLease::Forwarded(lease)) => Some(lease),
            Some(InputLease::Consumed(_)) | None => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, key: &InputLeaseKey) -> bool {
        self.leases.contains_key(key)
    }

    pub(crate) fn owns_existing_lifecycle(
        &mut self,
        lease_key: &InputLeaseKey,
        key: &TerminalKey,
    ) -> bool {
        if self.source_overflowed(lease_key.source_id) {
            return true;
        }
        if key.kind == crossterm::event::KeyEventKind::Press && !key.has_physical_identity() {
            self.leases.remove(lease_key);
            return false;
        }
        self.leases.contains_key(lease_key)
    }

    pub(crate) fn insert_forwarded(
        &mut self,
        key: InputLeaseKey,
        target: TerminalInputTarget,
        original: TerminalKey,
    ) -> bool {
        if !self.insertion_allowed(&key) {
            return false;
        }
        self.leases.insert(
            key,
            InputLease::Forwarded(ForwardedInputLease {
                target,
                key: original,
            }),
        );
        true
    }

    pub(crate) fn insert_consumed(
        &mut self,
        key: InputLeaseKey,
        disposition: ConsumedInputLease,
    ) -> bool {
        if !self.insertion_allowed(&key) {
            return false;
        }
        self.leases.insert(key, InputLease::Consumed(disposition));
        true
    }

    pub(crate) fn remove(&mut self, key: &InputLeaseKey) -> Option<InputLease> {
        self.leases.remove(key)
    }

    pub(crate) fn remove_source(&mut self, source_id: InputSourceId) -> Vec<ForwardedInputLease> {
        self.overflowed_sources.remove(&source_id);
        self.remove_source_leases(source_id)
    }

    fn remove_source_leases(&mut self, source_id: InputSourceId) -> Vec<ForwardedInputLease> {
        let keys = self
            .leases
            .keys()
            .filter(|key| key.source_id == source_id)
            .copied()
            .collect::<Vec<_>>();
        self.remove_keys(keys)
    }

    pub(crate) fn remove_target(
        &mut self,
        target: &TerminalInputTarget,
    ) -> Vec<ForwardedInputLease> {
        let keys = self
            .leases
            .iter()
            .filter_map(|(key, lease)| match lease {
                InputLease::Forwarded(lease) if &lease.target == target => Some(*key),
                InputLease::Forwarded(_) | InputLease::Consumed(_) => None,
            })
            .collect::<Vec<_>>();
        let mut releases = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(InputLease::Forwarded(lease)) = self.leases.remove(&key) {
                releases.push(lease);
                self.leases.insert(
                    key,
                    InputLease::Consumed(ConsumedInputLease::SuppressRepeats),
                );
            }
        }
        releases
    }

    fn remove_keys(
        &mut self,
        keys: impl IntoIterator<Item = InputLeaseKey>,
    ) -> Vec<ForwardedInputLease> {
        keys.into_iter()
            .filter_map(|key| match self.leases.remove(&key) {
                Some(InputLease::Forwarded(lease)) => Some(lease),
                Some(InputLease::Consumed(_)) | None => None,
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.leases.is_empty() && self.overflowed_sources.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.leases.len()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    fn target() -> TerminalInputTarget {
        TerminalInputTarget {
            terminal_id: crate::terminal::TerminalId::alloc(),
        }
    }

    fn pane_context() -> TerminalInputContext {
        TerminalInputContext::Pane(crate::terminal::TerminalId::alloc())
    }

    fn physical_generated_slash(repeat_count: u16) -> TerminalKey {
        TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()))
            .with_windows_record(crate::input::WindowsKeyRecord {
                key_down: true,
                repeat_count,
                virtual_key_code: 0x37,
                virtual_scan_code: 0x08,
                unicode: u16::from(b'/'),
                control_key_state: 0x0010,
            })
    }

    fn physical_key(scan_code: u16) -> TerminalKey {
        TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()).with_windows_record(
            crate::input::WindowsKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key_code: 0x58,
                virtual_scan_code: scan_code,
                unicode: u16::from(b'x'),
                control_key_state: 0,
            },
        )
    }
    #[test]
    fn remove_source_returns_forwarded_and_discards_consumed_leases() {
        let key = TerminalKey::new(KeyCode::Esc, KeyModifiers::empty());
        let forwarded = InputLeaseKey::new(7, &key);
        let consumed = InputLeaseKey::new(
            7,
            &TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        let other_source = InputLeaseKey::new(8, &key);
        let first_target = target();
        let mut leases = InputLeaseTable::default();
        leases.insert_forwarded(forwarded, first_target.clone(), key.clone());
        leases.insert_consumed(consumed, ConsumedInputLease::SuppressRepeats);
        leases.insert_forwarded(other_source, target(), key.clone());

        assert_eq!(
            leases.remove_source(7),
            vec![ForwardedInputLease {
                target: first_target,
                key,
            }]
        );
        assert_eq!(leases.len(), 1);
        assert!(leases.contains(&other_source));
    }

    #[test]
    fn remove_target_releases_forwarded_input_and_suppresses_until_physical_release() {
        let key = physical_key(30);
        let removed_key = InputLeaseKey::new(7, &key);
        let retained_key = InputLeaseKey::new(8, &key);
        let removed_target = target();
        let retained_target = target();
        let mut leases = InputLeaseTable::default();
        leases.insert_forwarded(removed_key, removed_target.clone(), key.clone());
        leases.insert_forwarded(retained_key, retained_target, key.clone());

        assert_eq!(
            leases.remove_target(&removed_target),
            vec![ForwardedInputLease {
                target: removed_target,
                key: key.clone(),
            }]
        );
        assert_eq!(leases.len(), 2);
        assert!(leases.contains(&removed_key));
        assert!(leases.contains(&retained_key));
        let repeated = leases.normalize_press(&removed_key, key);
        assert_eq!(repeated.kind, crossterm::event::KeyEventKind::Repeat);
        assert!(matches!(
            leases.plan_repeat(removed_key, &repeated, None),
            RepeatPlan::Ignore
        ));
        assert!(leases.remove_forwarded(&removed_key).is_none());
        assert!(!leases.contains(&removed_key));
    }

    #[test]
    fn duplicate_physical_press_normalizes_for_forwarded_and_consumed_leases() {
        let record = crate::input::WindowsKeyRecord {
            key_down: true,
            repeat_count: 1,
            virtual_key_code: 65,
            virtual_scan_code: 30,
            unicode: 97,
            control_key_state: 0,
        };
        let physical =
            TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty()).with_windows_record(record);
        let lease_key = InputLeaseKey::new(7, &physical);
        let mut leases = InputLeaseTable::default();

        assert_eq!(
            leases.normalize_press(&lease_key, physical.clone()).kind,
            crossterm::event::KeyEventKind::Press
        );
        leases.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);
        assert_eq!(
            leases.normalize_press(&lease_key, physical.clone()).kind,
            crossterm::event::KeyEventKind::Repeat
        );
        leases.insert_forwarded(lease_key, target(), physical.clone());
        assert_eq!(
            leases.normalize_press(&lease_key, physical.clone()).kind,
            crossterm::event::KeyEventKind::Repeat
        );
    }

    #[test]
    fn physical_generated_text_keeps_native_repeat_lifecycle() {
        let key = physical_generated_slash(3);
        let lease_key = InputLeaseKey::new(7, &key);
        let context = pane_context();
        let forwarded_target = target();
        let mut leases = InputLeaseTable::default();

        assert_eq!(
            leases.normalize_press(&lease_key, key.clone()).kind,
            crossterm::event::KeyEventKind::Press
        );
        assert!(matches!(
            leases.complete_press(
                lease_key,
                &key,
                Some(&context),
                Some(&context),
                ConsumedRepeatPolicy::StableContext,
                Some(forwarded_target.clone()),
                false,
            ),
            RepeatPlan::Ignore
        ));
        assert!(leases.contains(&lease_key));

        let repeated = key.with_repeat_count(1);
        let repeated = leases.normalize_press(&lease_key, repeated);
        assert_eq!(repeated.kind, crossterm::event::KeyEventKind::Repeat);
        assert!(matches!(
            leases.plan_repeat(lease_key, &repeated, Some(&context)),
            RepeatPlan::Forwarded(target) if target == forwarded_target
        ));
        assert!(leases.remove_forwarded(&lease_key).is_some());
    }

    #[test]
    fn consumed_grouped_physical_generated_text_reprocesses_repeats() {
        let key = physical_generated_slash(3);
        let lease_key = InputLeaseKey::new(7, &key);
        let context = pane_context();
        let mut leases = InputLeaseTable::default();

        assert!(matches!(
            leases.complete_press(
                lease_key,
                &key,
                Some(&context),
                Some(&context),
                ConsumedRepeatPolicy::StableContext,
                None,
                false,
            ),
            RepeatPlan::Reprocess {
                context: TerminalInputContext::Pane(_),
                repetitions: 2,
                tracked: true,
            }
        ));
    }

    #[test]
    fn consumed_repeat_is_suppressed_after_pane_terminal_replacement() {
        let key = physical_generated_slash(1);
        let lease_key = InputLeaseKey::new(7, &key);
        let original = pane_context();
        let replacement = pane_context();
        let mut leases = InputLeaseTable::default();

        assert!(matches!(
            leases.complete_press(
                lease_key,
                &key,
                Some(&original),
                Some(&original),
                ConsumedRepeatPolicy::StableContext,
                None,
                false
            ),
            RepeatPlan::Ignore
        ));
        let repeated = leases.normalize_press(&lease_key, key.with_repeat_count(1));
        assert_eq!(repeated.kind, crossterm::event::KeyEventKind::Repeat);
        assert!(matches!(
            leases.plan_repeat(lease_key, &repeated, Some(&replacement)),
            RepeatPlan::Ignore
        ));
        assert!(matches!(
            leases.plan_repeat(lease_key, &repeated, Some(&replacement)),
            RepeatPlan::Ignore
        ));
    }

    #[test]
    fn handled_physical_press_uses_owned_omp_navigation_repeat_plan() {
        let first = physical_generated_slash(3);
        let lease_key = InputLeaseKey::new(7, &first);
        let context = pane_context();
        let mut leases = InputLeaseTable::default();

        assert!(matches!(
            leases.complete_press(
                lease_key,
                &first,
                Some(&context),
                Some(&context),
                ConsumedRepeatPolicy::StableContext,
                None,
                true,
            ),
            RepeatPlan::Ignore
        ));
        assert!(leases.contains(&lease_key));

        let repeated = leases.normalize_press(&lease_key, physical_generated_slash(u16::MAX));
        assert_eq!(repeated.kind, crossterm::event::KeyEventKind::Repeat);
        assert_eq!(repeated.repeat_count, u16::MAX);
        assert!(matches!(
            leases.plan_repeat(lease_key, &repeated, Some(&context)),
            RepeatPlan::OmpReplyNavigation(TerminalInputContext::Pane(_))
        ));
    }

    #[test]
    fn semantic_generated_text_remains_untracked() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()))
            .with_repeat_count(3);
        let lease_key = InputLeaseKey::new(7, &key);
        let context = pane_context();
        let mut leases = InputLeaseTable::default();

        assert!(matches!(
            leases.complete_press(
                lease_key,
                &key,
                Some(&context),
                Some(&context),
                ConsumedRepeatPolicy::StableContext,
                Some(target()),
                false,
            ),
            RepeatPlan::Ignore
        ));
        assert!(leases.is_empty());
    }

    #[test]
    fn new_semantic_press_recomputes_consumed_repeat_disposition() {
        let key = TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()).with_repeat_count(3);
        let lease_key = InputLeaseKey::new(7, &key);
        let context = pane_context();
        let mut leases = InputLeaseTable::default();
        leases.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);

        let key = leases.normalize_press(&lease_key, key);
        let plan = leases.complete_press(
            lease_key,
            &key,
            Some(&context),
            Some(&context),
            ConsumedRepeatPolicy::StableContext,
            None,
            false,
        );
        assert!(matches!(
            plan,
            RepeatPlan::Reprocess {
                context: TerminalInputContext::Pane(_),
                repetitions: 2,
                tracked: true,
            }
        ));
    }

    #[test]
    fn physical_and_semantic_identities_do_not_collide() {
        let record = crate::input::WindowsKeyRecord {
            key_down: true,
            repeat_count: 1,
            virtual_key_code: 65,
            virtual_scan_code: 30,
            unicode: 97,
            control_key_state: 0,
        };
        let physical =
            TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty()).with_windows_record(record);
        let semantic = TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty());

        assert_ne!(
            InputLeaseKey::new(7, &physical),
            InputLeaseKey::new(7, &semantic)
        );
    }
    #[test]
    fn per_source_overflow_releases_forwarded_leases_and_quarantines_until_reset() {
        let source_id = 7;
        let target = target();
        let mut leases = InputLeaseTable::default();
        for scan_code in 1..=MAX_INPUT_LEASES_PER_SOURCE as u16 {
            let key = physical_key(scan_code);
            let lease_key = InputLeaseKey::new(source_id, &key);
            assert!(leases.prepare_press(&lease_key).is_none());
            assert!(leases.insert_forwarded(lease_key, target.clone(), key));
        }

        let overflow_key = physical_key(MAX_INPUT_LEASES_PER_SOURCE as u16 + 1);
        let releases = leases
            .prepare_press(&InputLeaseKey::new(source_id, &overflow_key))
            .expect("source should overflow");
        assert_eq!(releases.len(), MAX_INPUT_LEASES_PER_SOURCE);
        assert_eq!(leases.len(), 0);
        assert!(leases.source_overflowed(source_id));
        assert!(leases
            .prepare_press(&InputLeaseKey::new(source_id, &overflow_key))
            .is_some());

        assert!(leases.remove_source(source_id).is_empty());
        assert!(!leases.source_overflowed(source_id));
        assert!(leases
            .prepare_press(&InputLeaseKey::new(source_id, &overflow_key))
            .is_none());
    }

    #[test]
    fn global_lease_bound_quarantines_only_the_overflowing_source() {
        let mut leases = InputLeaseTable::default();
        for source_id in 0..(MAX_INPUT_LEASES_TOTAL / MAX_INPUT_LEASES_PER_SOURCE) as u64 {
            for scan_code in 1..=MAX_INPUT_LEASES_PER_SOURCE as u16 {
                let key = physical_key(scan_code);
                assert!(leases.insert_consumed(
                    InputLeaseKey::new(source_id, &key),
                    ConsumedInputLease::SuppressRepeats,
                ));
            }
        }
        let tracked_key = physical_key(1);
        let tracked_lease = InputLeaseKey::new(0, &tracked_key);
        let tracked_target = target();
        assert!(leases.insert_forwarded(
            tracked_lease,
            tracked_target.clone(),
            tracked_key.clone(),
        ));
        assert_eq!(leases.len(), MAX_INPUT_LEASES_TOTAL);

        let overflow_source = 99;
        let overflow_key = physical_key(1);
        assert_eq!(
            leases.prepare_press(&InputLeaseKey::new(overflow_source, &overflow_key)),
            Some(Vec::new())
        );
        assert!(leases.source_overflowed(overflow_source));
        assert!(!leases.source_overflowed(0));
        assert!(matches!(
            leases.plan_repeat(tracked_lease, &tracked_key, None),
            RepeatPlan::Forwarded(target) if target == tracked_target
        ));
        assert!(leases.remove_forwarded(&tracked_lease).is_some());

        assert!(leases.remove_source(overflow_source).is_empty());
        assert!(!leases.source_overflowed(overflow_source));
        assert!(leases
            .prepare_press(&InputLeaseKey::new(overflow_source, &overflow_key))
            .is_none());
    }

    #[test]
    fn source_quarantine_registry_covers_every_admitted_source_until_its_own_reset() {
        let mut leases = InputLeaseTable::default();
        for source_id in 0..MAX_INPUT_SOURCES as u64 {
            leases.overflowed_sources.insert(source_id);
        }
        let source_id = MAX_INPUT_SOURCES as u64 - 1;
        let unrelated_source = 0;
        let key = physical_key(1);
        let lease_key = InputLeaseKey::new(source_id, &key);
        assert_eq!(leases.overflowed_sources.len(), MAX_INPUT_SOURCES);
        assert!(leases.suppress_key(&lease_key));
        assert!(leases.source_suppressed(source_id));

        leases.remove_source(unrelated_source);
        assert!(leases.suppress_key(&lease_key));
        assert!(leases.source_suppressed(source_id));

        leases.remove_source(source_id);
        assert!(!leases.source_suppressed(source_id));
        let context = pane_context();
        assert!(matches!(
            leases.plan_repeat(
                lease_key,
                &key.with_kind(crossterm::event::KeyEventKind::Repeat),
                Some(&context),
            ),
            RepeatPlan::Reprocess {
                context: planned,
                repetitions: 1,
                tracked: false,
            } if planned == context
        ));
    }
}
