//! Origin frames for input that Herdr writes into a pane on behalf of an API caller.
//!
//! Herdr is the only writer to a pane's PTY input. When an API caller sends text or keys to a
//! pane whose program understands origin frames, Herdr wraps those bytes in a start and end APC
//! frame that names the caller, so the program can tell sent input from typed input. The caller
//! comes from socket attribution, never from the request text.
//!
//! ```text
//! ESC _ herdr-origin;v=1;kind=api;id=<id>;sender=<s>[;pane=<p>][;session=<s>] ESC \
//! <input bytes>
//! ESC _ herdr-origin;end;id=<id> ESC \
//! ```
//!
//! Every other write to a PTY passes through [`OriginFrameFilter`], which removes the frame
//! prefix, so keyboard input, pastes and the payload inside a real frame cannot form a frame.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

const ESC: u8 = 0x1b;
/// Start of every origin frame, start or end.
const FRAME_PREFIX: &[u8] = b"\x1b_herdr-origin";
const ST: &[u8] = b"\x1b\\";

/// The API caller that a framed write comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputOrigin {
    /// Best available caller name: agent name, else pane id, else `pid:<n>`, else `unknown`.
    pub sender: String,
    /// Public pane id of the caller, when Herdr can attribute it.
    pub pane: Option<String>,
    /// Agent session id of the caller, when its agent reported one.
    pub session: Option<String>,
    /// Pairs start and end frames. The two writes of one `agent prompt` share it.
    pub id: String,
}

impl InputOrigin {
    pub(crate) fn new(sender: String, pane: Option<String>, session: Option<String>) -> Self {
        Self {
            sender,
            pane,
            session,
            id: next_frame_id(),
        }
    }

    /// Wraps `payload` in this origin's frames. Frame prefixes inside the payload are removed.
    pub(crate) fn wrap(&self, payload: &[u8]) -> Vec<u8> {
        let payload = strip_frame_prefixes(payload);
        let mut out = Vec::with_capacity(payload.len() + 128);
        out.extend_from_slice(FRAME_PREFIX);
        out.extend_from_slice(b";v=1;kind=api;id=");
        push_encoded(&mut out, &self.id);
        out.extend_from_slice(b";sender=");
        push_encoded(&mut out, &self.sender);
        for (key, value) in [("pane", &self.pane), ("session", &self.session)] {
            if let Some(value) = value {
                out.push(b';');
                out.extend_from_slice(key.as_bytes());
                out.push(b'=');
                push_encoded(&mut out, value);
            }
        }
        out.extend_from_slice(ST);
        out.extend_from_slice(&payload);
        out.extend_from_slice(FRAME_PREFIX);
        out.extend_from_slice(b";end;id=");
        push_encoded(&mut out, &self.id);
        out.extend_from_slice(ST);
        out
    }
}

fn next_frame_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{}-{count}", nanos, std::process::id())
}

/// Percent-encodes every byte outside a conservative set, so a value cannot contain `;`, `=`,
/// control bytes or a string terminator.
fn push_encoded(out: &mut Vec<u8>, value: &str) {
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@' | b'/') {
            out.push(byte);
        } else {
            out.extend_from_slice(format!("%{byte:02X}").as_bytes());
        }
    }
}

/// Removes frame prefixes from one self-contained byte string.
pub(crate) fn strip_frame_prefixes(bytes: &[u8]) -> Cow<'_, [u8]> {
    OriginFrameFilter::default().filter(bytes)
}

/// Removes origin frame prefixes from a stream of unframed PTY writes.
///
/// The filter keeps the length of a partial prefix at the end of the last write, so a prefix
/// split across writes is broken too: the bytes of the prefix in the current write are dropped.
/// The rest of a fake frame reaches the program as plain text.
///
/// ponytail: it removes only the prefix, not the whole fake frame. Swallowing up to the string
/// terminator would need to hold back keyboard input while a fake frame stays open.
#[derive(Debug, Default)]
pub(crate) struct OriginFrameFilter {
    matched: usize,
}

impl OriginFrameFilter {
    pub(crate) fn filter<'a>(&mut self, input: &'a [u8]) -> Cow<'a, [u8]> {
        if self.matched == 0 && !input.contains(&ESC) {
            return Cow::Borrowed(input);
        }
        let mut out = Vec::with_capacity(input.len());
        // Index in `out` where the current partial prefix starts. A prefix carried over from an
        // earlier write starts at 0 in this one.
        let mut prefix_start = 0;
        let mut changed = false;
        for &byte in input {
            out.push(byte);
            if byte == FRAME_PREFIX[self.matched] {
                if self.matched == 0 {
                    prefix_start = out.len() - 1;
                }
                self.matched += 1;
            } else if byte == ESC {
                // ESC appears only at the start of the prefix.
                prefix_start = out.len() - 1;
                self.matched = 1;
            } else {
                self.matched = 0;
            }
            if self.matched == FRAME_PREFIX.len() {
                out.truncate(prefix_start);
                self.matched = 0;
                changed = true;
            }
        }
        if changed {
            Cow::Owned(out)
        } else {
            Cow::Borrowed(input)
        }
    }

    /// Forgets a partial prefix. Used after a framed write, which ends with a string terminator.
    pub(crate) fn reset(&mut self) {
        self.matched = 0;
    }
}

