//! Actor-local input attribution. Secrets and retained input never leave runtime memory.
use serde::{Deserialize, Serialize};
use std::{io, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Principal {
    pub smarty_id: String,
    pub display_name: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum InputSource {
    Client {
        connection_id: u64,
        principal: Option<Principal>,
    },
    Api,
    Neutral,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CutKind {
    Submit,
    Discard,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CutRequest {
    pub epoch: String,
    pub epoch_key: String,
    pub seq: u64,
    pub token: String,
    pub cut: u64,
    pub digest: String,
    pub kind: CutKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub(crate) enum CutResult {
    Client { principal: Option<Principal> },
    Api,
    Mixed,
    Unknown { reason: String },
}
#[derive(Debug, Clone)]
pub(crate) enum ConsumerOperation {
    Enroll {
        peer: crate::platform::ProcessIdentity,
    },
    Cut(CutRequest),
    Release {
        epoch: String,
        epoch_key: String,
    },
}
// No Debug: enrollment response contains a bearer capability.
pub(crate) enum ConsumerResponse {
    Enrolled {
        epoch: String,
        epoch_key: String,
        nonce: String,
    },
    Cut(CutResult),
    Released,
    Refused {
        reason: String,
    },
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditRecord {
    pub epoch: String,
    pub seq: u64,
    pub token: String,
    pub cut: u64,
    pub digest: String,
    pub kind: CutKind,
    pub result: CutResult,
}
pub(crate) type AuditSink = Arc<dyn Fn(&AuditRecord) -> io::Result<()> + Send + Sync + 'static>;

#[cfg(unix)]
mod ledger;
#[cfg(unix)]
mod replies;
#[cfg(unix)]
pub(crate) use ledger::Ledger;
#[cfg(unix)]
pub(crate) use replies::classify_replies;

/// Applied in final stream order, never per origin. Change only the final byte
/// of the reserved introducer, keeping length and each byte's source intact.
/// No lookahead/held bytes means Escape keys cannot stall and partial writes
/// cannot lose provenance.
#[derive(Default)]
pub(crate) struct Sanitizer {
    matched: usize,
}
impl Sanitizer {
    #[cfg(any(unix, test))]
    pub(crate) fn reset(&mut self) {
        self.matched = 0;
    }
    pub(crate) fn sanitize(&mut self, bytes: bytes::Bytes) -> bytes::Bytes {
        const PREFIX: &[u8] = b"\x1b_herdr";
        let mut output = None;
        for (i, &byte) in bytes.iter().enumerate() {
            if byte == PREFIX[self.matched] {
                self.matched += 1;
                if self.matched == PREFIX.len() {
                    output.get_or_insert_with(|| bytes.to_vec())[i] = b'R';
                    self.matched = 0;
                }
            } else {
                self.matched = usize::from(byte == PREFIX[0]);
            }
        }
        output.map(bytes::Bytes::from).unwrap_or(bytes)
    }
}
impl InputSource {
    pub(crate) fn user(self) -> Self {
        if self == Self::Neutral {
            Self::Unknown
        } else {
            self
        }
    }
}

#[cfg(test)]
mod sanitizer_tests {
    use super::*;
    #[test]
    fn input_consumer_sanitizer_escape_immediate_and_every_split() {
        let prefix = b"\x1b_herdr";
        for split in 0..=prefix.len() {
            let mut sanitizer = Sanitizer::default();
            let first = sanitizer.sanitize(bytes::Bytes::copy_from_slice(&prefix[..split]));
            assert_eq!(first.len(), split, "no held bytes");
            let last = sanitizer.sanitize(bytes::Bytes::copy_from_slice(&prefix[split..]));
            assert_eq!([first.as_ref(), last.as_ref()].concat(), b"\x1b_herdR");
        }
        let mut sanitizer = Sanitizer::default();
        assert_eq!(
            sanitizer.sanitize(bytes::Bytes::from_static(b"\x1b")),
            b"\x1b".as_slice()
        );
        assert_eq!(
            sanitizer.sanitize(bytes::Bytes::from_static(b"[A")),
            b"[A".as_slice()
        );
        let benign = bytes::Bytes::from_static(b"abc\x1b_hello\x1b[?25;1$y");
        assert_eq!(sanitizer.sanitize(benign.clone()), benign);
        assert_eq!(InputSource::Neutral.user(), InputSource::Unknown);
        sanitizer.sanitize(bytes::Bytes::from_static(b"\x1b_he"));
        sanitizer.reset();
        assert_eq!(
            sanitizer.sanitize(bytes::Bytes::from_static(b"rdr")),
            b"rdr".as_slice()
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "input_consumer/tests.rs"]
mod tests;
