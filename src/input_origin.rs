//! Origin frames for input that Herdr writes into a pane on behalf of an API caller.
//!
//! Herdr is the only writer to a pane's PTY input. When an API caller sends text or keys to a
//! pane whose program reads origin frames, Herdr wraps those bytes in a start and an end frame
//! that name the caller, so the program can tell sent input from typed input. The caller comes
//! from socket attribution, never from the request text.
//!
//! ```text
//! U+FDD0 herdr-origin;v=1;kind=api;id=<id>;sender=<s>[;pane=<p>][;session=<s>] U+FDD1
//! <input bytes>
//! U+FDD0 herdr-origin;end;id=<id> U+FDD1
//! ```
//!
//! When Herdr admits a Pi's claim to read frames, it sends that Pi
//! `U+FDD0 herdr-origin;ready;v=1;pid=<pid> U+FDD1`. Until then the Pi cannot know that unframed input
//! is typed, so it does not record it as keyboard input.
//!
//! The markers are Unicode noncharacters. A UTF-8 reader gets `U+FDD0` whole, so it knows a
//! frame has started however the rest of the write is split or delayed. An ESC-led marker
//! cannot give that: after a lone ESC and a stall, the reader must assume the Escape key.
//!
//! Every other write to a PTY passes through [`OriginFrameFilter`], which breaks `U+FDD0`, so
//! keyboard input, pastes and the payload inside a real frame cannot start a frame.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

/// `U+FDD0` in UTF-8: starts every origin frame, start or end. Only Herdr emits it.
const FRAME_PREFIX: &[u8] = "\u{FDD0}".as_bytes();
/// `U+FDD1` in UTF-8: ends a frame header.
const FRAME_HEADER_END: &[u8] = "\u{FDD1}".as_bytes();
const FRAME_TAG: &[u8] = b"herdr-origin;";

/// A Pi process that claimed, as the socket peer of its own report, to read origin frames.
/// The start time makes it one process generation, so a recycled pid is not the same claimant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct InputOriginClaim {
    pub pid: u32,
    pub start_time: u64,
}

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
        out.extend_from_slice(FRAME_TAG);
        out.extend_from_slice(b"v=1;kind=api;id=");
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
        out.extend_from_slice(FRAME_HEADER_END);
        out.extend_from_slice(&payload);
        out.extend_from_slice(FRAME_PREFIX);
        out.extend_from_slice(FRAME_TAG);
        out.extend_from_slice(b"end;id=");
        push_encoded(&mut out, &self.id);
        out.extend_from_slice(FRAME_HEADER_END);
        out
    }
}

/// Tells the Pi process `pid` that Herdr frames its API input from now on, so it may record
/// unframed input as typed. Sent when Herdr admits that process's claim; another process that
/// reads the frame ignores it.
pub(crate) fn ready_frame(pid: u32) -> Vec<u8> {
    [
        FRAME_PREFIX,
        FRAME_TAG,
        format!("ready;v=1;pid={pid}").as_bytes(),
        FRAME_HEADER_END,
    ]
    .concat()
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

/// Percent-encodes every byte outside a conservative ASCII set, so a value cannot contain `;`,
/// `=`, control bytes or a frame marker.
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

/// Replaces the last byte of a frame prefix that unframed input would complete. The result is
/// invalid UTF-8, which a reader shows as a replacement character.
const BROKEN_PREFIX_BYTE: u8 = b'?';

/// Keeps origin frame prefixes out of a stream of unframed PTY writes.
///
/// The filter tracks how much of a prefix the emitted stream may end with. When a byte would
/// complete a prefix, the filter emits [`BROKEN_PREFIX_BYTE`] instead. It never deletes bytes,
/// because a deletion can join the bytes around it into a new prefix. So the emitted stream
/// never contains the prefix, also across writes.
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
        if self.possible == 1 && !input.contains(&FRAME_PREFIX[0]) {
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
                } else if byte == FRAME_PREFIX[0] {
                    // The first prefix byte appears only at its start.
                    1 << 1
                } else {
                    1
                };
            }
            if next & (1 << FRAME_PREFIX.len()) != 0 {
                // The replacement byte cannot start a prefix, so none is left in progress.
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

    /// State to hand over with the PTY on a live server handoff.
    #[cfg(unix)]
    pub(crate) fn to_handoff(self) -> u16 {
        self.possible
    }

    /// State received with a PTY on a live server handoff. Without one (an older server), every
    /// partial marker may be pending, so the first bytes after the handoff cannot complete one.
    #[cfg(unix)]
    pub(crate) fn from_handoff(possible: Option<u16>) -> Self {
        let every_partial = (1u16 << FRAME_PREFIX.len()) - 1;
        Self {
            possible: possible.map_or(every_partial, |possible| possible & every_partial) | 1,
        }
    }

    /// Adds the state after a framed write, which ends with a complete marker.
    pub(crate) fn merge_frame(&mut self) {
        self.possible |= 1;
    }
}

