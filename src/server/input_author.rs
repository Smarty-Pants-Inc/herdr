//! Conservative input evidence, independent of terminal parsing and presentation.
//!
//! An Enter is NOT an editor boundary. A query consumes only freshness, never
//! cumulative source evidence: API, mixed-client and uncertain-input taint lasts
//! for the terminal's lifetime. This prevents a query for turn A (or a same-pane
//! descendant) from laundering API text already queued for draft B. It deliberately
//! cannot attest a clean human turn after API input, history recall, stale drafts,
//! or ambiguous editing without a future synchronized consumer/editor protocol.

use std::collections::HashMap;
use std::time::{Duration, Instant};

#[cfg(test)]
use crate::api::schema::PaneInputAuthorCaller as InputAuthorCaller;
pub(crate) use crate::api::schema::{
    PaneInputAuthor as InputAuthorResult, PaneInputAuthorClient as InputAuthorClient,
    PaneInputAuthorPrincipal as InputAuthorPrincipal, PaneInputAuthorSource as InputAuthorSource,
};
use crate::terminal::TerminalId;

const MAX_TERMINALS: usize = 4096;
const INPUT_EVIDENCE_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct TerminalEvidence {
    source: Option<InputAuthorSource>,
    last_input: Option<Instant>,
    fresh: bool,
    /// Sticky: expiry cannot create an empty-and-clean editor baseline.
    blocked_reason: Option<&'static str>,
    enter_candidates: u8,
}

/// One constant-space aggregate per stable terminal. No unbounded FIFO, payload
/// storage, filesystem/process work or render-loop access.
#[derive(Debug, Default)]
pub(crate) struct InputAuthorTracker {
    terminals: HashMap<TerminalId, TerminalEvidence>,
    /// Permanent fail-closed state: an untracked API receipt must not later be
    /// forgotten just because dead tracked terminals were pruned.
    saturated: bool,
}

impl InputAuthorTracker {
    pub(crate) fn record(&mut self, terminal_id: TerminalId, source: InputAuthorSource) {
        self.record_at(terminal_id, source, Instant::now());
    }

    fn record_at(&mut self, terminal_id: TerminalId, source: InputAuthorSource, now: Instant) {
        // Never evict old taint to make a previously dirty terminal look clean.
        if self.saturated {
            return;
        }
        if self.terminals.len() >= MAX_TERMINALS && !self.terminals.contains_key(&terminal_id) {
            self.saturated = true;
            return;
        }
        let evidence = self.terminals.entry(terminal_id).or_default();
        if evidence.fresh
            && evidence
                .last_input
                .is_some_and(|last| now.saturating_duration_since(last) > INPUT_EVIDENCE_TTL)
        {
            evidence.blocked_reason = Some("stale_draft");
        }
        evidence.source = Some(match evidence.source.take() {
            Some(previous) => merge(previous, source),
            None => source,
        });
        evidence.last_input = Some(now);
        evidence.fresh = true;
    }

    /// Multiple Enter candidates without a consumer observation are ambiguous,
    /// not several trusted editor submissions. This does not reset draft sources.
    pub(crate) fn note_enter(&mut self, terminal_id: &TerminalId) {
        if let Some(evidence) = self.terminals.get_mut(terminal_id) {
            evidence.enter_candidates = evidence.enter_candidates.saturating_add(1);
            if evidence.enter_candidates > 1 {
                evidence.source = Some(InputAuthorSource::Api { caller: None });
            }
        }
    }

    pub(crate) fn take(&mut self, terminal_id: &TerminalId) -> InputAuthorResult {
        self.take_at(terminal_id, Instant::now())
    }

    fn take_at(&mut self, terminal_id: &TerminalId, now: Instant) -> InputAuthorResult {
        if self.saturated {
            return unknown("evidence_capacity_exceeded");
        }
        let Some(evidence) = self.terminals.get_mut(terminal_id) else {
            return unknown("no_evidence");
        };
        if !evidence.fresh {
            return unknown("no_new_evidence");
        }
        evidence.fresh = false;
        // This is only a candidate counter, not a source/reset boundary. All
        // contributors remain in `source`, including input for a newer draft.
        evidence.enter_candidates = 0;
        if evidence
            .last_input
            .is_some_and(|last| now.saturating_duration_since(last) > INPUT_EVIDENCE_TTL)
        {
            evidence.blocked_reason = Some("stale_draft");
        }
        if let Some(reason) = evidence.blocked_reason {
            return unknown(reason);
        }
        InputAuthorResult {
            v: 1,
            source: evidence
                .source
                .clone()
                .unwrap_or_else(|| unknown("no_evidence").source),
        }
    }

    /// Safe only for terminals actually removed from the live registry. Never
    /// invoke on Enter, query, disconnect, process replacement or identity change.
    pub(crate) fn retain_live(&mut self, mut is_live: impl FnMut(&TerminalId) -> bool) {
        self.terminals.retain(|terminal, _| is_live(terminal));
    }
}

