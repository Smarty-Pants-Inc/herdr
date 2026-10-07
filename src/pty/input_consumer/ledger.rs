//! Bounded memory-only successful-write receipts and immutable cut replays.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
const BUDGET: usize = 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(600);
const EXPIRY: Duration = Duration::from_secs(30);
struct Span {
    bytes: Vec<u8>,
    source: InputSource,
    written: Instant,
}
struct Replay {
    request: CutRequest,
    result: CutResult,
}
pub(crate) struct Ledger {
    pub(crate) epoch: String,
    key: String,
    previous: u64,
    written: u64,
    seq: u64,
    spans: VecDeque<Span>,
    replays: Vec<Replay>,
    raw_cost: usize,
    replay_cost: usize,
    poison: Option<String>,
}
#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;

fn source_cost(source: &InputSource) -> usize {
    256 + match source {
        InputSource::Client {
            principal: Some(p), ..
        } => p.smarty_id.len() + p.display_name.len(),
        _ => 0,
    }
}
fn unknown(reason: &str) -> CutResult {
    CutResult::Unknown {
        reason: reason.into(),
    }
}
impl Ledger {
    pub(crate) fn new(epoch: String, key: String) -> Self {
        Self {
            epoch,
            key,
            previous: 0,
            written: 0,
            seq: 0,
            spans: VecDeque::new(),
            replays: Vec::new(),
            raw_cost: 0,
            replay_cost: 0,
            poison: None,
        }
    }
    pub(crate) fn authentic(&self, epoch: &str, key: &str) -> bool {
        // Fixed-size keys; do not early-exit on a mismatching key byte.
        let different = self
            .key
            .as_bytes()
            .iter()
            .zip(key.as_bytes())
            .fold(0u8, |d, (a, b)| d | (a ^ b));
        epoch == self.epoch && key.len() == self.key.len() && different == 0
    }
    pub(crate) fn poisoned(&self) -> bool {
        self.poison.is_some()
    }
    pub(crate) fn poison(&mut self, reason: &str) {
        if self.poison.is_none() || reason == "termios_changed" {
            self.poison = Some(reason.into());
        }
        self.spans.clear();
        self.raw_cost = 0;
    }
    pub(crate) fn expire(&mut self, now: Instant) {
        if self
            .spans
            .front()
            .is_some_and(|s| now.saturating_duration_since(s.written) > RETENTION)
        {
            self.poison("receipt_expired");
        }
    }
    pub(crate) fn record(&mut self, bytes: &[u8], source: &InputSource, now: Instant) {
        self.written = self.written.saturating_add(bytes.len() as u64);
        if self.poison.is_some() || bytes.is_empty() {
            return;
        }
        if self
            .spans
            .front()
            .is_some_and(|s| now.saturating_duration_since(s.written) > RETENTION)
        {
            self.poison("receipt_expired");
            return;
        }
        let cost = bytes.len() + source_cost(source);
        if self.raw_cost.saturating_add(cost) > BUDGET {
            self.poison("receipt_overflow");
            return;
        }
        if self
            .raw_cost
            .saturating_add(self.replay_cost)
            .saturating_add(cost)
            > BUDGET
        {
            self.poison("replay_overflow");
            return;
        }
        self.raw_cost += cost;
        self.spans.push_back(Span {
            bytes: bytes.to_vec(),
            source: source.clone(),
            written: now,
        });
    }
    pub(crate) fn cut(
        &mut self,
        req: CutRequest,
        audit: Option<&AuditSink>,
        now: Instant,
    ) -> ConsumerResponse {
        if !self.authentic(&req.epoch, &req.epoch_key) {
            return ConsumerResponse::Refused {
                reason: "invalid_epoch".into(),
            };
        }
        if let Some(old) = self.replays.iter().find(|r| r.request.token == req.token) {
            let mut comparison = req.clone();
            comparison.epoch_key.clear();
            if old.request != comparison {
                return ConsumerResponse::Refused {
                    reason: "token_conflict".into(),
                };
            }
            return ConsumerResponse::Cut(if let Some(reason) = &self.poison {
                unknown(reason)
            } else {
                old.result.clone()
            });
        }
        if req.seq != self.seq.saturating_add(1) {
            return self.refuse_logged(&req, "seq_mismatch", audit);
        }
        if let Some(reason) = self.poison.clone() {
            return self.refuse_logged(&req, &reason, audit);
        }
        let mut replay_cost = 512usize
            .saturating_add(req.epoch.len())
            .saturating_add(req.token.len())
            .saturating_add(req.digest.len());
        if self
            .replay_cost
            .saturating_add(self.raw_cost)
            .saturating_add(replay_cost)
            > BUDGET
        {
            self.poison("replay_overflow");
            return self.refuse_logged(&req, "replay_overflow", audit);
        }
        let mut result = if req.cut < self.previous || req.cut > self.written {
            unknown("cut_beyond_written")
        } else {
            self.interval(&req, now)
        };
        if let CutResult::Unknown { reason } = &result {
            // Unknown source is an honest classification, not stream desync.
            if reason != "unknown_input" && reason != "empty_interval" {
                self.poison(reason);
            }
        }
        if let CutResult::Client { principal: Some(p) } = &result {
            replay_cost = replay_cost
                .saturating_add(p.smarty_id.len())
                .saturating_add(p.display_name.len());
        }
        if self
            .replay_cost
            .saturating_add(self.raw_cost)
            .saturating_add(replay_cost)
            > BUDGET
        {
            self.poison("replay_overflow");
            return self.refuse_logged(&req, "replay_overflow", audit);
        }
        if !Self::log(&req, &result, audit) {
            result = unknown("input_log_unavailable");
            self.poison("input_log_unavailable");
            self.poison = Some("input_log_unavailable".into());
        }
        if self.poison.is_none() {
            self.consume((req.cut - self.previous) as usize);
            self.previous = req.cut;
        }
        self.seq = req.seq;
        // Never retain the bearer key in replay records.
        let mut stored = req;
        stored.epoch_key.clear();
        self.replay_cost += replay_cost;
        self.replays.push(Replay {
            request: stored,
            result: result.clone(),
        });
        ConsumerResponse::Cut(result)
    }
    fn log(req: &CutRequest, result: &CutResult, audit: Option<&AuditSink>) -> bool {
        let record = AuditRecord {
            epoch: req.epoch.clone(),
            seq: req.seq,
            token: req.token.clone(),
            cut: req.cut,
            digest: req.digest.clone(),
            kind: req.kind,
            result: result.clone(),
        };
        audit.is_some_and(|sink| sink(&record).is_ok())
    }
    /// Every returned cut result is durably logged first, including early
    /// `unknown` answers that are not stored as replay records. A failed log
    /// still fails closed: `input_log_unavailable` and poison.
    fn refuse_logged(
        &mut self,
        req: &CutRequest,
        reason: &str,
        audit: Option<&AuditSink>,
    ) -> ConsumerResponse {
        if Self::log(req, &unknown(reason), audit) {
            ConsumerResponse::Cut(unknown(reason))
        } else {
            self.poison("input_log_unavailable");
            self.poison = Some("input_log_unavailable".into());
            ConsumerResponse::Cut(unknown("input_log_unavailable"))
        }
    }
    fn interval(&self, req: &CutRequest, now: Instant) -> CutResult {
        let mut left = (req.cut - self.previous) as usize;
        let mut hash = Sha256::new();
        let mut client: Option<(u64, Option<Principal>)> = None;
        let mut api = false;
        let mut mixed = false;
        let mut untrusted = false;
        let mut endpoint = None;
        for span in &self.spans {
            if left == 0 {
                break;
            }
            let n = left.min(span.bytes.len());
            left -= n;
            hash.update(&span.bytes[..n]);
            endpoint = Some(span.written);
            if now.saturating_duration_since(span.written) > RETENTION {
                return unknown("receipt_expired");
            }
            match &span.source {
                InputSource::Client {
                    connection_id,
                    principal,
                } => {
                    if client
                        .as_ref()
                        .is_some_and(|(id, p)| id != connection_id || p != principal)
                    {
                        mixed = true;
                    } else {
                        client = Some((*connection_id, principal.clone()));
                    }
                }
                InputSource::Api => api = true,
                InputSource::Unknown => untrusted = true,
                InputSource::Neutral => {}
            }
        }
        if left != 0 {
            return unknown("cut_beyond_written");
        }
        if format!("{:x}", hash.finalize()) != req.digest {
            return unknown("digest_mismatch");
        }
        if endpoint.is_some_and(|t| now.saturating_duration_since(t) > EXPIRY) {
            return unknown("cut_expired");
        }
        if untrusted {
            unknown("unknown_input")
        } else if mixed || (api && client.is_some()) {
            CutResult::Mixed
        } else if api {
            CutResult::Api
        } else if let Some((_, principal)) = client {
            CutResult::Client { principal }
        } else {
            unknown("empty_interval")
        }
    }
    fn consume(&mut self, mut count: usize) {
        while count > 0 {
            let Some(mut span) = self.spans.pop_front() else {
                break;
            };
            if count < span.bytes.len() {
                span.bytes.drain(..count);
                self.raw_cost -= count;
                self.spans.push_front(span);
                break;
            }
            count -= span.bytes.len();
            self.raw_cost -= span.bytes.len() + source_cost(&span.source);
        }
    }
}
