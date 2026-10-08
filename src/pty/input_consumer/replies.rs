//! Closed reply grammar. Only the actor's emulator/appearance funnel calls this.
use super::InputSource;
use bytes::Bytes;
fn numbers(s: &str) -> Option<Vec<u32>> {
    if s.len() > 96 {
        return None;
    }
    s.split(';')
        .map(|n| {
            if n.is_empty() || n.len() > 9 || !n.bytes().all(|b| b.is_ascii_digit()) {
                None
            } else {
                n.parse().ok()
            }
        })
        .collect()
}
fn allowed(frame: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(frame) else {
        return false;
    };
    if s == "\x1bP>|libghostty\x1b\\" {
        return true;
    }
    if let Some(hex) = s
        .strip_prefix("\x1bP!|")
        .and_then(|s| s.strip_suffix("\x1b\\"))
    {
        return hex.len() == 8
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b));
    }
    if let Some(csi) = s.strip_prefix("\x1b[") {
        if csi == "0n" || matches!(csi, "?997;1n" | "?997;2n" | "?999;1n" | "?999;2n") {
            return true;
        }
        if let Some(v) = csi
            .strip_prefix('?')
            .and_then(|s| s.strip_suffix('c'))
            .and_then(numbers)
        {
            return !v.is_empty();
        }
        if let Some(v) = csi
            .strip_prefix('>')
            .and_then(|s| s.strip_suffix('c'))
            .and_then(numbers)
        {
            return v.len() == 3;
        }
        // Cell size report (reply to CSI 16 t): CSI 6 ; height ; width t.
        if let Some(v) = csi.strip_suffix('t').and_then(numbers) {
            return v.len() == 3 && v[0] == 6 && v[1] > 0 && v[2] > 0;
        }
        if let Some(v) = csi.strip_suffix('R').and_then(numbers) {
            return v.len() == 2 && v.iter().all(|n| *n > 0);
        }
        if let Some(v) = csi
            .strip_suffix("$y")
            .and_then(|s| numbers(s.strip_prefix('?').unwrap_or(s)))
        {
            return v.len() == 2 && v[1] <= 4;
        }
        if let Some(v) = csi
            .strip_prefix('?')
            .and_then(|s| s.strip_suffix('u'))
            .and_then(numbers)
        {
            return v.len() == 1 && v[0] <= 31;
        }
    }
    if let Some(osc) = s
        .strip_prefix("\x1b]")
        .and_then(|s| s.strip_suffix('\x07').or_else(|| s.strip_suffix("\x1b\\")))
    {
        let parts = osc.split(';').collect::<Vec<_>>();
        let rgb = match parts.as_slice() {
            ["10" | "11", rgb] => Some(*rgb),
            ["4", index, rgb] if numbers(index).is_some_and(|v| v.len() == 1 && v[0] <= 255) => {
                Some(*rgb)
            }
            _ => None,
        };
        if let Some(rgb) = rgb.and_then(|s| s.strip_prefix("rgb:")) {
            let channels = rgb.split('/').collect::<Vec<_>>();
            return channels.len() == 3
                && channels
                    .iter()
                    .all(|c| c.len() == 4 && c.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }
    false
}
/// Split complete callback batches into independently classified frames. An
/// incomplete callback is unknown immediately; it cannot later gain authority.
pub(crate) fn classify_replies(bytes: Bytes) -> Vec<(Bytes, InputSource)> {
    let mut result = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let tail = &bytes[start..];
        let end = if tail.starts_with(b"\x1b[") {
            tail.iter()
                .enumerate()
                .skip(2)
                .find(|(_, b)| (0x40u8..=0x7e).contains(*b))
                .map(|(i, _)| i + 1)
        } else if tail.starts_with(b"\x1bP") || tail.starts_with(b"\x1b]") {
            let st = tail.windows(2).position(|w| w == b"\x1b\\").map(|i| i + 2);
            let bel = if tail.starts_with(b"\x1b]") {
                tail.iter().position(|b| *b == 7).map(|i| i + 1)
            } else {
                None
            };
            match (st, bel) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        } else {
            None
        };
        let len = end.unwrap_or(tail.len());
        let frame = bytes.slice(start..start + len);
        let source = if allowed(&frame) {
            InputSource::Neutral
        } else {
            InputSource::Unknown
        };
        result.push((frame, source));
        start += len;
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_consumer_closed_reply_grammar() {
        for reply in [
            "\x1b[?1;2c",
            "\x1b[>1;2;3c",
            "\x1bP!|0123ABCD\x1b\\",
            "\x1b[0n",
            "\x1b[12;4R",
            "\x1b[?997;1n",
            "\x1b[?997;2n",
            "\x1b[6;18;9t",
            "\x1b[6;1;1t",
            "\x1b[?25;1$y",
            "\x1b[?31u",
            "\x1bP>|libghostty\x1b\\",
            "\x1b]10;rgb:ffff/0000/ABCD\x07",
            "\x1b]4;255;rgb:1234/5678/abcd\x1b\\",
        ] {
            assert!(allowed(reply.as_bytes()), "{reply:?}");
        }
        for reply in [
            "\x1b]52;c;data\x07",
            "\x1b]lhello\x1b\\",
            "\x1b]Licon\x1b\\",
            "\x1bP1$rhello\x1b\\",
            "\x1bP>|attacker\x1b\\",
            "\x1b[?32u",
            "\x1b[3n",
            "\x1b[21t",
            "\x1b[6;0;9t",
            "\x1b[6;18t",
            "\x1b[6;18;9;1t",
            "\x1b[4;600;800t",
            "\x1b[8;24;80t",
            "\x1b[6;18;9tX",
            "\x1b[6;-1;9t",
            "\x1b[?997;3n",
            "\x1b[?997n",
            "\x1b]12;rgb:ffff/0000/0000\x07",
            "\x1b]4;256;rgb:ffff/0000/0000\x07",
            "\x1b[0nJUNK",
            "\x1b[?1",
            "\x1b[?1;;2c",
        ] {
            assert!(!allowed(reply.as_bytes()), "{reply:?}");
        }
        let pi = classify_replies(Bytes::from_static(b"\x1b[6;18;9t\x1b[?997;2n"));
        assert_eq!(pi.len(), 2);
        assert!(pi.iter().all(|(_, s)| *s == InputSource::Neutral));
        let mixed = classify_replies(Bytes::from_static(b"\x1b[0n\x1b]52;c;evil\x07\x1b[1;2R"));
        assert_eq!(
            mixed.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>(),
            vec![
                InputSource::Neutral,
                InputSource::Unknown,
                InputSource::Neutral
            ]
        );
        for bytes in [
            b"\x1b[0n\x1b[?".as_slice(),
            b"\x1b]10;rgb:ffff/0000/0000".as_slice(),
            b"\x9b0n".as_slice(),
            b"\x901$rtitle\x9c".as_slice(),
            b"\x1b]Licon\x1b\\".as_slice(),
            b"\x1b[?1;;2c".as_slice(),
        ] {
            assert!(
                classify_replies(Bytes::copy_from_slice(bytes))
                    .iter()
                    .any(|(_, s)| *s == InputSource::Unknown),
                "{bytes:?}"
            );
        }
    }
}