fn unknown(reason: &str) -> InputAuthorResult {
    InputAuthorResult {
        v: 1,
        source: InputAuthorSource::Unknown {
            reason: reason.to_owned(),
        },
    }
}

fn merge(previous: InputAuthorSource, next: InputAuthorSource) -> InputAuthorSource {
    match (previous, next) {
        (InputAuthorSource::Client { client: a }, InputAuthorSource::Client { client: b })
            if a == b =>
        {
            InputAuthorSource::Client { client: a }
        }
        (InputAuthorSource::Api { caller: a }, InputAuthorSource::Api { caller: b }) if a == b => {
            InputAuthorSource::Api { caller: a }
        }
        (InputAuthorSource::Api { caller }, InputAuthorSource::Client { .. })
        | (InputAuthorSource::Client { .. }, InputAuthorSource::Api { caller }) => {
            InputAuthorSource::Api { caller }
        }
        // Different connections are mixed even when mapped to the same name.
        // Unknown synthetic editor input is API/uncertainty, never human.
        _ => InputAuthorSource::Api { caller: None },
    }
}

/// Only accept-time resolver facts can populate principal. Display `hello.user`
/// and shared local UID are never principal mappings.
pub(crate) fn client_receipt(
    client_id: u64,
    identity: Option<&crate::server::client_identity::ClientIdentity>,
) -> InputAuthorSource {
    let (peer_pid, uid, principal) = identity.map_or((None, None, None), |identity| {
        (
            identity.peer_pid,
            identity.uid,
            identity
                .principal
                .as_ref()
                .map(|principal| InputAuthorPrincipal {
                    id: principal.id.clone(),
                    name: principal.name.clone(),
                    binding: "herdr-client".to_owned(),
                }),
        )
    });
    InputAuthorSource::Client {
        client: InputAuthorClient {
            client_id,
            peer_pid,
            uid,
            principal,
        },
    }
}

/// Classify editor-affecting ambiguity without interpreting a submit boundary.
/// History/external editor/control sequences may import older, unattested text.
/// Printable single-line input and simple deletion are source contributions;
/// they never subtract an earlier API author from the draft.
pub(crate) fn semantic_input_is_uncertain(event: &crate::protocol::ClientPaneInputEvent) -> bool {
    use crate::protocol::{
        ClientKeyCode as Code, ClientKeyKind as Kind, ClientPaneInputEvent as Event,
    };
    match event {
        Event::TextCommit(text) | Event::Paste(text) => text.chars().any(char::is_control),
        Event::Key {
            kind: Kind::Release,
            ..
        } => false,
        Event::Key {
            code,
            modifiers,
            repeat_count,
            generated_text,
            ..
        } => {
            let text_controls = generated_text
                .as_ref()
                .is_some_and(|text| text.chars().any(char::is_control));
            text_controls
                || match code {
                    Code::Enter => *modifiers != 0 || *repeat_count > 1,
                    Code::Char(_) => *modifiers & !1 != 0,
                    Code::Backspace | Code::Delete | Code::Left | Code::Right => *modifiers != 0,
                    // Up/Down/history, completion, Escape and editing commands have
                    // no proven empty-baseline semantics in the generic server.
                    _ => true,
                }
        }
        Event::Mouse { .. } => true,
    }
}

