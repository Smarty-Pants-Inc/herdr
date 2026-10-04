//! Compatibility decoder for scroll-aware retained patches from older servers.
//!
//! Local server detection is disabled pending smarty-dev#4637. Receivers retain
//! the codec so attaching to a server that already supports it remains valid.

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};

use super::{
    CellData, PaneSurfacePatch, PaneSurfacePatchRow, ServerMessage, SurfaceRect, MAX_FRAME_SIZE,
};

pub(crate) const CAPABILITY: &str = "surface_scroll";
pub(crate) const MESSAGE_KIND: &str = "endpoint.surface-scroll.v1";
const MAX_SCROLLS: usize = 64;
const SCROLL_BYTES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SurfaceScroll {
    pub(crate) rect: SurfaceRect,
    pub(crate) shift: i16,
}

#[derive(Debug)]
pub(crate) struct ScrollPatch {
    pub(crate) scrolls: Vec<SurfaceScroll>,
    pub(crate) patch: PaneSurfacePatch,
}

fn for_each_swap(height: usize, shift: i16, mut swap: impl FnMut(usize, usize)) {
    let distance = usize::from(shift.unsigned_abs());
    if shift > 0 {
        for y in 0..height - distance {
            swap(y, y + distance);
        }
    } else {
        for y in (distance..height).rev() {
            swap(y, y - distance);
        }
    }
}

fn scroll_fits(scroll: &SurfaceScroll, width: u16, height: u16) -> bool {
    let rect = scroll.rect;
    rect.width > 0
        && rect.height >= 2
        && scroll.shift != 0
        && scroll.shift.unsigned_abs() < rect.height
        && rect
            .x
            .checked_add(rect.width)
            .is_some_and(|end| end <= width)
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|end| end <= height)
}