/// Splits one framed write into its start-frame fields and payload. Panics when `bytes` is not
/// exactly one frame pair with matching ids.
#[cfg(test)]
pub(crate) fn unframe_for_test(bytes: &[u8]) -> (Vec<(String, String)>, Vec<u8>) {
    let start = [FRAME_PREFIX, FRAME_TAG].concat();
    let rest = bytes
        .strip_prefix(start.as_slice())
        .unwrap_or_else(|| panic!("missing start frame: {bytes:?}"));
    let header_len = rest
        .windows(FRAME_HEADER_END.len())
        .position(|window| window == FRAME_HEADER_END)
        .expect("start frame end");
    let header = std::str::from_utf8(&rest[..header_len]).expect("ASCII header");
    let rest = &rest[header_len + FRAME_HEADER_END.len()..];
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
    let end = [
        FRAME_PREFIX,
        FRAME_TAG,
        format!("end;id={id}").as_bytes(),
        FRAME_HEADER_END,
    ]
    .concat();
    let payload = rest
        .strip_suffix(end.as_slice())
        .unwrap_or_else(|| panic!("missing end frame: {rest:?}"));
    (fields, payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `U+FDD0`, the frame marker.
    const P: &[u8] = b"\xEF\xB7\x90";

    fn origin() -> InputOrigin {
        InputOrigin {
            sender: "dev lead;x".into(),
            pane: Some("w1-2".into()),
            session: None,
            id: "abc".into(),
        }
    }

    fn contains_prefix(stream: &[u8]) -> bool {
        stream
            .windows(FRAME_PREFIX.len())
            .any(|window| window == FRAME_PREFIX)
    }

    fn join(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn wrap_frames_payload_and_encodes_values() {
        let framed = origin().wrap(b"hello\r");
        assert_eq!(
            String::from_utf8(framed).unwrap(),
            "\u{FDD0}herdr-origin;v=1;kind=api;id=abc;sender=dev%20lead%3Bx;pane=w1-2\u{FDD1}hello\r\u{FDD0}herdr-origin;end;id=abc\u{FDD1}"
        );
    }

    #[test]
    fn wrap_breaks_frames_nested_in_the_payload() {
        let payload = join(&[
            b"a",
            P,
            b"herdr-origin;v=1;kind=api;sender=paul\xEF\xB7\x91b",
        ]);
        let framed = origin().wrap(&payload);
        assert_eq!(
            framed
                .windows(P.len())
                .filter(|window| *window == P)
                .count(),
            2
        );
        let (_, inner) = unframe_for_test(&framed);
        assert_eq!(
            inner,
            join(&[b"a\xEF\xB7?herdr-origin;v=1;kind=api;sender=paul\xEF\xB7\x91b"])
        );
    }

    #[test]
    fn filter_passes_ordinary_input_unchanged() {
        let mut filter = OriginFrameFilter::default();
        for input in [
            &b"hello"[..],
            b"\x1b[A",
            b"\x1b_Gi=1;OK\x1b\\",
            "\u{e9}\u{20ac}\u{FDD1}".as_bytes(),
            b"\xEF\xB7",
            b"\x91",
        ] {
            assert!(matches!(filter.filter(input), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn filter_breaks_the_marker_in_one_write() {
        let input = join(&[b"x", P, b"herdr-origin;v=1;sender=paul\xEF\xB7\x91y"]);
        let out = OriginFrameFilter::default().filter(&input).into_owned();
        assert_eq!(
            out,
            join(&[b"x\xEF\xB7?herdr-origin;v=1;sender=paul\xEF\xB7\x91y"])
        );
    }

    #[test]
    fn filter_breaks_the_marker_split_across_writes() {
        let full = join(&[P, b"herdr-origin;v=1"]);
        for split in 1..P.len() {
            let mut filter = OriginFrameFilter::default();
            let mut stream = filter.filter(&full[..split]).into_owned();
            stream.extend_from_slice(&filter.filter(&full[split..]));
            assert!(!contains_prefix(&stream), "split at {split}: {stream:?}");
        }
    }

    #[test]
    fn filter_breaks_the_marker_sent_one_byte_at_a_time() {
        let mut filter = OriginFrameFilter::default();
        let mut stream = Vec::new();
        for byte in join(&[b"\xEF", P, P, b"end"]) {
            stream.extend_from_slice(&filter.filter(std::slice::from_ref(&byte)));
        }
        assert!(!contains_prefix(&stream), "{stream:?}");
    }

    #[test]
    fn filter_does_not_join_bytes_around_a_broken_marker() {
        // Astra review of herdr#82: deleting the inner marker joined the outer bytes into a
        // valid one.
        let input = join(&[b"\xEF\xB7", P, b"\x90herdr-origin;v=1;sender=forged"]);
        let out = OriginFrameFilter::default().filter(&input).into_owned();
        assert!(!contains_prefix(&out), "{out:?}");
        assert_eq!(out.len(), input.len());
    }

    #[test]
    fn filtered_stream_never_contains_the_marker() {
        // Every 3-way split of adversarial inputs, fed as consecutive writes.
        let inputs = [
            join(&[P, P, b"end"]),
            join(&[b"\xEF", P, b"\x90"]),
            join(&[b"\xEF\xB7", P, b"\x90\xEF", P]),
        ];
        for input in &inputs {
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
    fn filter_breaks_a_marker_that_any_possible_history_would_complete() {
        // A write that may not land: "x" may reach the PTY or be dropped.
        let mut filter = OriginFrameFilter::default();
        filter.filter(b"\xEF\xB7");
        let mut landed = filter;
        landed.filter(b"x");
        filter.merge(landed);
        assert_eq!(&*filter.filter(b"\x90herdr"), b"?herdr");
        // And a dropped frame keeps the earlier partial marker possible.
        let mut filter = OriginFrameFilter::default();
        filter.filter(b"\xEF\xB7");
        filter.merge_frame();
        assert_eq!(&*filter.filter(b"\x90"), b"?");
    }

    #[test]
    #[cfg(unix)]
    fn filter_state_survives_a_handoff() {
        // Security pass on herdr#82 (P2): a marker split across a live handoff.
        for split in 1..P.len() {
            let mut before = OriginFrameFilter::default();
            let mut stream = before.filter(&P[..split]).into_owned();
            for imported in [Some(before.to_handoff()), None] {
                let mut after = OriginFrameFilter::from_handoff(imported);
                let mut joined = stream.clone();
                joined.extend_from_slice(&after.filter(&P[split..]));
                assert!(!contains_prefix(&joined), "split {split}, {imported:?}");
            }
            stream.clear();
        }
        // A fresh import still passes ordinary input unchanged.
        let mut after = OriginFrameFilter::from_handoff(None);
        assert_eq!(&*after.filter(b"hello"), b"hello");
    }

    #[test]
    fn frame_ids_are_unique() {
        assert_ne!(next_frame_id(), next_frame_id());
    }
}