/// Raw framing is not a trusted editor parser. Any ESC/invalid UTF-8/control
/// ambiguity (including fragmented paste/protocol input) taints permanently;
/// the original bytes still reach the terminal unchanged.
pub(crate) fn raw_input_is_uncertain(data: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(data) else {
        return true;
    };
    let enters = data.iter().filter(|&&byte| byte == b'\r').count();
    enters > 1
        || text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\r' | '\u{8}' | '\u{7f}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> TerminalId {
        serde_json::from_str("\"attestation-terminal\"").expect("test terminal")
    }
    fn client(id: u64, principal: Option<&str>) -> InputAuthorSource {
        InputAuthorSource::Client {
            client: InputAuthorClient {
                client_id: id,
                peer_pid: Some(id as u32),
                uid: Some(1000),
                principal: principal.map(|id| InputAuthorPrincipal {
                    id: id.to_owned(),
                    name: id.to_owned(),
                    binding: "herdr-client".to_owned(),
                }),
            },
        }
    }
    fn api(pane: Option<&str>) -> InputAuthorSource {
        InputAuthorSource::Api {
            caller: pane.map(|pane| InputAuthorCaller {
                pid: Some(77),
                pane: Some(pane.to_owned()),
                agent: None,
            }),
        }
    }
    #[test]
    fn mapped_client_is_returned_with_principal() {
        let mut tracker = InputAuthorTracker::default();
        tracker.record(terminal(), client(1, Some("paul")));
        assert!(matches!(tracker.take(&terminal()).source,
            InputAuthorSource::Client { client } if client.principal.as_ref().map(|p| p.id.as_str()) == Some("paul")));
    }
    #[test]
    fn local_client_is_client_source_without_principal() {
        let mut tracker = InputAuthorTracker::default();
        tracker.record(terminal(), client(1, None));
        assert!(matches!(tracker.take(&terminal()).source,
            InputAuthorSource::Client { client } if client.principal.is_none()));
    }
    #[test]
    fn api_after_client_enter_is_api_not_human() {
        let mut tracker = InputAuthorTracker::default();
        tracker.record(terminal(), client(1, Some("paul")));
        tracker.record(terminal(), api(Some("w1:p2")));
        assert!(matches!(tracker.take(&terminal()).source,
            InputAuthorSource::Api { caller: Some(caller) } if caller.pane.as_deref() == Some("w1:p2")));
    }
    #[test]
    fn different_clients_are_fail_closed_as_api_source() {
        let mut tracker = InputAuthorTracker::default();
        tracker.record(terminal(), client(1, Some("paul")));
        tracker.record(terminal(), client(2, Some("kate")));
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Api { caller: None }
        ));
    }
    #[test]
    fn repeated_query_does_not_replay_or_consume_future_evidence() {
        let mut tracker = InputAuthorTracker::default();
        tracker.record(terminal(), client(1, Some("paul")));
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Client { .. }
        ));
        assert!(matches!(tracker.take(&terminal()).source,
            InputAuthorSource::Unknown { reason } if reason == "no_new_evidence"));
        tracker.record(terminal(), api(None));
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Api { .. }
        ));
    }
    #[test]
    fn query_of_turn_a_cannot_launder_api_text_already_queued_for_turn_b() {
        let mut tracker = InputAuthorTracker::default();
        tracker.record(terminal(), client(1, Some("paul")));
        tracker.record(terminal(), api(Some("w1:p2")));
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Api { .. }
        ));
        tracker.record(terminal(), client(1, Some("paul")));
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Api { .. }
        ));
    }
    #[test]
    fn same_pane_descendant_cannot_consume_api_draft_into_a_human_turn() {
        let mut tracker = InputAuthorTracker::default();
        tracker.record(terminal(), api(None));
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Api { .. }
        ));
        tracker.record(terminal(), client(1, Some("paul")));
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Api { .. }
        ));
    }
    #[test]
    fn evidence_is_constant_space_for_many_receipts() {
        let mut tracker = InputAuthorTracker::default();
        for _ in 0..100_000 {
            tracker.record(terminal(), client(1, Some("paul")));
        }
        assert_eq!(tracker.terminals.len(), 1);
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Client { .. }
        ));
    }
    #[test]
    fn capacity_loss_cannot_promote_an_untracked_api_draft_after_pruning() {
        let mut tracker = InputAuthorTracker::default();
        for _ in 0..MAX_TERMINALS {
            tracker.record(TerminalId::alloc(), client(1, Some("paul")));
        }
        tracker.record(terminal(), api(None));
        tracker.retain_live(|_| false);
        tracker.record(terminal(), client(1, Some("paul")));
        assert!(matches!(tracker.take(&terminal()).source,
            InputAuthorSource::Unknown { reason } if reason == "evidence_capacity_exceeded"));
        assert!(tracker.terminals.is_empty());
    }

    #[test]
    fn stale_evidence_never_becomes_clean_after_new_client_input() {
        let mut tracker = InputAuthorTracker::default();
        let now = Instant::now();
        tracker.record_at(terminal(), client(1, Some("paul")), now);
        let later = now + INPUT_EVIDENCE_TTL + Duration::from_secs(1);
        assert!(matches!(tracker.take_at(&terminal(), later).source,
            InputAuthorSource::Unknown { reason } if reason == "stale_draft"));
        tracker.record_at(terminal(), client(1, Some("paul")), later);
        assert!(matches!(
            tracker.take_at(&terminal(), later).source,
            InputAuthorSource::Unknown { .. }
        ));
    }
    #[test]
    fn multiple_enters_are_not_a_pending_author_fifo() {
        let mut tracker = InputAuthorTracker::default();
        tracker.record(terminal(), client(1, Some("paul")));
        tracker.note_enter(&terminal());
        tracker.note_enter(&terminal());
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Api { .. }
        ));
        tracker.record(terminal(), client(1, Some("paul")));
        assert!(matches!(
            tracker.take(&terminal()).source,
            InputAuthorSource::Api { .. }
        ));
    }
    #[test]
    fn multiline_paste_and_fragmented_raw_control_are_uncertain() {
        assert!(semantic_input_is_uncertain(
            &crate::protocol::ClientPaneInputEvent::Paste("a\nb".into())
        ));
        assert!(raw_input_is_uncertain(b"\x1b[20"));
        assert!(raw_input_is_uncertain(b"x\r\r"));
        assert!(!raw_input_is_uncertain(b"hello\r"));
    }
}
