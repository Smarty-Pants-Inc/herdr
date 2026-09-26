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
//! Every other write to a PTY passes through [`OriginFrameFilter`], which breaks the frame
//! prefix, so keyboard input, pastes and the payload inside a real frame cannot form a frame.
//! Pi treats the prefix as a sync point, so an open escape sequence or paste in earlier input
//! cannot hide a frame.

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

    /// Wraps `payload` in this origin's frames. Frame prefixes inside the payload are broken.
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

/// Breaks frame prefixes in one self-contained byte string.
pub(crate) fn strip_frame_prefixes(bytes: &[u8]) -> Cow<'_, [u8]> {
    OriginFrameFilter::default().filter(bytes)
}

/// Replaces the last byte of a frame prefix that unframed input would complete.
const BROKEN_PREFIX_BYTE: u8 = b'?';

/// Keeps origin frame prefixes out of a stream of unframed PTY writes.
///
/// The filter tracks how much of a prefix the emitted stream may end with. When a byte would
/// complete a prefix, the filter emits [`BROKEN_PREFIX_BYTE`] instead. It never deletes bytes,
/// because a deletion can join the bytes around it into a new prefix. So the emitted stream
/// never contains the prefix, also across writes. Pi then sees an unknown APC and drops it.
///
/// A queued write can still be dropped before it reaches the PTY (a submission deadline on
/// Windows). So the filter keeps the set of partial-prefix lengths the stream may end with, and
/// breaks a prefix that any of them would complete. [`Self::merge`] adds the states of a write
/// that may or may not land. Callers filter with a copy and commit it only after the write is
/// accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OriginFrameFilter {
    /// Bit `n` set: the stream may end with the first `n` bytes of the prefix.
    possible: u16,
}

const _: () = assert!(FRAME_PREFIX.len() < u16::BITS as usize);

impl Default for OriginFrameFilter {
    fn default() -> Self {
        Self { possible: 1 }
    }
}

impl OriginFrameFilter {
    pub(crate) fn filter<'a>(&mut self, input: &'a [u8]) -> Cow<'a, [u8]> {
        if self.possible == 1 && !input.contains(&ESC) {
            return Cow::Borrowed(input);
        }
        let mut out: Option<Vec<u8>> = None;
        for (index, &byte) in input.iter().enumerate() {
            let mut next = 0u16;
            for (matched, &expected) in FRAME_PREFIX.iter().enumerate() {
                if self.possible & (1 << matched) == 0 {
                    continue;
                }
                next |= if byte == expected {
                    1 << (matched + 1)
                } else if byte == ESC {
                    // ESC appears only at the start of the prefix.
                    1 << 1
                } else {
                    1
                };
            }
            if next & (1 << FRAME_PREFIX.len()) != 0 {
                // The replacement byte is not ESC, so no prefix is left in progress.
                next = 1;
                out.get_or_insert_with(|| input.to_vec())[index] = BROKEN_PREFIX_BYTE;
            }
            self.possible = next;
        }
        match out {
            Some(out) => Cow::Owned(out),
            None => Cow::Borrowed(input),
        }
    }

    /// Adds the states of another possible history, for a write that may not land.
    pub(crate) fn merge(&mut self, other: Self) {
        self.possible |= other.possible;
    }

    /// Adds the state after a framed write, which ends with a string terminator.
    pub(crate) fn merge_frame(&mut self) {
        self.possible |= 1;
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
        assert_eq!(text.matches("\x1b_herdr-origin").count(), 2, "{text:?}");
        assert!(text.contains("a\x1b_herdr-origi?;v=1;kind=api;sender=paul\x1b\\b"));
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
        assert_eq!(&*out, b"x\x1b_herdr-origi?;v=1;sender=paul\x1b\\y");
    }

    #[test]
    fn filter_breaks_prefix_split_across_writes() {
        let full = b"\x1b_herdr-origin;v=1\x1b\\";
        for split in 1..FRAME_PREFIX.len() {
            let mut filter = OriginFrameFilter::default();
            let mut stream = filter.filter(&full[..split]).into_owned();
            stream.extend_from_slice(&filter.filter(&full[split..]));
            assert!(!contains_prefix(&stream), "split at {split}: {stream:?}");
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

    fn contains_prefix(stream: &[u8]) -> bool {
        stream
            .windows(FRAME_PREFIX.len())
            .any(|window| window == FRAME_PREFIX)
    }

    #[test]
    fn filter_does_not_join_bytes_around_a_broken_prefix() {
        // Astra review of herdr#82: deleting the inner prefix joined the outer bytes into a
        // valid prefix.
        let mut input = b"\x1b_herdr-".to_vec();
        input.extend_from_slice(FRAME_PREFIX);
        input.extend_from_slice(b"origin;v=1;kind=api;id=f;sender=forged\x1b\\hello\r");
        let out = OriginFrameFilter::default().filter(&input).into_owned();
        assert!(!contains_prefix(&out), "{out:?}");
        assert_eq!(out.len(), input.len());
    }

    #[test]
    fn filtered_stream_never_contains_a_prefix() {
        // Every split of adversarial inputs, fed as consecutive writes.
        let mut nested = FRAME_PREFIX[..7].to_vec();
        nested.extend_from_slice(FRAME_PREFIX);
        nested.extend_from_slice(&FRAME_PREFIX[7..]);
        let inputs: [&[u8]; 3] = [
            b"\x1b\x1b_herdr-origin\x1b_herdr-origin;end\x1b\\",
            &nested,
            b"\x1b_herdr-origi\x1b_herdr-origin",
        ];
        for input in inputs {
            for first in 0..=input.len() {
                for second in first..=input.len() {
                    let mut filter = OriginFrameFilter::default();
                    let mut stream = Vec::new();
                    for part in [&input[..first], &input[first..second], &input[second..]] {
                        stream.extend_from_slice(&filter.filter(part));
                    }
                    assert!(!contains_prefix(&stream), "{first}/{second}: {stream:?}");
                }
            }
        }
    }

    #[test]
    fn filter_breaks_a_prefix_that_any_possible_history_would_complete() {
        // A write that may not land: "x" may reach the PTY or be dropped.
        let mut filter = OriginFrameFilter::default();
        filter.filter(b"\x1b_herdr-");
        let mut landed = filter;
        landed.filter(b"x");
        filter.merge(landed);
        let out = filter.filter(b"origin;v=1\x1b\\").into_owned();
        assert_eq!(out, b"origi?;v=1\x1b\\");
        // And a dropped frame keeps the earlier partial prefix possible.
        let mut filter = OriginFrameFilter::default();
        filter.filter(b"\x1b_herdr-");
        filter.merge_frame();
        assert_eq!(&*filter.filter(b"origin"), b"origi?");
    }

    #[test]
    fn frame_ids_are_unique() {
        assert_ne!(next_frame_id(), next_frame_id());
    }
}