fn rects_overlap(a: SurfaceRect, b: SurfaceRect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

fn scrolls_disjoint(scrolls: &[SurfaceScroll]) -> bool {
    scrolls.iter().enumerate().all(|(index, scroll)| {
        scrolls[index + 1..]
            .iter()
            .all(|other| !rects_overlap(scroll.rect, other.rect))
    })
}

fn row_fits(row: &PaneSurfacePatchRow, width: u16, height: u16) -> bool {
    row.y < height && usize::from(row.x).saturating_add(row.cells.len()) <= usize::from(width)
}

fn apply_scroll(cells: &mut [CellData], width: u16, scroll: &SurfaceScroll) {
    let rect = scroll.rect;
    let stride = usize::from(width);
    let row_start = |y: usize| (usize::from(rect.y) + y) * stride + usize::from(rect.x);
    for_each_swap(usize::from(rect.height), scroll.shift, |a, b| {
        let (a, b) = (row_start(a), row_start(b));
        for x in 0..usize::from(rect.width) {
            cells.swap(a + x, b + x);
        }
    });
}

pub(crate) fn decode(data: &str) -> Result<ScrollPatch, String> {
    let limit = base64::encoded_len(MAX_FRAME_SIZE, false).unwrap_or(usize::MAX);
    if data.len() > limit {
        return Err("surface scroll exceeds the frame limit".into());
    }
    let bytes = STANDARD_NO_PAD
        .decode(data)
        .map_err(|error| format!("invalid surface scroll: {error}"))?;
    let count = usize::from(*bytes.first().ok_or("empty surface scroll")?);
    if count == 0 || count > MAX_SCROLLS {
        return Err("surface scroll has an invalid scroll count".into());
    }
    let header = 1 + count * SCROLL_BYTES;
    if bytes.len() < header {
        return Err("surface scroll is truncated".into());
    }
    let scrolls = bytes[1..header]
        .chunks_exact(SCROLL_BYTES)
        .map(|chunk| {
            let value = |at: usize| u16::from_le_bytes([chunk[at], chunk[at + 1]]);
            SurfaceScroll {
                rect: SurfaceRect {
                    x: value(0),
                    y: value(2),
                    width: value(4),
                    height: value(6),
                },
                shift: i16::from_le_bytes([chunk[8], chunk[9]]),
            }
        })
        .collect();
    let mut frame = &bytes[header..];
    let decoded = super::read_message::<_, ServerMessage>(&mut frame, MAX_FRAME_SIZE);
    if !frame.is_empty() {
        return Err("surface scroll has trailing bytes".into());
    }
    match decoded {
        Ok(ServerMessage::PaneSurfacePatch(patch)) => Ok(ScrollPatch { scrolls, patch }),
        Ok(_) => Err("surface scroll does not carry a pane patch".into()),
        Err(error) => Err(format!("invalid surface scroll patch: {error}")),
    }
}

pub(crate) fn apply(
    cells: &mut [CellData],
    width: u16,
    height: u16,
    scroll_patch: ScrollPatch,
) -> Result<PaneSurfacePatch, String> {
    let ScrollPatch { scrolls, mut patch } = scroll_patch;
    if cells.len() != usize::from(width) * usize::from(height) {
        return Err("surface scroll baseline has an invalid grid".into());
    }
    if !scrolls
        .iter()
        .all(|scroll| scroll_fits(scroll, width, height))
        || !scrolls_disjoint(&scrolls)
        || !patch.rows.iter().all(|row| row_fits(row, width, height))
    {
        return Err("surface scroll exceeds the cell baseline".into());
    }
    for scroll in &scrolls {
        apply_scroll(cells, width, scroll);
    }
    for row in &patch.rows {
        let start = usize::from(row.y) * usize::from(width) + usize::from(row.x);
        cells[start..start + row.cells.len()].clone_from_slice(&row.cells);
    }
    let mut rows = Vec::with_capacity(patch.rows.len());
    for row in std::mem::take(&mut patch.rows) {
        rows.extend(outside_scrolls(row, &scrolls));
    }
    for scroll in &scrolls {
        let rect = scroll.rect;
        for y in rect.y..rect.y + rect.height {
            let start = usize::from(y) * usize::from(width) + usize::from(rect.x);
            rows.push(PaneSurfacePatchRow {
                x: rect.x,
                y,
                cells: cells[start..start + usize::from(rect.width)].to_vec(),
            });
        }
    }
    patch.rows = rows;
    Ok(patch)
}

fn outside_scrolls(
    row: PaneSurfacePatchRow,
    scrolls: &[SurfaceScroll],
) -> Vec<PaneSurfacePatchRow> {
    let start = usize::from(row.x);
    let mut covered = scrolls
        .iter()
        .map(|scroll| scroll.rect)
        .filter(|rect| row.y >= rect.y && row.y - rect.y < rect.height)
        .map(|rect| {
            (
                usize::from(rect.x),
                usize::from(rect.x) + usize::from(rect.width),
            )
        })
        .collect::<Vec<_>>();
    if covered.is_empty() {
        return vec![row];
    }
    covered.sort_unstable();
    let end = start + row.cells.len();
    let mut spans = Vec::new();
    let mut cursor = start;
    for (left, right) in covered.into_iter().chain([(end, end)]) {
        let span_end = left.clamp(cursor, end);
        if span_end > cursor {
            spans.push(PaneSurfacePatchRow {
                x: cursor as u16,
                y: row.y,
                cells: row.cells[cursor - start..span_end - start].to_vec(),
            });
        }
        cursor = cursor.max(right.min(end));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FrameData;
    use ratatui::{buffer::Buffer, layout::Rect};

    fn patch() -> PaneSurfacePatch {
        PaneSurfacePatch {
            boot_id: "boot".into(),
            projection_revision: 1,
            base_surface_revision: 1,
            surface_revision: 2,
            rows: Vec::new(),
            panes: Vec::new(),
            cursor: None,
        }
    }

    #[test]
    fn legacy_scroll_decoder_retains_validation_and_expansion() {
        let frame = FrameData::from_ratatui_buffer(&Buffer::empty(Rect::new(0, 0, 2, 3)), None);
        let scroll = SurfaceScroll {
            rect: SurfaceRect {
                x: 0,
                y: 0,
                width: 2,
                height: 3,
            },
            shift: 1,
        };
        let mut bytes = vec![1];
        for value in [0u16, 0, 2, 3] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&1i16.to_le_bytes());
        super::super::write_message(&mut bytes, &ServerMessage::PaneSurfacePatch(patch())).unwrap();
        let data = STANDARD_NO_PAD.encode(&bytes);
        let mut cells = frame.cells.clone();
        let expanded = apply(&mut cells, 2, 3, decode(&data).unwrap()).unwrap();
        assert_eq!(expanded.rows.len(), 3);
        for invalid in [
            SurfaceScroll {
                rect: SurfaceRect {
                    height: 4,
                    ..scroll.rect
                },
                ..scroll
            },
            SurfaceScroll { shift: 3, ..scroll },
        ] {
            let mut cells = frame.cells.clone();
            assert!(apply(
                &mut cells,
                2,
                3,
                ScrollPatch {
                    scrolls: vec![invalid],
                    patch: patch()
                }
            )
            .is_err());
            assert_eq!(cells, frame.cells);
        }
        assert!(apply(
            &mut cells,
            2,
            3,
            ScrollPatch {
                scrolls: vec![scroll, scroll],
                patch: patch()
            }
        )
        .is_err());
        bytes.push(0);
        assert!(decode(&STANDARD_NO_PAD.encode(bytes)).is_err());
        for data in [
            "".into(),
            STANDARD_NO_PAD.encode([0u8]),
            STANDARD_NO_PAD.encode([1u8, 0, 0]),
        ] {
            assert!(decode(&data).is_err());
        }
    }
}