/// Splits one framed write into its start-frame fields and payload. Panics when `bytes` is not
/// exactly one frame pair with matching ids.
#[cfg(test)]
pub(crate) fn unframe_for_test(bytes: &[u8]) -> (Vec<(String, String)>, Vec<u8>) {
    let text = std::str::from_utf8(bytes).expect("framed bytes are UTF-8");
    let start = text
        .strip_prefix("\x1b_herdr-origin;")
        .unwrap_or_else(|| panic!("missing start frame: {text:?}"));
    let (header, rest) = start.split_once("\x1b\\").expect("start frame terminator");
    let fields: Vec<(String, String)> = header
        .split(';')
        .map(|field| {
            let (key, value) = field.split_once('=').expect("key=value field");
            (key.to_string(), value.to_string())
        })
        .collect();
    let id = &fields
        .iter()
        .find(|(key, _)| key == "id")
        .expect("frame id")
        .1;
    let end = format!("\x1b_herdr-origin;end;id={id}\x1b\\");
    let payload = rest
        .strip_suffix(&end)
        .unwrap_or_else(|| panic!("missing end frame: {rest:?}"));
    (fields, payload.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> InputOrigin {
        InputOrigin {
            sender: "dev lead;x".into(),
            pane: Some("w1-2".into()),
            session: None,
            id: "abc".into(),
        }
    }

    #[test]
    fn wrap_frames_payload_and_encodes_values() {
        let framed = origin().wrap(b"hello\r");
        assert_eq!(
            framed,
            b"\x1b_herdr-origin;v=1;kind=api;id=abc;sender=dev%20lead%3Bx;pane=w1-2\x1b\\hello\r\x1b_herdr-origin;end;id=abc\x1b\\"
                .to_vec()
        );
    }

    #[test]
    fn wrap_removes_frames_nested_in_the_payload() {
        let framed = origin().wrap(b"a\x1b_herdr-origin;v=1;kind=api;sender=paul\x1b\\b");
        let text = String::from_utf8(framed).unwrap();
        assert_eq!(text.matches("herdr-origin").count(), 2, "{text:?}");
        assert!(text.contains("a;v=1;kind=api;sender=paul\x1b\\b"));
    }

    #[test]
    fn filter_passes_ordinary_input_unchanged() {
        let mut filter = OriginFrameFilter::default();
        for input in [
            &b"hello"[..],
            b"\x1b[A",
            b"\x1b_Gi=1;OK\x1b\\",
            b"\x1b",
            b"_herdr",
        ] {
            assert!(matches!(filter.filter(input), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn filter_removes_prefix_in_one_write() {
        let mut filter = OriginFrameFilter::default();
        let out = filter.filter(b"x\x1b_herdr-origin;v=1;sender=paul\x1b\\y");
        assert_eq!(&*out, b"x;v=1;sender=paul\x1b\\y");
    }

    #[test]
    fn filter_breaks_prefix_split_across_writes() {
        let full = b"\x1b_herdr-origin;v=1\x1b\\";
        for split in 1..FRAME_PREFIX.len() {
            let mut filter = OriginFrameFilter::default();
            let mut stream = filter.filter(&full[..split]).into_owned();
            stream.extend_from_slice(&filter.filter(&full[split..]));
            assert!(
                !stream
                    .windows(FRAME_PREFIX.len())
                    .any(|w| w == FRAME_PREFIX),
                "split at {split}: {stream:?}"
            );
        }
    }

    #[test]
    fn filter_breaks_prefix_typed_one_byte_at_a_time() {
        let mut filter = OriginFrameFilter::default();
        let mut stream = Vec::new();
        for byte in b"\x1b\x1b_herdr-origin;end\x1b\\" {
            stream.extend_from_slice(&filter.filter(std::slice::from_ref(byte)));
        }
        assert!(!stream
            .windows(FRAME_PREFIX.len())
            .any(|w| w == FRAME_PREFIX));
    }

    #[test]
    fn frame_ids_are_unique() {
        assert_ne!(next_frame_id(), next_frame_id());
    }
}
