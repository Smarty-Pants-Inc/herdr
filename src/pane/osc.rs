use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use tracing::info;

use crate::layout::PaneId;

use super::terminal::GhosttyPaneCore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DefaultColorQuery {
    Foreground,
    Background,
    Cursor,
}

impl DefaultColorQuery {
    pub(super) fn osc_number(self) -> u8 {
        match self {
            Self::Foreground => 10,
            Self::Background => 11,
            Self::Cursor => 12,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DefaultColorEvent {
    Query(DefaultColorQuery),
    Set(DefaultColorQuery),
    Reset(DefaultColorQuery),
    PaletteQuery(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DefaultColorTrackedEvent {
    pub(super) end_offset: usize,
    pub(super) event: DefaultColorEvent,
}

#[derive(Debug, Default)]
pub(super) struct DefaultColorOscTracker {
    state: DefaultColorOscTrackerState,
    body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DefaultColorOscTrackerState {
    #[default]
    Ground,
    Escape,
    OscBody,
    OscEscape,
    IgnoreString,
    IgnoreStringEscape,
    OversizedOsc,
    OversizedOscEscape,
}

fn is_ignored_string_intro(byte: u8) -> bool {
    matches!(byte, b'P' | b'_' | b'^' | b'X')
}

impl DefaultColorOscTracker {
    pub(super) fn observe(&mut self, bytes: &[u8]) -> bool {
        let mut saw_default_color_set = false;

        for &byte in bytes {
            match self.state {
                DefaultColorOscTrackerState::Ground => {
                    if byte == 0x1b {
                        self.state = DefaultColorOscTrackerState::Escape;
                    }
                }
                DefaultColorOscTrackerState::Escape => {
                    if byte == b']' {
                        self.body.clear();
                        self.state = DefaultColorOscTrackerState::OscBody;
                    } else if is_ignored_string_intro(byte) {
                        self.body.clear();
                        self.state = DefaultColorOscTrackerState::IgnoreString;
                    } else if byte == 0x1b {
                        self.state = DefaultColorOscTrackerState::Escape;
                    } else {
                        self.state = DefaultColorOscTrackerState::Ground;
                    }
                }
                DefaultColorOscTrackerState::OscBody => match byte {
                    0x07 => {
                        saw_default_color_set |= is_default_color_set_osc(&self.body);
                        self.body.clear();
                        self.state = DefaultColorOscTrackerState::Ground;
                    }
                    0x1b => self.state = DefaultColorOscTrackerState::OscEscape,
                    _ => self.body.push(byte),
                },
                DefaultColorOscTrackerState::OscEscape => {
                    if byte == b'\\' {
                        saw_default_color_set |= is_default_color_set_osc(&self.body);
                        self.body.clear();
                        self.state = DefaultColorOscTrackerState::Ground;
                    } else {
                        self.body.push(0x1b);
                        self.body.push(byte);
                        self.state = DefaultColorOscTrackerState::OscBody;
                    }
                }
                DefaultColorOscTrackerState::IgnoreString => {
                    if byte == 0x1b {
                        self.state = DefaultColorOscTrackerState::IgnoreStringEscape;
                    }
                }
                DefaultColorOscTrackerState::IgnoreStringEscape => {
                    if byte == b'\\' {
                        self.state = DefaultColorOscTrackerState::Ground;
                    } else if byte != 0x1b {
                        self.state = DefaultColorOscTrackerState::IgnoreString;
                    }
                }
                DefaultColorOscTrackerState::OversizedOsc => {
                    if byte == 0x1b {
                        self.state = DefaultColorOscTrackerState::OversizedOscEscape;
                    } else if byte == 0x07 {
                        self.state = DefaultColorOscTrackerState::Ground;
                    }
                }
                DefaultColorOscTrackerState::OversizedOscEscape => {
                    if byte == b'\\' {
                        self.state = DefaultColorOscTrackerState::Ground;
                    } else if byte != 0x1b {
                        self.state = DefaultColorOscTrackerState::OversizedOsc;
                    }
                }
            }

            if self.body.len() > 1024 {
                self.body.clear();
                self.state = DefaultColorOscTrackerState::OversizedOsc;
            }
        }

        saw_default_color_set
    }
}

fn is_default_color_set_osc(body: &[u8]) -> bool {
    parse_default_color_events(body)
        .iter()
        .any(|event| matches!(event, DefaultColorEvent::Set(_)))
}

#[derive(Debug, Default)]
pub(super) struct DefaultColorEventTracker {
    state: DefaultColorOscTrackerState,
    body: Vec<u8>,
    pending: Vec<DefaultColorTrackedEvent>,
}

impl DefaultColorEventTracker {
    pub(super) fn observe(&mut self, bytes: &[u8]) {
        for (index, &byte) in bytes.iter().enumerate() {
            match self.state {
                DefaultColorOscTrackerState::Ground => {
                    if byte == 0x1b {
                        self.state = DefaultColorOscTrackerState::Escape;
                    }
                }
                DefaultColorOscTrackerState::Escape => {
                    if byte == b']' {
                        self.body.clear();
                        self.state = DefaultColorOscTrackerState::OscBody;
                    } else if is_ignored_string_intro(byte) {
                        self.body.clear();
                        self.state = DefaultColorOscTrackerState::IgnoreString;
                    } else if byte == 0x1b {
                        self.state = DefaultColorOscTrackerState::Escape;
                    } else {
                        self.state = DefaultColorOscTrackerState::Ground;
                    }
                }
                DefaultColorOscTrackerState::OscBody => match byte {
                    0x07 => {
                        self.finalize(index + 1);
                        self.state = DefaultColorOscTrackerState::Ground;
                    }
                    0x1b => self.state = DefaultColorOscTrackerState::OscEscape,
                    _ => self.body.push(byte),
                },
                DefaultColorOscTrackerState::OscEscape => {
                    if byte == b'\\' {
                        self.finalize(index + 1);
                        self.state = DefaultColorOscTrackerState::Ground;
                    } else {
                        self.body.push(0x1b);
                        self.body.push(byte);
                        self.state = DefaultColorOscTrackerState::OscBody;
                    }
                }
                DefaultColorOscTrackerState::IgnoreString => {
                    if byte == 0x1b {
                        self.state = DefaultColorOscTrackerState::IgnoreStringEscape;
                    }
                }
                DefaultColorOscTrackerState::IgnoreStringEscape => {
                    if byte == b'\\' {
                        self.state = DefaultColorOscTrackerState::Ground;
                    } else if byte != 0x1b {
                        self.state = DefaultColorOscTrackerState::IgnoreString;
                    }
                }
                DefaultColorOscTrackerState::OversizedOsc => {
                    if byte == 0x1b {
                        self.state = DefaultColorOscTrackerState::OversizedOscEscape;
                    } else if byte == 0x07 {
                        self.state = DefaultColorOscTrackerState::Ground;
                    }
                }
                DefaultColorOscTrackerState::OversizedOscEscape => {
                    if byte == b'\\' {
                        self.state = DefaultColorOscTrackerState::Ground;
                    } else if byte != 0x1b {
                        self.state = DefaultColorOscTrackerState::OversizedOsc;
                    }
                }
            }

            if self.body.len() > 1024 {
                self.body.clear();
                self.state = DefaultColorOscTrackerState::OversizedOsc;
            }
        }
    }

    fn finalize(&mut self, end_offset: usize) {
        self.pending.extend(
            parse_default_color_events(&self.body)
                .into_iter()
                .map(|event| DefaultColorTrackedEvent { end_offset, event }),
        );
        self.body.clear();
    }

    pub(super) fn in_progress_event(&self) -> Option<DefaultColorEvent> {
        if !matches!(
            self.state,
            DefaultColorOscTrackerState::OscBody | DefaultColorOscTrackerState::OscEscape
        ) {
            return None;
        }
        let mut events = parse_default_color_events(&self.body);
        (events.len() == 1).then(|| events.remove(0))
    }

    pub(super) fn drain_pending(&mut self) -> Vec<DefaultColorTrackedEvent> {
        std::mem::take(&mut self.pending)
    }
}

fn parse_default_color_events(body: &[u8]) -> Vec<DefaultColorEvent> {
    let single = match body {
        b"10;?" => Some(DefaultColorEvent::Query(DefaultColorQuery::Foreground)),
        b"11;?" => Some(DefaultColorEvent::Query(DefaultColorQuery::Background)),
        b"12;?" => Some(DefaultColorEvent::Query(DefaultColorQuery::Cursor)),
        b"110" | b"110;" => Some(DefaultColorEvent::Reset(DefaultColorQuery::Foreground)),
        b"111" | b"111;" => Some(DefaultColorEvent::Reset(DefaultColorQuery::Background)),
        _ => parse_palette_color_query(body),
    };
    if let Some(event) = single {
        return vec![event];
    }
    parse_default_color_set_events(body)
}

fn parse_palette_color_query(body: &[u8]) -> Option<DefaultColorEvent> {
    let index = body.strip_prefix(b"4;")?.strip_suffix(b";?")?;
    if index.is_empty() || index.len() > 3 || !index.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut value: u16 = 0;
    for &digit in index {
        value = value * 10 + u16::from(digit - b'0');
    }
    u8::try_from(value)
        .ok()
        .map(DefaultColorEvent::PaletteQuery)
}

fn parse_default_color_set_events(body: &[u8]) -> Vec<DefaultColorEvent> {
    let Some(separator) = body.iter().position(|byte| *byte == b';') else {
        return Vec::new();
    };
    let start = match &body[..separator] {
        b"10" => 10,
        b"11" => 11,
        b"12" => 12,
        _ => return Vec::new(),
    };
    body[separator + 1..]
        .split(|byte| *byte == b';')
        .filter(|value| !value.is_empty())
        .enumerate()
        .filter_map(|(offset, value)| {
            if value == b"?" {
                return None;
            }
            let query = match start + offset {
                10 => DefaultColorQuery::Foreground,
                11 => DefaultColorQuery::Background,
                12 => DefaultColorQuery::Cursor,
                _ => return None,
            };
            Some(DefaultColorEvent::Set(query))
        })
        .collect()
}

pub(crate) type ReportedCwd = (PathBuf, Option<String>);

pub(super) fn parse_reported_cwd(value: &[u8]) -> Option<ReportedCwd> {
    let value = std::str::from_utf8(value).ok()?.trim();
    if value.starts_with("file://") {
        return parse_file_uri_cwd(value);
    }
    let path = value.trim_matches('"');
    (!path.is_empty()).then(|| (PathBuf::from(path), None))
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteExecReady {
    pub(crate) hostname: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
}

pub(super) struct FilteredPtyBytes<'a> {
    pub(super) bytes: Cow<'a, [u8]>,
    pub(super) ready: Option<RemoteExecReady>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RemoteExecReadyFilter {
    state: RemoteExecReadyFilterState,
    prefix: Vec<u8>,
    payload: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_nonce: Option<crate::execution::RemoteExecReadyNonce>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum RemoteExecReadyFilterState {
    #[default]
    Ground,
    Prefix,
    Payload,
    PayloadEscape,
    Discarding,
    DiscardingEscape,
}

impl RemoteExecReadyFilter {
    #[cfg(any(unix, test))]
    pub(crate) fn set_expected_nonce(
        &mut self,
        expected_nonce: Option<crate::execution::RemoteExecReadyNonce>,
    ) {
        self.expected_nonce = expected_nonce;
    }

    #[cfg(unix)]
    pub(crate) fn validated_handoff_state(mut self) -> Self {
        let valid = match self.state {
            RemoteExecReadyFilterState::Ground => {
                self.prefix.clear();
                self.payload.clear();
                true
            }
            RemoteExecReadyFilterState::Prefix => {
                self.payload.is_empty()
                    && !self.prefix.is_empty()
                    && self.prefix.len() < crate::execution::REMOTE_EXEC_READY_OSC_PREFIX.len()
                    && crate::execution::REMOTE_EXEC_READY_OSC_PREFIX.starts_with(&self.prefix)
            }
            RemoteExecReadyFilterState::Payload | RemoteExecReadyFilterState::PayloadEscape => {
                self.prefix.is_empty()
                    && self.payload.len() <= crate::execution::REMOTE_EXEC_READY_PAYLOAD_MAX_BYTES
            }
            RemoteExecReadyFilterState::Discarding
            | RemoteExecReadyFilterState::DiscardingEscape => {
                self.prefix.is_empty() && self.payload.is_empty()
            }
        };
        if valid {
            self
        } else {
            Self::default()
        }
    }

    pub(super) fn filter<'a>(&mut self, bytes: &'a [u8]) -> FilteredPtyBytes<'a> {
        if self.state == RemoteExecReadyFilterState::Ground
            && !bytes.iter().enumerate().any(|(index, byte)| {
                if *byte != crate::execution::REMOTE_EXEC_READY_OSC_PREFIX[0] {
                    return false;
                }
                let candidate = &bytes[index..];
                candidate.starts_with(crate::execution::REMOTE_EXEC_READY_OSC_PREFIX)
                    || crate::execution::REMOTE_EXEC_READY_OSC_PREFIX.starts_with(candidate)
            })
        {
            return FilteredPtyBytes {
                bytes: Cow::Borrowed(bytes),
                ready: None,
            };
        }

        let mut output = Vec::with_capacity(bytes.len());
        let mut ready = None;
        for &byte in bytes {
            self.filter_byte(byte, &mut output, &mut ready);
        }
        FilteredPtyBytes {
            bytes: Cow::Owned(output),
            ready,
        }
    }

    fn filter_byte(&mut self, byte: u8, output: &mut Vec<u8>, ready: &mut Option<RemoteExecReady>) {
        let mut pending = Some(byte);
        while let Some(byte) = pending.take() {
            match self.state {
                RemoteExecReadyFilterState::Ground => {
                    if byte == crate::execution::REMOTE_EXEC_READY_OSC_PREFIX[0] {
                        self.prefix.push(byte);
                        self.state = RemoteExecReadyFilterState::Prefix;
                    } else {
                        output.push(byte);
                    }
                }
                RemoteExecReadyFilterState::Prefix => {
                    if byte == crate::execution::REMOTE_EXEC_READY_OSC_PREFIX[self.prefix.len()] {
                        self.prefix.push(byte);
                        if self.prefix.len() == crate::execution::REMOTE_EXEC_READY_OSC_PREFIX.len()
                        {
                            self.prefix.clear();
                            self.payload.clear();
                            self.state = RemoteExecReadyFilterState::Payload;
                        }
                    } else {
                        output.append(&mut self.prefix);
                        self.state = RemoteExecReadyFilterState::Ground;
                        pending = Some(byte);
                    }
                }
                RemoteExecReadyFilterState::Payload => match byte {
                    0x07 => self.finish(ready),
                    0x1b => self.state = RemoteExecReadyFilterState::PayloadEscape,
                    _ if self.payload.len()
                        < crate::execution::REMOTE_EXEC_READY_PAYLOAD_MAX_BYTES =>
                    {
                        self.payload.push(byte)
                    }
                    _ => {
                        self.payload.clear();
                        self.state = RemoteExecReadyFilterState::Discarding;
                    }
                },
                RemoteExecReadyFilterState::PayloadEscape => {
                    if byte == b'\\' {
                        self.finish(ready);
                    } else if byte == 0x07 {
                        self.payload.clear();
                        self.state = RemoteExecReadyFilterState::Ground;
                    } else {
                        self.payload.clear();
                        self.state = if byte == 0x1b {
                            RemoteExecReadyFilterState::DiscardingEscape
                        } else {
                            RemoteExecReadyFilterState::Discarding
                        };
                    }
                }
                RemoteExecReadyFilterState::Discarding => match byte {
                    0x07 => self.state = RemoteExecReadyFilterState::Ground,
                    0x1b => self.state = RemoteExecReadyFilterState::DiscardingEscape,
                    _ => {}
                },
                RemoteExecReadyFilterState::DiscardingEscape => {
                    if byte == b'\\' {
                        self.state = RemoteExecReadyFilterState::Ground;
                    } else if byte != 0x1b {
                        self.state = RemoteExecReadyFilterState::Discarding;
                    }
                }
            }
        }
    }

    fn finish(&mut self, ready: &mut Option<RemoteExecReady>) {
        if ready.is_none() {
            if let Some(parsed) =
                parse_remote_exec_ready(&self.payload, self.expected_nonce.as_ref())
            {
                self.expected_nonce = None;
                *ready = Some(parsed);
            }
        }
        self.payload.clear();
        self.state = RemoteExecReadyFilterState::Ground;
    }
}

fn parse_remote_exec_ready(
    payload: &[u8],
    expected_nonce: Option<&crate::execution::RemoteExecReadyNonce>,
) -> Option<RemoteExecReady> {
    #[derive(Deserialize)]
    struct WirePayload {
        nonce: String,
        #[serde(default)]
        hostname: Option<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    }

    let WirePayload {
        nonce,
        hostname,
        cwd,
    } = serde_json::from_slice(payload).ok()?;
    if !expected_nonce.is_some_and(|expected| expected.matches(&nonce)) {
        return None;
    }
    let hostname =
        hostname.filter(|hostname| !hostname.is_empty() && !hostname.chars().any(char::is_control));
    let cwd = cwd.filter(|cwd| cwd.is_absolute());
    Some(RemoteExecReady { hostname, cwd })
}

/// Collects complete OSC bodies from a raw byte stream. Consumers receive only
/// bodies, keeping the framing state machine independent from OSC commands.
#[derive(Debug, Default)]
struct OscStreamCollector {
    state: OscStreamState,
    body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OscStreamState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Body,
    IgnoringString,
    Discarding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawC1Transition {
    Ground,
    String,
    Csi,
    Osc,
}

fn raw_c1_transition(byte: u8) -> Option<RawC1Transition> {
    match byte {
        0x80..=0x8f | 0x91..=0x97 | 0x99 | 0x9a | 0x9c => Some(RawC1Transition::Ground),
        0x90 | 0x98 | 0x9e | 0x9f => Some(RawC1Transition::String),
        0x9b => Some(RawC1Transition::Csi),
        0x9d => Some(RawC1Transition::Osc),
        _ => None,
    }
}

impl OscStreamCollector {
    const MAX_BODY_BYTES: usize = 4096;

    fn observe(&mut self, bytes: &[u8], mut receive: impl FnMut(&[u8])) {
        for &byte in bytes {
            self.observe_byte(byte, &mut receive);
        }
    }

    fn observe_byte(&mut self, byte: u8, receive: &mut impl FnMut(&[u8])) {
        // Ghostty decodes Ground as UTF-8, so raw C1 bytes are controls only
        // after a control sequence has left Ground. OSC overrides them as data.
        if byte == 0x1b {
            self.finish_body(receive);
            self.state = OscStreamState::Escape;
            return;
        }
        if matches!(byte, 0x18 | 0x1a) {
            self.finish_body(receive);
            self.state = OscStreamState::Ground;
            return;
        }
        if !matches!(
            self.state,
            OscStreamState::Ground | OscStreamState::Body | OscStreamState::Discarding
        ) {
            if let Some(transition) = raw_c1_transition(byte) {
                self.state = match transition {
                    RawC1Transition::Ground => OscStreamState::Ground,
                    RawC1Transition::String => OscStreamState::IgnoringString,
                    RawC1Transition::Csi => OscStreamState::Csi,
                    RawC1Transition::Osc => {
                        self.body.clear();
                        OscStreamState::Body
                    }
                };
                return;
            }
        }

        match self.state {
            OscStreamState::Ground => {}
            OscStreamState::Escape => match byte {
                b'[' => self.state = OscStreamState::Csi,
                b']' => {
                    self.body.clear();
                    self.state = OscStreamState::Body;
                }
                byte if is_ignored_string_intro(byte) => {
                    self.state = OscStreamState::IgnoringString;
                }
                0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f => {}
                0x20..=0x2f => self.state = OscStreamState::EscapeIntermediate,
                0x30..=0x7e => self.state = OscStreamState::Ground,
                _ => {}
            },
            OscStreamState::EscapeIntermediate => match byte {
                0x00..=0x17 | 0x19 | 0x1c..=0x2f | 0x7f => {}
                0x30..=0x7e => self.state = OscStreamState::Ground,
                _ => {}
            },
            OscStreamState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.state = OscStreamState::Ground;
                }
            }
            OscStreamState::Body => match byte {
                0x07 => self.finish(receive),
                byte if is_osc_ignored_control(byte) => {}
                _ => self.push(byte),
            },
            OscStreamState::IgnoringString => {}
            OscStreamState::Discarding => {
                if byte == 0x07 {
                    self.state = OscStreamState::Ground;
                }
            }
        }
    }

    fn finish_body(&mut self, receive: &mut impl FnMut(&[u8])) {
        if self.state == OscStreamState::Body {
            self.finish(receive);
        }
    }

    fn reset(&mut self) {
        self.state = OscStreamState::Ground;
        self.body.clear();
    }

    fn push(&mut self, byte: u8) {
        self.body.push(byte);
        if self.body.len() > Self::MAX_BODY_BYTES {
            self.body.clear();
            self.state = OscStreamState::Discarding;
        } else {
            self.state = OscStreamState::Body;
        }
    }

    fn finish(&mut self, receive: &mut impl FnMut(&[u8])) {
        receive(&self.body);
        self.body.clear();
        self.state = OscStreamState::Ground;
    }
}

fn is_osc_ignored_control(byte: u8) -> bool {
    matches!(byte, 0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f)
}

/// Tracks the small subset of terminal control state needed to discard OSC 133
/// reply markers from alternate screens and to reset anchors before primary ED3.
#[derive(Debug, Default)]
struct PrimaryScreenEscapeTracker {
    alternate_screen: bool,
    state: PrimaryScreenEscapeState,
}

const PRIMARY_SCREEN_ESCAPE_MAX_CSI_BYTES: usize = 32;

#[derive(Debug, Default)]
enum PrimaryScreenEscapeState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi {
        params: [u8; PRIMARY_SCREEN_ESCAPE_MAX_CSI_BYTES],
        len: usize,
    },
    CsiDiscard,
    OscString,
    String,
}

fn primary_screen_csi_state() -> PrimaryScreenEscapeState {
    PrimaryScreenEscapeState::Csi {
        params: [0; PRIMARY_SCREEN_ESCAPE_MAX_CSI_BYTES],
        len: 0,
    }
}

impl PrimaryScreenEscapeTracker {
    fn set_alternate_screen(&mut self, alternate_screen: bool) {
        self.alternate_screen = alternate_screen;
    }

    fn alternate_screen(&self) -> bool {
        self.alternate_screen
    }

    /// Returns true only for a primary-screen ED3 that has fully arrived.
    fn observe(&mut self, byte: u8) -> bool {
        // Ghostty decodes Ground as UTF-8, so raw C1 bytes are controls only
        // after a control sequence has left Ground. OSC overrides them as data.
        if byte == 0x1b {
            self.state = PrimaryScreenEscapeState::Escape;
            return false;
        }
        if matches!(byte, 0x18 | 0x1a) {
            self.state = PrimaryScreenEscapeState::Ground;
            return false;
        }
        if !matches!(
            self.state,
            PrimaryScreenEscapeState::Ground | PrimaryScreenEscapeState::OscString
        ) {
            if let Some(transition) = raw_c1_transition(byte) {
                self.state = match transition {
                    RawC1Transition::Ground => PrimaryScreenEscapeState::Ground,
                    RawC1Transition::String => PrimaryScreenEscapeState::String,
                    RawC1Transition::Csi => primary_screen_csi_state(),
                    RawC1Transition::Osc => PrimaryScreenEscapeState::OscString,
                };
                return false;
            }
        }

        match &mut self.state {
            PrimaryScreenEscapeState::Ground => {}
            PrimaryScreenEscapeState::Escape => match byte {
                b'[' => self.state = primary_screen_csi_state(),
                b']' => self.state = PrimaryScreenEscapeState::OscString,
                byte if is_ignored_string_intro(byte) => {
                    self.state = PrimaryScreenEscapeState::String;
                }
                0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f => {}
                0x20..=0x2f => self.state = PrimaryScreenEscapeState::EscapeIntermediate,
                0x30..=0x7e => self.state = PrimaryScreenEscapeState::Ground,
                _ => {}
            },
            PrimaryScreenEscapeState::EscapeIntermediate => match byte {
                0x00..=0x17 | 0x19 | 0x1c..=0x2f | 0x7f => {}
                0x30..=0x7e => self.state = PrimaryScreenEscapeState::Ground,
                _ => {}
            },
            PrimaryScreenEscapeState::Csi { params, len } => {
                if (0x40..=0x7e).contains(&byte) {
                    let clear =
                        apply_primary_screen_csi(&params[..*len], byte, &mut self.alternate_screen);
                    self.state = PrimaryScreenEscapeState::Ground;
                    return clear;
                }
                if (0x20..=0x3f).contains(&byte) {
                    if *len < params.len() {
                        params[*len] = byte;
                        *len += 1;
                    } else {
                        self.state = PrimaryScreenEscapeState::CsiDiscard;
                    }
                }
            }
            PrimaryScreenEscapeState::CsiDiscard => {
                if (0x40..=0x7e).contains(&byte) {
                    self.state = PrimaryScreenEscapeState::Ground;
                }
            }
            PrimaryScreenEscapeState::OscString => {
                if byte == 0x07 {
                    self.state = PrimaryScreenEscapeState::Ground;
                }
            }
            PrimaryScreenEscapeState::String => {}
        }
        false
    }
}

fn apply_primary_screen_csi(params: &[u8], final_byte: u8, alternate_screen: &mut bool) -> bool {
    if matches!(final_byte, b'h' | b'l') && params.first() == Some(&b'?') {
        let alternate_mode = params[1..]
            .split(|byte| *byte == b';')
            .any(|mode| matches!(mode, b"47" | b"1047" | b"1049"));
        if alternate_mode {
            *alternate_screen = final_byte == b'h';
        }
    }
    final_byte == b'J' && !*alternate_screen && matches!(params, b"3" | b"?3")
}

/// Maximum retained string length for agent OSC title and progress payloads.
/// Title text is untrusted model output; cap it to bound memory and log size.
const AGENT_OSC_MAX_CHARS: usize = 256;

/// Maximum retained reply ID length. The pending queue lives only for the
/// current PTY buffer, so its total allocation is bounded by that buffer.
const OMP_REPLY_ANCHOR_MAX_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct OmpReplyGenerationToken {
    process: u64,
    marker_epoch: u64,
}

impl OmpReplyGenerationToken {
    fn next_process(self) -> Self {
        Self {
            process: self.process.wrapping_add(1),
            ..self
        }
    }

    fn next_marker(self) -> Self {
        Self {
            marker_epoch: self.marker_epoch.wrapping_add(1),
            ..self
        }
    }
}

#[derive(Debug)]
pub(super) struct PendingOmpReplyAnchor {
    pub(super) end_offset: usize,
    pub(super) anchor_id: Vec<u8>,
}

#[derive(Debug, Default)]
pub(super) struct AgentOscObservation {
    pub(super) terminal_title_changed: bool,
    pub(super) reply_anchor_events: Vec<PendingOmpReplyAnchor>,
    pub(super) reset_reply_selection: bool,
}

#[derive(Debug)]
struct OmpReplyAnchor {
    id: Vec<u8>,
    row: crate::ghostty::TrackedGridRef,
}

struct OmpReplyAid<'a> {
    session: &'a [u8],
    id: &'a [u8],
}

/// Always-on tracker that retains the latest OSC 0/2 title and OSC 9 progress
/// payload emitted by the child process. Nothing here affects rendering; this
/// is pure passive capture for the detection engine (Stage C / Stage D).
///
/// - `latest_title` — last OSC 0 or OSC 2 payload, sanitized. An empty
///   payload (e.g. `\x1b]0;\x07`) clears the stored value.
/// - `latest_progress` — last OSC 9 payload (the part after `9;`), stored
///   as-is after sanitization. E.g. `"4;3;"` or `"4;0;"`.
#[derive(Debug, Default)]
pub(super) struct AgentOscStateTracker {
    collector: OscStreamCollector,
    latest_title: Option<String>,
    terminal_title: Option<String>,
    latest_progress: Option<String>,
    primary_screen_escapes: PrimaryScreenEscapeTracker,
    omp_reply_session: Option<Vec<u8>>,
    omp_reply_anchors: VecDeque<OmpReplyAnchor>,
    omp_reply_generation: OmpReplyGenerationToken,
}

impl AgentOscStateTracker {
    #[cfg(test)]
    pub(super) fn observe(&mut self, bytes: &[u8]) -> AgentOscObservation {
        self.observe_on_screen(bytes, false)
    }

    pub(super) fn observe_on_screen(
        &mut self,
        bytes: &[u8],
        alternate_screen: bool,
    ) -> AgentOscObservation {
        self.drop_pruned_omp_reply_anchors();
        self.primary_screen_escapes
            .set_alternate_screen(alternate_screen);

        let (
            collector,
            latest_title,
            terminal_title,
            latest_progress,
            primary_screen_escapes,
            omp_reply_session,
            omp_reply_anchors,
            omp_reply_generation,
        ) = (
            &mut self.collector,
            &mut self.latest_title,
            &mut self.terminal_title,
            &mut self.latest_progress,
            &mut self.primary_screen_escapes,
            &mut self.omp_reply_session,
            &mut self.omp_reply_anchors,
            &mut self.omp_reply_generation,
        );
        let mut observation = AgentOscObservation::default();

        for (index, &byte) in bytes.iter().enumerate() {
            if primary_screen_escapes.observe(byte) {
                omp_reply_anchors.clear();
                observation.reply_anchor_events.clear();
                observation.reset_reply_selection = true;
            }
            collector.observe_byte(byte, &mut |body| {
                let Some((command, payload)) = parse_agent_osc_body(body) else {
                    return;
                };
                match command {
                    b"0" | b"2" => {
                        let title = sanitize_agent_osc_string(payload, AGENT_OSC_MAX_CHARS);
                        let title = (!title.is_empty()).then_some(title);
                        observation.terminal_title_changed |= *terminal_title != title;
                        *terminal_title = title.clone();
                        *latest_title = title;
                    }
                    b"9" => {
                        *latest_progress =
                            Some(sanitize_agent_osc_string(payload, AGENT_OSC_MAX_CHARS));
                    }
                    b"133" if !primary_screen_escapes.alternate_screen() => {
                        let Some(aid) = omp_reply_anchor(payload) else {
                            return;
                        };
                        if omp_reply_session.as_deref() != Some(aid.session) {
                            // The first marker for each process-private session is
                            // ordered PTY evidence newer than any earlier detector token.
                            *omp_reply_generation = omp_reply_generation.next_marker();
                            omp_reply_anchors.clear();
                            *omp_reply_session = Some(aid.session.to_vec());
                            observation.reply_anchor_events.clear();
                            observation.reset_reply_selection = true;
                        }
                        observation.reply_anchor_events.push(PendingOmpReplyAnchor {
                            // Registration occurs after this terminating byte has
                            // reached Ghostty, so an OSC 133 A tracks its fresh
                            // prompt row instead of the preceding row.
                            end_offset: index + 1,
                            anchor_id: aid.id.to_vec(),
                        });
                    }
                    _ => {}
                }
            });
        }
        observation
    }

    pub(super) fn register_omp_reply_anchor(
        &mut self,
        event: PendingOmpReplyAnchor,
        terminal: &crate::ghostty::Terminal,
    ) {
        self.drop_pruned_omp_reply_anchors();

        let Ok(Some(row)) = terminal.track_active_primary_prompt_row() else {
            return;
        };
        if let Some(anchor) = self
            .omp_reply_anchors
            .iter_mut()
            .find(|anchor| anchor.id == event.anchor_id)
        {
            anchor.row = row;
        } else {
            self.omp_reply_anchors.push_back(OmpReplyAnchor {
                id: event.anchor_id,
                row,
            });
        }
    }

    pub(super) fn omp_reply_anchor_rows(&mut self) -> Vec<(Vec<u8>, usize)> {
        let mut rows = Vec::with_capacity(self.omp_reply_anchors.len());
        self.omp_reply_anchors.retain(|anchor| {
            if !anchor.row.is_primary_prompt() {
                return false;
            }
            let Some(row) = anchor.row.screen_row() else {
                return false;
            };
            rows.push((anchor.id.clone(), row));
            true
        });
        rows
    }

    pub(super) fn terminal_title(&self) -> Option<&str> {
        self.terminal_title.as_deref()
    }

    #[cfg(unix)]
    pub(super) fn seed_terminal_title(&mut self, title: Option<String>) {
        self.terminal_title = title;
    }

    /// Returns the latest retained OSC title, or `""` if none has been seen or
    /// the last update was an empty clear.
    #[allow(dead_code)] // used by terminal.rs; full call chain wired in Stage C
    pub(super) fn latest_title(&self) -> &str {
        self.latest_title.as_deref().unwrap_or("")
    }

    /// Returns the latest retained OSC 9 progress payload, or `""` if none.
    #[allow(dead_code)] // used by terminal.rs; full call chain wired in Stage C
    pub(super) fn latest_progress(&self) -> &str {
        self.latest_progress.as_deref().unwrap_or("")
    }

    pub(super) fn clear_omp_reply_anchors(&mut self) {
        self.omp_reply_session = None;
        self.omp_reply_anchors.clear();
    }

    pub(super) fn omp_reply_generation(&self) -> OmpReplyGenerationToken {
        self.omp_reply_generation
    }

    /// Advances only the process generation named by `expected`. A newer marker
    /// survives only when the detector confirmed that it came from a replacement
    /// process; marker order alone cannot suppress an exit reset.
    /// The terminal-core lock orders both decisions against PTY marker parsing.
    pub(super) fn reset_omp_reply_state_if_current(
        &mut self,
        expected: OmpReplyGenerationToken,
        preserve_newer_replacement_marker: bool,
    ) -> bool {
        if self.omp_reply_generation.process != expected.process {
            return false;
        }
        let newer_marker = self.omp_reply_generation.marker_epoch != expected.marker_epoch;
        self.omp_reply_generation = self.omp_reply_generation.next_process();
        if preserve_newer_replacement_marker && newer_marker {
            return false;
        }
        self.clear_omp_reply_anchors();
        self.collector.reset();
        true
    }

    #[cfg(test)]
    pub(super) fn omp_reply_anchor_count(&self) -> usize {
        self.omp_reply_anchors.len()
    }

    fn drop_pruned_omp_reply_anchors(&mut self) {
        // Rebinding a stable AID keeps its logical deque position while moving
        // its row forward, so invalid older anchors can follow a valid front.
        // Retain preserves that navigation order while pruning every stale pin.
        self.omp_reply_anchors
            .retain(|anchor| anchor.row.has_value() && anchor.row.is_primary_prompt());
    }

    /// Drops the retained title and progress so a new foreground agent cannot
    /// inherit OSC evidence emitted by a previous process. The in-flight parse
    /// state is kept: a sequence spanning the agent change finalizes normally
    /// and is attributed to the new agent.
    pub(super) fn clear_retained(&mut self) {
        self.latest_title = None;
        self.latest_progress = None;
    }
}

/// Splits an OSC body at the first `;`, returning `(command, payload)`.
/// Returns `None` if there is no `;`.
fn parse_agent_osc_body(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let sep = body.iter().position(|&b| b == b';')?;
    Some((&body[..sep], &body[sep + 1..]))
}

fn omp_reply_anchor(payload: &[u8]) -> Option<OmpReplyAid<'_>> {
    let mut fields = payload.split(|byte| *byte == b';');
    if fields.next() != Some(b"A".as_slice()) {
        return None;
    }
    let id = fields.find_map(|field| field.strip_prefix(b"aid=omp-response-"))?;
    let separator = id.iter().position(|byte| *byte == b':')?;
    let (session, durable_id) = id.split_at(separator);
    let durable_id = &durable_id[1..];
    (id.len() <= OMP_REPLY_ANCHOR_MAX_BYTES
        && safe_omp_reply_id_part(session)
        && safe_omp_reply_id_part(durable_id))
    .then_some(OmpReplyAid { session, id })
}

fn safe_omp_reply_id_part(value: &[u8]) -> bool {
    !value.is_empty()
        && value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn sanitize_agent_osc_string(payload: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(payload);
    let mut out = String::new();
    for ch in text.chars().filter(|ch| !ch.is_control()).take(max_chars) {
        out.push(ch);
    }
    out
}

/// Reconstructs selected OSC sequences for local evidence capture while
/// debugging agent title/status behavior. This is intentionally passive:
/// nothing here affects terminal rendering or detection state.
#[derive(Debug)]
pub(super) struct OscDebugTracker {
    enabled: bool,
    collector: OscStreamCollector,
    pending: Vec<OscDebugEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OscDebugEvent {
    pub(super) command: String,
    pub(super) payload: String,
}

impl OscDebugTracker {
    pub(super) fn from_env() -> Self {
        Self {
            enabled: osc_debug_enabled_from_env(),
            collector: OscStreamCollector::default(),
            pending: Vec::new(),
        }
    }

    pub(super) fn observe(&mut self, bytes: &[u8]) {
        if !self.enabled {
            return;
        }
        let (collector, pending) = (&mut self.collector, &mut self.pending);
        collector.observe(bytes, |body| {
            if let Some(event) = parse_osc_debug_event(body) {
                pending.push(event);
            }
        });
    }

    pub(super) fn drain_pending(&mut self) -> Vec<OscDebugEvent> {
        std::mem::take(&mut self.pending)
    }
}

impl Default for OscDebugTracker {
    fn default() -> Self {
        Self::from_env()
    }
}

fn osc_debug_enabled_from_env() -> bool {
    std::env::var("HERDR_DEBUG_OSC_EVIDENCE")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn parse_osc_debug_event(body: &[u8]) -> Option<OscDebugEvent> {
    let separator = body.iter().position(|byte| *byte == b';')?;
    let command = &body[..separator];
    let payload = &body[separator + 1..];
    if !matches!(command, b"0" | b"2" | b"9" | b"21337") {
        return None;
    }
    Some(OscDebugEvent {
        command: std::str::from_utf8(command).ok()?.to_string(),
        payload: sanitized_osc_debug_payload(payload),
    })
}

fn sanitized_osc_debug_payload(payload: &[u8]) -> String {
    const MAX_CHARS: usize = 512;
    let text = String::from_utf8_lossy(payload);
    let mut sanitized = String::new();
    for ch in text.chars().filter(|ch| !ch.is_control()).take(MAX_CHARS) {
        sanitized.push(ch);
    }
    if text.chars().count() > MAX_CHARS {
        sanitized.push_str("...");
    }
    sanitized
}

fn parse_file_uri_cwd(uri: &str) -> Option<ReportedCwd> {
    let rest = uri.strip_prefix("file://")?;
    let (authority, path) = if rest.starts_with('/') {
        (None, rest)
    } else {
        let slash = rest.find('/')?;
        let authority = percent_decode_utf8(&rest[..slash])?;
        let authority = (!authority.is_empty()).then_some(authority);
        (authority, &rest[slash..])
    };
    let path = percent_decode_utf8(path)?;

    #[cfg(windows)]
    {
        let mut path = path;
        if path.len() >= 3
            && path.as_bytes()[0] == b'/'
            && path.as_bytes()[2] == b':'
            && path.as_bytes()[1].is_ascii_alphabetic()
        {
            path.remove(0);
        }
        Some((PathBuf::from(path.replace('/', "\\")), authority))
    }

    #[cfg(not(windows))]
    Some((PathBuf::from(path), authority))
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            let hi = *bytes.get(idx + 1)?;
            let lo = *bytes.get(idx + 2)?;
            output.push(hex_value(hi)? * 16 + hex_value(lo)?);
            idx += 3;
        } else {
            output.push(bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn foreground_job_is_shell(job: &crate::platform::ForegroundJob, shell_pid: u32) -> bool {
    job.processes.iter().any(|process| process.pid == shell_pid)
}

pub(super) fn current_transient_default_color_owner(shell_pid: u32) -> Option<u32> {
    let job = crate::detect::foreground_job(shell_pid)?;
    (!foreground_job_is_shell(&job, shell_pid)).then_some(job.process_group_id)
}

fn foreground_job_uses_droid_scrollback_compat(job: &crate::platform::ForegroundJob) -> bool {
    job.processes.iter().any(|process| {
        process.name.eq_ignore_ascii_case("droid")
            || process
                .argv0
                .as_deref()
                .is_some_and(|argv0| argv0.eq_ignore_ascii_case("droid"))
            || process.cmdline.as_deref().is_some_and(|cmdline| {
                cmdline.eq_ignore_ascii_case("droid")
                    || cmdline.starts_with("droid ")
                    || cmdline.to_ascii_lowercase().contains("/droid")
            })
    })
}

pub(super) fn contains_scrollback_clear_sequence(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"\x1b[3J")
        || bytes.windows(5).any(|window| window == b"\x1b[?3J")
}

fn strip_scrollback_clear_sequences<'a>(bytes: &'a [u8]) -> Cow<'a, [u8]> {
    if !contains_scrollback_clear_sequence(bytes) {
        return Cow::Borrowed(bytes);
    }

    let mut filtered = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let remaining = &bytes[index..];
        if remaining.starts_with(b"\x1b[3J") {
            index += 4;
            continue;
        }
        if remaining.starts_with(b"\x1b[?3J") {
            index += 5;
            continue;
        }
        filtered.push(bytes[index]);
        index += 1;
    }

    Cow::Owned(filtered)
}

pub(super) fn maybe_filter_primary_screen_scrollback_clear<'a>(
    bytes: &'a [u8],
    alternate_screen: bool,
    foreground_job: Option<&crate::platform::ForegroundJob>,
) -> Cow<'a, [u8]> {
    // Droid redraws its primary-screen TUI with CSI 3 J, which erases pane
    // scrollback inside herdr. Keep the hack scoped to Droid on the primary
    // screen so normal terminal clear-history behavior still works elsewhere.
    if alternate_screen
        || !contains_scrollback_clear_sequence(bytes)
        || !foreground_job.is_some_and(foreground_job_uses_droid_scrollback_compat)
    {
        return Cow::Borrowed(bytes);
    }

    strip_scrollback_clear_sequences(bytes)
}

#[cfg(target_os = "macos")]
pub(super) fn should_restore_host_terminal_theme(
    owner_pgid: u32,
    shell_pid: u32,
    alternate_screen: bool,
    foreground_job: Option<&crate::platform::ForegroundJob>,
) -> bool {
    if alternate_screen {
        return false;
    }

    let Some(foreground_job) = foreground_job else {
        return false;
    };

    let _ = owner_pgid;
    foreground_job_is_shell(foreground_job, shell_pid)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn should_restore_host_terminal_theme(
    owner_pgid: u32,
    shell_pid: u32,
    alternate_screen: bool,
    foreground_job: Option<&crate::platform::ForegroundJob>,
) -> bool {
    if alternate_screen {
        return false;
    }

    let Some(foreground_job) = foreground_job else {
        return false;
    };

    foreground_job.process_group_id != owner_pgid
        && foreground_job_is_shell(foreground_job, shell_pid)
}

pub(super) fn write_host_terminal_theme(
    terminal: &mut crate::ghostty::Terminal,
    theme: crate::terminal_theme::TerminalTheme,
) {
    write_host_terminal_theme_selective(terminal, theme, true, true);
}

pub(super) fn write_host_terminal_theme_selective(
    terminal: &mut crate::ghostty::Terminal,
    theme: crate::terminal_theme::TerminalTheme,
    foreground: bool,
    background: bool,
) {
    if foreground {
        write_host_default_color(
            terminal,
            crate::terminal_theme::DefaultColorKind::Foreground,
            theme.foreground,
        );
    }
    if background {
        write_host_default_color(
            terminal,
            crate::terminal_theme::DefaultColorKind::Background,
            theme.background,
        );
    }
}

fn write_host_default_color(
    terminal: &mut crate::ghostty::Terminal,
    kind: crate::terminal_theme::DefaultColorKind,
    color: Option<crate::terminal_theme::RgbColor>,
) {
    let sequence = if let Some(color) = color {
        crate::terminal_theme::osc_set_default_color_sequence(kind, color)
    } else {
        crate::terminal_theme::osc_reset_default_color_sequence(kind).to_string()
    };
    terminal.write(sequence.as_bytes());
}

pub(super) fn restore_host_terminal_theme_if_needed(
    core: &mut GhosttyPaneCore,
    pane_id: PaneId,
    shell_pid: u32,
    alternate_screen: bool,
    foreground_job: Option<&crate::platform::ForegroundJob>,
) -> bool {
    let Some(owner_pgid) = core.transient_default_color_owner_pgid else {
        return false;
    };
    if core.host_terminal_theme.is_empty() {
        return false;
    }
    if !should_restore_host_terminal_theme(owner_pgid, shell_pid, alternate_screen, foreground_job)
    {
        return false;
    }

    core.transient_default_color_owner_pgid = None;
    core.child_default_foreground_changed = false;
    core.child_default_background_changed = false;
    write_host_terminal_theme(&mut core.terminal, core.host_terminal_theme);
    info!(
        pane = pane_id.raw(),
        owner_pgid, "restored host terminal default colors after transient override"
    );
    true
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::layout::PaneId;

    fn pane_default_theme(
        pane: &super::super::GhosttyPaneTerminal,
    ) -> crate::terminal_theme::TerminalTheme {
        let mut core = pane.core.lock().unwrap();
        let super::super::terminal::GhosttyPaneCore {
            terminal,
            render_state,
            ..
        } = &mut *core;
        render_state.update(terminal).unwrap();
        let colors = render_state.colors().unwrap();
        crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: colors.foreground.r,
                g: colors.foreground.g,
                b: colors.foreground.b,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: colors.background.r,
                g: colors.background.g,
                b: colors.background.b,
            }),
            ..Default::default()
        }
    }

    fn shell_job(shell_pid: u32) -> crate::platform::ForegroundJob {
        crate::platform::ForegroundJob {
            process_group_id: shell_pid,
            processes: vec![crate::platform::ForegroundProcess {
                pid: shell_pid,
                name: "zsh".to_string(),
                argv0: Some("zsh".to_string()),
                argv: Some(vec!["zsh".to_string()]),
                cmdline: Some("zsh".to_string()),
            }],
        }
    }

    fn tracked_default_color_events(
        events: Vec<DefaultColorTrackedEvent>,
    ) -> Vec<DefaultColorEvent> {
        events.into_iter().map(|event| event.event).collect()
    }

    fn enabled_osc_debug_tracker() -> OscDebugTracker {
        OscDebugTracker {
            enabled: true,
            collector: OscStreamCollector::default(),
            pending: Vec::new(),
        }
    }

    #[test]
    fn osc_stream_collector_honors_ghostty_raw_c1_anywhere_transitions() {
        let mut collector = OscStreamCollector::default();
        let mut bodies = Vec::new();

        collector.observe(
            b"\x1bPqX\x9b?1049h\x1b]9;a\x07\x1bPignored\x9d2;c\x07\x1b%\x9d2;b\x1b\\",
            |body| bodies.push(body.to_vec()),
        );

        assert_eq!(
            bodies,
            vec![b"9;a".to_vec(), b"2;c".to_vec(), b"2;b".to_vec()]
        );
    }

    #[test]
    fn ground_utf8_c1_osc_lookalike_does_not_synthesize_reply_marker() {
        let mut tracker = AgentOscStateTracker::default();
        let generation = tracker.omp_reply_generation();

        let observation = tracker.observe(b"\xC3\x9D133;A;aid=omp-response-forged:reply-1\x07");

        assert!(observation.reply_anchor_events.is_empty());
        assert!(!observation.reset_reply_selection);
        assert_eq!(tracker.omp_reply_generation(), generation);
    }

    #[test]
    fn ground_utf8_c1_csi_lookalike_does_not_clear_primary_replies() {
        let mut tracker = PrimaryScreenEscapeTracker::default();

        for &byte in b"\xC3\x9B3J" {
            assert!(!tracker.observe(byte));
        }
    }

    #[test]
    fn osc_del_is_data_and_cannot_normalize_malformed_reply_marker() {
        let malformed = b"\x1b]13\x7f3;A;aid=omp-response-forged:reply-1\x07";
        let mut collector = OscStreamCollector::default();
        let mut bodies = Vec::new();
        collector.observe(malformed, |body| bodies.push(body.to_vec()));
        assert_eq!(
            bodies,
            vec![b"13\x7f3;A;aid=omp-response-forged:reply-1".to_vec()]
        );

        let mut tracker = AgentOscStateTracker::default();
        let observation = tracker.observe(malformed);
        assert!(observation.reply_anchor_events.is_empty());
        assert!(!observation.reset_reply_selection);
    }

    #[test]
    fn default_color_tracker_detects_split_osc_11_sequences() {
        let mut tracker = DefaultColorOscTracker::default();

        assert!(!tracker.observe(b"\x1b]11;rgb:11/22"));
        assert!(tracker.observe(b"/33\x1b\\"));
    }

    #[test]
    fn default_color_tracker_ignores_osc_queries() {
        let mut tracker = DefaultColorOscTracker::default();

        assert!(!tracker.observe(b"\x1b]10;?\x1b\\"));
        assert!(!tracker.observe(b"\x1b]11;?\x07"));
    }

    #[test]
    fn reported_cwd_parses_file_uri_and_bare_paths() {
        assert_eq!(
            parse_reported_cwd(b"file:///tmp/herdr%20repo"),
            Some((std::path::PathBuf::from("/tmp/herdr repo"), None))
        );
        assert_eq!(
            parse_reported_cwd(b"file://build-host/tmp/herdr%20repo"),
            Some((
                std::path::PathBuf::from("/tmp/herdr repo"),
                Some("build-host".into())
            ))
        );
        assert_eq!(
            parse_reported_cwd(b"C:\\Users\\herdr\\src\\herdr"),
            Some((
                std::path::PathBuf::from("C:\\Users\\herdr\\src\\herdr"),
                None
            ))
        );
        assert_eq!(
            parse_reported_cwd(b"\"C:\\my proj\""),
            Some((std::path::PathBuf::from("C:\\my proj"), None))
        );
    }

    #[test]
    fn reported_cwd_rejects_invalid_or_empty_values() {
        assert_eq!(parse_reported_cwd(b""), None);
        assert_eq!(parse_reported_cwd(b"\xff"), None);
    }

    #[test]
    fn remote_exec_ready_filter_requires_exact_nonce_without_losing_output() {
        let nonce = crate::execution::RemoteExecReadyNonce::generate().unwrap();
        let wrong_nonce = crate::execution::RemoteExecReadyNonce::generate().unwrap();
        let mut filter = RemoteExecReadyFilter::default();
        filter.set_expected_nonce(Some(nonce.clone()));

        let spoof = format!(
            "\x1b]6973;herdr-remote-exec-ready={{\"nonce\":\"{}\",\"hostname\":\"spoof\"}}\x1b\\",
            wrong_nonce.as_str()
        );
        let spoofed = filter.filter(spoof.as_bytes());
        assert!(spoofed.bytes.is_empty());
        assert_eq!(spoofed.ready, None);

        let first = filter.filter(b"before\x1b]6973;herdr-remote-");
        assert_eq!(first.bytes.as_ref(), b"before");
        assert_eq!(first.ready, None);

        let second_payload = format!(
            "exec-ready={{\"nonce\":\"{}\",\"hostname\":\"actual-",
            nonce.as_str()
        );
        let second = filter.filter(second_payload.as_bytes());
        assert!(second.bytes.is_empty());
        assert_eq!(second.ready, None);

        let third = filter.filter(b"node\",\"cwd\":\"/remote/plugin-root\"}\x1b\\after");
        assert_eq!(third.bytes.as_ref(), b"after");
        assert_eq!(
            third.ready,
            Some(RemoteExecReady {
                hostname: Some("actual-node".into()),
                cwd: Some("/remote/plugin-root".into()),
            })
        );
    }

    #[test]
    fn remote_exec_ready_filter_accepts_payloads_beyond_the_old_cwd_limit() {
        let cwd = format!("/{}", "x".repeat(4096));
        let nonce = crate::execution::RemoteExecReadyNonce::generate().unwrap();
        let payload = serde_json::to_vec(&serde_json::json!({
            "nonce": nonce.as_str(),
            "hostname": "actual-node",
            "cwd": cwd,
        }))
        .unwrap();
        assert!(payload.len() > 1024);
        assert!(payload.len() <= crate::execution::REMOTE_EXEC_READY_PAYLOAD_MAX_BYTES);
        let mut marker = crate::execution::REMOTE_EXEC_READY_OSC_PREFIX.to_vec();
        marker.extend_from_slice(&payload);
        marker.extend_from_slice(b"\x1b\\");

        let mut filter = RemoteExecReadyFilter::default();
        filter.set_expected_nonce(Some(nonce));
        let filtered = filter.filter(&marker);

        assert!(filtered.bytes.is_empty());
        assert_eq!(
            filtered.ready,
            Some(RemoteExecReady {
                hostname: Some("actual-node".into()),
                cwd: Some(std::path::PathBuf::from(format!("/{}", "x".repeat(4096)))),
            })
        );
    }

    #[test]
    fn remote_exec_ready_filter_preserves_split_non_marker_osc() {
        let mut filter = RemoteExecReadyFilter::default();
        let mut output = Vec::new();

        output.extend_from_slice(filter.filter(b"\x1b]6973;herdr-remote-exec").bytes.as_ref());
        output.extend_from_slice(filter.filter(b"-other=visible\x1b\\").bytes.as_ref());

        assert_eq!(output, b"\x1b]6973;herdr-remote-exec-other=visible\x1b\\");
    }

    #[test]
    fn remote_exec_ready_filter_discards_control_hostname_but_keeps_ready() {
        let nonce = crate::execution::RemoteExecReadyNonce::generate().unwrap();
        let mut filter = RemoteExecReadyFilter::default();
        filter.set_expected_nonce(Some(nonce.clone()));
        let payload = format!(
            "\x1b]6973;herdr-remote-exec-ready={{\"nonce\":\"{}\",\"hostname\":\"build\\u0007node\"}}\x1b\\",
            nonce.as_str()
        );
        let filtered = filter.filter(payload.as_bytes());

        assert!(filtered.bytes.is_empty());
        assert_eq!(
            filtered.ready,
            Some(RemoteExecReady {
                hostname: None,
                cwd: None,
            })
        );
    }

    // -----------------------------------------------------------------------
    // AgentOscStateTracker tests
    // -----------------------------------------------------------------------

    #[test]
    fn agent_osc_osc0_title_with_bel() {
        let mut t = AgentOscStateTracker::default();
        t.observe("hello\x1b]0;braille title\x07world".as_bytes());
        assert_eq!(t.latest_title(), "braille title");
        assert_eq!(t.terminal_title(), Some("braille title"));
        assert_eq!(t.latest_progress(), "");
    }

    #[test]
    fn agent_osc_osc2_title_with_st() {
        let mut t = AgentOscStateTracker::default();
        t.observe("hello\x1b]2;static title\x1b\\world".as_bytes());
        assert_eq!(t.latest_title(), "static title");
        assert_eq!(t.latest_progress(), "");
    }

    #[test]
    fn agent_osc_empty_osc0_clears_title() {
        let mut t = AgentOscStateTracker::default();
        // First set a title.
        t.observe(b"\x1b]0;some title\x07");
        assert_eq!(t.latest_title(), "some title");
        // Then clear it with an empty payload (Codex pattern).
        t.observe(b"\x1b]0;\x07");
        assert_eq!(t.latest_title(), "");
        assert_eq!(t.terminal_title(), None);
    }

    #[test]
    fn clearing_agent_evidence_preserves_the_terminal_title() {
        let mut tracker = AgentOscStateTracker::default();
        tracker.observe("\x1b]2;✳ 修复🙂标题\x1b\\".as_bytes());

        tracker.clear_retained();

        assert_eq!(tracker.latest_title(), "");
        assert_eq!(tracker.terminal_title(), Some("✳ 修复🙂标题"));
    }

    #[cfg(unix)]
    #[test]
    fn handoff_seed_does_not_restore_agent_detection_evidence() {
        let mut tracker = AgentOscStateTracker::default();

        tracker.seed_terminal_title(Some("✳ restored title".into()));

        assert_eq!(tracker.terminal_title(), Some("✳ restored title"));
        assert_eq!(tracker.latest_title(), "");
    }

    #[test]
    fn agent_osc_osc9_sets_progress_with_bel() {
        let mut t = AgentOscStateTracker::default();
        t.observe(b"\x1b]9;4;3;\x07");
        assert_eq!(t.latest_progress(), "4;3;");
        assert_eq!(t.latest_title(), "");
    }

    #[test]
    fn agent_osc_osc9_clear_progress_with_st() {
        let mut t = AgentOscStateTracker::default();
        t.observe(b"\x1b]9;4;3;\x07");
        assert_eq!(t.latest_progress(), "4;3;");
        t.observe(b"\x1b]9;4;0;\x1b\\");
        assert_eq!(t.latest_progress(), "4;0;");
    }

    #[test]
    fn agent_osc_split_sequence_across_chunks() {
        let mut t = AgentOscStateTracker::default();
        t.observe(b"\x1b]9;4;3");
        assert_eq!(t.latest_progress(), "");
        t.observe(b";\x07");
        assert_eq!(t.latest_progress(), "4;3;");
    }

    #[test]
    fn agent_osc_queues_each_safe_reply_aid_by_process_session() {
        let mut tracker = AgentOscStateTracker::default();
        let initial_generation = tracker.omp_reply_generation();

        let first = tracker.observe(
            b"\x1b]133;A;aid=omp-response-run-a:reply-1\x07\x1b]133;A;redraw=1;aid=omp-response-run-a:reply-1\x1b\\\
              \x1b]133;A;aid=omp-response-run-a:reply-2\x07",
        );
        assert!(first.reset_reply_selection);
        assert_eq!(
            first
                .reply_anchor_events
                .iter()
                .map(|event| event.anchor_id.as_slice())
                .collect::<Vec<_>>(),
            vec![
                b"run-a:reply-1".as_slice(),
                b"run-a:reply-1".as_slice(),
                b"run-a:reply-2".as_slice(),
            ]
        );
        let first_generation = tracker.omp_reply_generation();
        assert_eq!(first_generation.process, initial_generation.process);
        assert_eq!(
            first_generation.marker_epoch,
            initial_generation.marker_epoch.wrapping_add(1)
        );

        let replacement = tracker.observe(b"\x1b]133;A;aid=omp-response-run-b:reply-1\x07");
        assert!(replacement.reset_reply_selection);
        assert_eq!(
            replacement
                .reply_anchor_events
                .iter()
                .map(|event| event.anchor_id.as_slice())
                .collect::<Vec<_>>(),
            vec![b"run-b:reply-1".as_slice()]
        );
        let replacement_generation = tracker.omp_reply_generation();
        assert_eq!(replacement_generation.process, first_generation.process);
        assert_eq!(
            replacement_generation.marker_epoch,
            first_generation.marker_epoch.wrapping_add(1)
        );
    }

    #[test]
    fn agent_osc_discards_alternate_aids_and_resets_before_same_or_split_ed3_replay() {
        let mut tracker = AgentOscStateTracker::default();
        let alternate = tracker.observe_on_screen(
            b"\x1b[?1049h\x1b]133;A;aid=omp-response-forged:reply-1\x07\x1b[?1049l",
            false,
        );
        assert!(alternate.reply_anchor_events.is_empty());
        assert!(!alternate.reset_reply_selection);

        let same_buffer = tracker.observe(
            b"\x1b]133;A;aid=omp-response-run-a:old\x07\x1b[3J\x1b]133;A;aid=omp-response-run-a:fresh\x07",
        );
        assert!(same_buffer.reset_reply_selection);
        assert_eq!(same_buffer.reply_anchor_events[0].anchor_id, b"run-a:fresh");

        let split_start = tracker.observe(b"\x1b[3");
        assert!(!split_start.reset_reply_selection);
        let split_end = tracker.observe(b"J\x1b]133;A;aid=omp-response-run-a:new\x07");
        assert!(split_end.reset_reply_selection);
        assert_eq!(split_end.reply_anchor_events[0].anchor_id, b"run-a:new");

        let oversized = format!(
            "\x1b]133;A;aid=omp-response-run-a:{}\x07",
            "x".repeat(OMP_REPLY_ANCHOR_MAX_BYTES)
        );
        assert!(tracker
            .observe(oversized.as_bytes())
            .reply_anchor_events
            .is_empty());
        assert!(tracker
            .observe(b"\x1b]133;A;aid=omp-response-run-a:reply/unsafe\x07")
            .reply_anchor_events
            .is_empty());
    }

    #[test]
    fn dcs_c1_alternate_transition_cannot_replace_primary_reply_session() {
        let mut tracker = AgentOscStateTracker::default();
        let primary = tracker.observe(b"\x1b]133;A;aid=omp-response-run-a:reply-1\x07");
        assert!(primary.reset_reply_selection);

        let alternate = tracker.observe_on_screen(
            b"\x1bPqX\x9b?1049h\x1b]133;A;aid=omp-response-forged:reply-1\x07",
            false,
        );
        assert!(alternate.reply_anchor_events.is_empty());
        assert!(!alternate.reset_reply_selection);

        let resumed = tracker.observe_on_screen(
            b"\x1b[?1049l\x1b]133;A;aid=omp-response-run-a:reply-2\x1b\\",
            true,
        );
        assert!(!resumed.reset_reply_selection);
        assert_eq!(
            resumed
                .reply_anchor_events
                .iter()
                .map(|event| event.anchor_id.as_slice())
                .collect::<Vec<_>>(),
            vec![b"run-a:reply-2".as_slice()]
        );
    }

    #[test]
    fn primary_screen_escape_tracker_caps_csi_and_keeps_split_ed3() {
        let mut tracker = PrimaryScreenEscapeTracker::default();
        assert!(!tracker.observe(0x1b));
        assert!(!tracker.observe(b'['));
        for _ in 0..=PRIMARY_SCREEN_ESCAPE_MAX_CSI_BYTES {
            assert!(!tracker.observe(b'3'));
        }
        assert!(!tracker.observe(b'J'));

        assert!(!tracker.observe(0x1b));
        assert!(!tracker.observe(b'['));
        assert!(!tracker.observe(b'3'));
        assert!(tracker.observe(b'J'));
    }

    #[test]
    fn agent_osc_bel_and_st_terminators_both_work() {
        let mut t = AgentOscStateTracker::default();
        t.observe(b"\x1b]0;title-bel\x07");
        assert_eq!(t.latest_title(), "title-bel");
        t.observe(b"\x1b]0;title-st\x1b\\");
        assert_eq!(t.latest_title(), "title-st");
    }

    #[test]
    fn agent_osc_oversized_payload_is_discarded_and_recovers() {
        let mut t = AgentOscStateTracker::default();
        // Set a title first.
        t.observe(b"\x1b]0;before\x07");
        assert_eq!(t.latest_title(), "before");

        // Feed an oversized OSC body (> 4096 bytes).
        let mut oversized = Vec::from(b"\x1b]0;".as_slice());
        oversized.extend(std::iter::repeat_n(b'x', 4097));
        oversized.push(0x07);
        t.observe(&oversized);
        // The oversized body is dropped; the previously stored title is kept.
        assert_eq!(t.latest_title(), "before");

        // After recovery, subsequent valid sequences are captured normally.
        t.observe(b"\x1b]0;after\x07");
        assert_eq!(t.latest_title(), "after");
    }

    #[test]
    fn agent_osc_cap_length_is_respected() {
        let mut t = AgentOscStateTracker::default();
        // Build a title of AGENT_OSC_MAX_CHARS + 50 ASCII chars.
        let long_title: String = "a".repeat(AGENT_OSC_MAX_CHARS + 50);
        let seq = format!("\x1b]0;{long_title}\x07");
        t.observe(seq.as_bytes());
        assert_eq!(t.latest_title().len(), AGENT_OSC_MAX_CHARS);
    }

    #[test]
    fn agent_osc_control_chars_stripped() {
        let mut t = AgentOscStateTracker::default();
        t.observe(b"\x1b]0;before\x01after\x07");
        assert_eq!(t.latest_title(), "beforeafter");
    }

    #[test]
    fn agent_osc_unrelated_osc_does_not_overwrite_title() {
        let mut t = AgentOscStateTracker::default();
        t.observe(b"\x1b]0;my title\x07");
        // OSC 4 (palette color), OSC 52 (clipboard) — should not touch title/progress.
        t.observe(b"\x1b]4;1;rgb:aa/bb/cc\x07");
        t.observe(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(t.latest_title(), "my title");
        assert_eq!(t.latest_progress(), "");
    }

    #[test]
    fn agent_osc_interleaved_sequences() {
        let mut t = AgentOscStateTracker::default();
        // OSC 0 title, then OSC 9 progress, then OSC 2 title update.
        t.observe(b"\x1b]0;first\x07\x1b]9;4;3;\x07\x1b]2;second\x07");
        assert_eq!(t.latest_title(), "second");
        assert_eq!(t.latest_progress(), "4;3;");
    }

    #[test]
    fn agent_osc_default_state_is_empty() {
        let t = AgentOscStateTracker::default();
        assert_eq!(t.latest_title(), "");
        assert_eq!(t.latest_progress(), "");
    }

    // -----------------------------------------------------------------------
    // OscDebugTracker tests (existing)
    // -----------------------------------------------------------------------

    #[test]
    fn osc_debug_tracker_detects_title_with_bel() {
        let mut tracker = enabled_osc_debug_tracker();

        tracker.observe("hello\x1b]0;✻ working title\x07world".as_bytes());

        assert_eq!(
            tracker.drain_pending(),
            vec![OscDebugEvent {
                command: "0".to_string(),
                payload: "✻ working title".to_string(),
            }]
        );
    }

    #[test]
    fn osc_debug_tracker_detects_title_with_st() {
        let mut tracker = enabled_osc_debug_tracker();

        tracker.observe("hello\x1b]2;static title\x1b\\world".as_bytes());

        assert_eq!(
            tracker.drain_pending(),
            vec![OscDebugEvent {
                command: "2".to_string(),
                payload: "static title".to_string(),
            }]
        );
    }

    #[test]
    fn osc_debug_tracker_detects_split_status_sequences() {
        let mut tracker = enabled_osc_debug_tracker();

        tracker.observe(b"\x1b]9;4;3");
        assert!(tracker.drain_pending().is_empty());
        tracker.observe(b"\x07\x1b]21337;status=working\x1b\\");

        assert_eq!(
            tracker.drain_pending(),
            vec![
                OscDebugEvent {
                    command: "9".to_string(),
                    payload: "4;3".to_string(),
                },
                OscDebugEvent {
                    command: "21337".to_string(),
                    payload: "status=working".to_string(),
                },
            ]
        );
    }

    #[test]
    fn osc_debug_tracker_ignores_untracked_osc_commands() {
        let mut tracker = enabled_osc_debug_tracker();

        tracker.observe(b"\x1b]52;c;SGVsbG8=\x07\x1b]7;file:///tmp\x07");

        assert!(tracker.drain_pending().is_empty());
    }

    #[test]
    fn osc_debug_tracker_sanitizes_control_characters() {
        let mut tracker = enabled_osc_debug_tracker();

        tracker.observe(b"\x1b]0;before\x01after\x07");

        assert_eq!(
            tracker.drain_pending(),
            vec![OscDebugEvent {
                command: "0".to_string(),
                payload: "beforeafter".to_string(),
            }]
        );
    }

    #[test]
    fn osc_debug_tracker_recovers_after_oversized_payload() {
        let mut tracker = enabled_osc_debug_tracker();
        let oversized = vec![b'a'; 4097];

        tracker.observe(b"\x1b]0;");
        tracker.observe(&oversized);
        tracker.observe(b"\x07\x1b]0;ok\x07");

        assert_eq!(
            tracker.drain_pending(),
            vec![OscDebugEvent {
                command: "0".to_string(),
                payload: "ok".to_string(),
            }]
        );
    }

    #[test]
    fn default_color_event_tracker_detects_queries_sets_and_resets() {
        let mut tracker = DefaultColorEventTracker::default();

        tracker.observe(
            b"\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07\x1b]4;0;?\x07\x1b]10;rgb:11/22/33\x07\x1b]111\x07",
        );

        assert_eq!(
            tracked_default_color_events(tracker.drain_pending()),
            vec![
                DefaultColorEvent::Query(DefaultColorQuery::Foreground),
                DefaultColorEvent::Query(DefaultColorQuery::Background),
                DefaultColorEvent::Query(DefaultColorQuery::Cursor),
                DefaultColorEvent::PaletteQuery(0),
                DefaultColorEvent::Set(DefaultColorQuery::Foreground),
                DefaultColorEvent::Reset(DefaultColorQuery::Background),
            ]
        );
    }

    #[test]
    fn default_color_event_tracker_tracks_each_multi_value_set() {
        let mut tracker = DefaultColorEventTracker::default();

        tracker.observe(
            b"\x1b]10;rgb:11/22/33;rgb:44/55/66\x1b\\\x1b]10;?;rgb:77/88/99\x1b\\\x1b]10;;rgb:aa/bb/cc\x1b\\",
        );

        assert_eq!(
            tracked_default_color_events(tracker.drain_pending()),
            vec![
                DefaultColorEvent::Set(DefaultColorQuery::Foreground),
                DefaultColorEvent::Set(DefaultColorQuery::Background),
                DefaultColorEvent::Set(DefaultColorQuery::Background),
                DefaultColorEvent::Set(DefaultColorQuery::Foreground),
            ]
        );
    }

    #[test]
    fn default_color_event_tracker_handles_split_default_color_queries() {
        let mut tracker = DefaultColorEventTracker::default();

        tracker.observe(b"\x1b]11");
        assert!(tracker.drain_pending().is_empty());
        tracker.observe(b";?\x1b");
        assert!(tracker.drain_pending().is_empty());
        tracker.observe(b"\\");

        assert_eq!(
            tracked_default_color_events(tracker.drain_pending()),
            vec![DefaultColorEvent::Query(DefaultColorQuery::Background)]
        );
    }

    #[test]
    fn default_color_event_tracker_handles_split_palette_color_queries() {
        let mut tracker = DefaultColorEventTracker::default();

        tracker.observe(b"\x1b]4;25");
        assert!(tracker.drain_pending().is_empty());
        tracker.observe(b"5;?\x1b");
        assert!(tracker.drain_pending().is_empty());
        tracker.observe(b"\\");

        assert_eq!(
            tracked_default_color_events(tracker.drain_pending()),
            vec![DefaultColorEvent::PaletteQuery(255)]
        );
    }

    #[test]
    fn default_color_event_tracker_rejects_malformed_palette_color_queries() {
        let mut tracker = DefaultColorEventTracker::default();

        tracker.observe(b"\x1b]4;;?\x07");
        tracker.observe(b"\x1b]4;-1;?\x07");
        tracker.observe(b"\x1b]4;256;?\x07");
        tracker.observe(b"\x1b]4;0;?;1;?\x07");
        tracker.observe(b"\x1b]4;0;rgb:1111/2222/3333\x07");
        tracker.observe(b"\x1b]4;0;?\x07");

        assert_eq!(
            tracked_default_color_events(tracker.drain_pending()),
            vec![DefaultColorEvent::PaletteQuery(0)]
        );
    }

    #[test]
    fn default_color_event_tracker_ignores_other_osc_and_dcs_payloads() {
        let mut tracker = DefaultColorEventTracker::default();

        tracker.observe(b"\x1b]0;title\x07");
        tracker.observe(b"\x1b]52;c;?\x07");
        tracker.observe(b"\x1bPtmux;\x1b\x1b]11;?\x07\x1b\\");
        tracker.observe(b"\x1bPtmux;payload\x07\x1b]11;?\x07\x1b\\");

        assert!(tracker.drain_pending().is_empty());
    }

    #[test]
    fn default_color_event_tracker_ignores_oversized_osc_until_terminator() {
        let mut tracker = DefaultColorEventTracker::default();
        let mut oversized = Vec::from(b"\x1b]11;".as_slice());
        oversized.extend(std::iter::repeat_n(b'a', 1025));
        oversized.extend_from_slice(b"\x1b]11;?\x07");

        tracker.observe(&oversized);
        assert!(tracker.drain_pending().is_empty());

        tracker.observe(b"\x1b]11;?\x07");
        assert_eq!(
            tracked_default_color_events(tracker.drain_pending()),
            vec![DefaultColorEvent::Query(DefaultColorQuery::Background)]
        );
    }

    #[test]
    fn droid_scrollback_compat_matches_process_name_and_cmdline() {
        let name_only = crate::platform::ForegroundJob {
            process_group_id: 42,
            processes: vec![crate::platform::ForegroundProcess {
                pid: 42,
                name: "droid".to_string(),
                argv0: None,
                argv: Some(vec![
                    "/opt/factory/droid".to_string(),
                    "--resume".to_string(),
                ]),
                cmdline: Some("/opt/factory/droid --resume".to_string()),
            }],
        };
        assert!(foreground_job_uses_droid_scrollback_compat(&name_only));

        let cmdline_only = crate::platform::ForegroundJob {
            process_group_id: 42,
            processes: vec![crate::platform::ForegroundProcess {
                pid: 42,
                name: "bun".to_string(),
                argv0: Some("bun".to_string()),
                argv: Some(vec![
                    "bun".to_string(),
                    "/home/can/.local/bin/droid".to_string(),
                    "--resume".to_string(),
                ]),
                cmdline: Some("/home/can/.local/bin/droid --resume".to_string()),
            }],
        };
        assert!(foreground_job_uses_droid_scrollback_compat(&cmdline_only));

        let shell = shell_job(7);
        assert!(!foreground_job_uses_droid_scrollback_compat(&shell));
    }

    #[test]
    fn strip_scrollback_clear_sequences_removes_ed3_only() {
        let filtered = strip_scrollback_clear_sequences(b"a\x1b[3Jb\x1b[?3Jc\x1b[2Jd");
        assert_eq!(filtered.as_ref(), b"abc\x1b[2Jd");
    }

    #[test]
    fn primary_screen_droid_compat_ignores_scrollback_clear_only_for_droid() {
        let droid_job = crate::platform::ForegroundJob {
            process_group_id: 42,
            processes: vec![crate::platform::ForegroundProcess {
                pid: 42,
                name: "droid".to_string(),
                argv0: Some("droid".to_string()),
                argv: Some(vec!["droid".to_string()]),
                cmdline: Some("droid".to_string()),
            }],
        };

        let filtered = maybe_filter_primary_screen_scrollback_clear(
            b"\x1b[3J\x1b[2J",
            false,
            Some(&droid_job),
        );
        assert_eq!(filtered.as_ref(), b"\x1b[2J");

        let shell = maybe_filter_primary_screen_scrollback_clear(
            b"\x1b[3J\x1b[2J",
            false,
            Some(&shell_job(7)),
        );
        assert_eq!(shell.as_ref(), b"\x1b[3J\x1b[2J");

        let alternate =
            maybe_filter_primary_screen_scrollback_clear(b"\x1b[3J\x1b[2J", true, Some(&droid_job));
        assert_eq!(alternate.as_ref(), b"\x1b[3J\x1b[2J");
    }

    #[test]
    fn host_theme_restore_waits_for_shell_and_non_alternate_screen() {
        assert!(!should_restore_host_terminal_theme(
            42,
            7,
            true,
            Some(&shell_job(7)),
        ));
        assert!(!should_restore_host_terminal_theme(42, 7, false, None));
        assert!(!should_restore_host_terminal_theme(
            42,
            7,
            false,
            Some(&crate::platform::ForegroundJob {
                process_group_id: 42,
                processes: vec![crate::platform::ForegroundProcess {
                    pid: 42,
                    name: "droid".to_string(),
                    argv0: Some("droid".to_string()),
                    argv: Some(vec!["droid".to_string()]),
                    cmdline: Some("droid".to_string()),
                }],
            }),
        ));
        assert!(should_restore_host_terminal_theme(
            42,
            7,
            false,
            Some(&shell_job(7)),
        ));

        #[cfg(target_os = "macos")]
        assert!(should_restore_host_terminal_theme(
            7,
            7,
            false,
            Some(&shell_job(7)),
        ));

        #[cfg(not(target_os = "macos"))]
        assert!(!should_restore_host_terminal_theme(
            7,
            7,
            false,
            Some(&shell_job(7)),
        ));
    }

    #[test]
    fn restore_host_terminal_theme_reapplies_cached_colors() {
        let (tx, _rx) = mpsc::channel(4);
        let terminal = crate::ghostty::Terminal::new(80, 24, 0).unwrap();
        let pane = super::super::GhosttyPaneTerminal::new(terminal, tx).unwrap();
        let pane_id = PaneId::from_raw(1);
        let shell_pid = 7;
        let host_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
            ..Default::default()
        };

        pane.apply_host_terminal_theme(host_theme);
        {
            let mut core = pane.core.lock().unwrap();
            core.transient_default_color_owner_pgid = Some(42);
            core.terminal.write(b"\x1b]11;rgb:dd/ee/ff\x1b\\");
        }
        assert_eq!(
            pane_default_theme(&pane).background,
            Some(crate::terminal_theme::RgbColor {
                r: 0xdd,
                g: 0xee,
                b: 0xff,
            })
        );

        {
            let mut core = pane.core.lock().unwrap();
            assert!(restore_host_terminal_theme_if_needed(
                &mut core,
                pane_id,
                shell_pid,
                false,
                Some(&shell_job(shell_pid)),
            ));
        }

        assert_eq!(pane_default_theme(&pane).background, host_theme.background);
        assert_eq!(pane_default_theme(&pane).foreground, host_theme.foreground);
    }
}
