//! Paints Herdr pane images on the outer terminal as DEC Sixel frames.
//!
//! Sixel has no placement, z-order, or image-delete concept: a frame paints over
//! the text cells it covers and stays until something is written over it. So the
//! host-side transport works by
//!
//! * painting each visible placement as one frame at its cell origin, with the
//!   outer terminal in Sixel Display Mode (`CSI ? 80 h`), which stops sixel from
//!   scrolling the screen,
//! * repainting a frame whenever its geometry changes, which overwrites the
//!   previous frame,
//! * asking the client for a full repaint when a frame disappears, so the text
//!   under it is written again and the stale pixels go away.
//!
//! Frames are self-contained: each declares its size and defines its colors with
//! explicit RGB, so no terminal palette state is assumed.

use crate::protocol::SurfaceGraphicsAssetKey;
use crate::sixel::encode::encode_frame as encode_sixel;
use std::collections::HashMap;

/// Upper bound for the pixel budget of one pane frame. Keeps a pane from turning a
/// single Sixel frame into tens of megabytes of terminal output when a small pane
/// shows a large source image.
pub(crate) const MAX_SIXEL_TARGET_PIXELS: usize = 1_000_000;

/// One painted Sixel image on the outer terminal, at a cell origin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PaintedFrame {
    pub(crate) row: u16,
    pub(crate) col: u16,
    pub(crate) cols: u32,
    pub(crate) rows: u32,
}

/// Per-connection Sixel painting state, kept next to the Kitty placement cache so a
/// protocol switch can repaint stale frames deterministically.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClientSixelState {
    pub(crate) painted: HashMap<SurfaceGraphicsAssetKey, PaintedFrame>,
}

/// Paints one pane image as a Sixel frame at a cell origin on the outer terminal,
/// scaled to `cols` by `rows` cells, returning the bytes including the cursor
/// positioning that places the frame.
///
/// Returns empty when the frame cannot be painted (zero-sized pane, over the pixel
/// budget, fully transparent source): the placement is skipped, matching the Kitty
/// path for placements too small to paint.
pub(crate) fn encode_frame(
    rgba: &[u8],
    image_width: u32,
    image_height: u32,
    cell_width_px: u32,
    cell_height_px: u32,
    cols: u32,
    rows: u32,
    row: u16,
    col: u16,
) -> Vec<u8> {
    let target_w = u64::from(cols) * u64::from(cell_width_px.max(1));
    let target_h = u64::from(rows) * u64::from(cell_height_px.max(1));
    if target_w * target_h > MAX_SIXEL_TARGET_PIXELS as u64 {
        return Vec::new();
    }
    let (Ok(target_w), Ok(target_h)) = (usize::try_from(target_w), usize::try_from(target_h))
    else {
        return Vec::new();
    };
    // The placement rectangle is the pane's clipping of the image, and a Sixel frame
    // paints every pixel it declares: scaling the source into the whole rectangle
    // stretches it on one axis, which shows as a distorted image and tearing along
    // band boundaries. Fit the source to the rectangle at one scale and paint that,
    // leaving the rows below unpainted.
    let fit_h = {
        let width_px = u64::try_from(target_w).unwrap_or(u64::MAX);
        let height_px = u64::try_from(target_h).unwrap_or(u64::MAX);
        let fit = if image_width == 0 {
            height_px
        } else {
            (u64::from(image_height) * width_px / u64::from(image_width)).max(1)
        };
        usize::try_from(fit.min(height_px)).unwrap_or(target_h)
    };
    // Foot paints a Sixel frame on the cursor's own row once DECGRA asks for no
    // offset, so the frame is anchored at the target cell row.
    let mut out = format!("\x1b[{};{}H", row + 1, col + 1).into_bytes();
    out.extend(encode_sixel(
        rgba,
        usize::try_from(image_width).unwrap_or(usize::MAX),
        usize::try_from(image_height).unwrap_or(usize::MAX),
        target_w,
        fit_h,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_fill(w: u32, h: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..(w * h) {
            out.extend([r, g, b, 255]);
        }
        out
    }

    #[test]
    fn frame_paints_at_the_placement_cell_origin() {
        let rgba = rgba_fill(10, 10, 255, 0, 0);
        let out = encode_frame(&rgba, 10, 10, 8, 16, 2, 2, 3, 5);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with(
                "\x1b[4;6H\x1bP0;1;0;0;1;1;0;0;0;0;0;0q\"0;0;16;16#1;2;100;0;0",
            ),
            "the frame anchors the 0-based row 3 placement at 1-based row 4 col 6, declares a 16x16 width-first extent at the graphics position, and defines its only colour as the first register: {text}"
        );
        assert!(
            text.contains("#1!16~$"),
            "the register paints a full band with RLE: {text}"
        );
        assert!(
            text.ends_with("\x1b\\"),
            "the frame closes with the string terminator: {text}"
        );
        assert!(
            !text.contains("\x1b[2q"),
            "no erase sequence is emitted: {text}"
        );
    }

    #[test]
    fn frame_skips_when_the_target_exceeds_the_budget() {
        let rgba = rgba_fill(10, 10, 0, 255, 0);
        assert!(
            encode_frame(&rgba, 10, 10, 8, 16, 500, 500, 0, 0).is_empty(),
            "frames beyond the pixel budget are skipped"
        );
    }

    #[test]
    fn frame_skips_fully_transparent_sources() {
        let rgba = vec![0u8; 4 * 10 * 10];
        let out = encode_frame(&rgba, 10, 10, 8, 16, 2, 2, 0, 0);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[1;1H",
            "no Sixel is emitted"
        );
    }

    #[test]
    fn frame_header_and_payload_declare_the_same_raster() {
        // A relayed preview frame that carries only its leading bands paints a strip
        // at the top of its placement. The encoder itself emits one band for every
        // declared row, so a header/payload mismatch points at the frame supply.
        let rgba = rgba_fill(840, 480, 200, 120, 40);
        let out = encode_frame(&rgba, 840, 480, 12, 28, 60, 18, 1, 36);
        let body = &out[out.iter().position(|byte| *byte == b'q').unwrap() + 1..out.len() - 2];
        let header = &body[body.iter().position(|byte| *byte == b'"').unwrap() + 1..];
        let header = &header[..header.iter().position(|byte| *byte == b'#').unwrap()];
        let declared: Vec<u32> = header
            .split(|byte: &u8| *byte == b';')
            .filter_map(|field| std::str::from_utf8(field).ok()?.parse().ok())
            .collect();
        let bands = body.iter().filter(|byte| **byte == b'-').count() as u32;
        assert_eq!(
            declared.len(),
            4,
            "DECGRA declares left;top;width;height: {header:?}"
        );
        let declared_height = declared.get(3).copied().unwrap_or(0);
        assert!(
            bands * 6 >= declared_height && bands.saturating_sub(1) * 6 < declared_height,
            "the payload carries every band the header declares, so a short payload is a frame-supply bug: bands={bands} declared={declared_height}"
        );
    }

    #[test]
    fn frame_keeps_the_source_aspect_inside_a_taller_placement() {
        // The live preview placement is clipped to the pane width (60 columns) while
        // the source is 840x480, so a stretch-to-rectangle frame paints 720x504 and
        // distorts the image vertically by a quarter.
        let rgba = rgba_fill(840, 480, 200, 120, 40);
        let out = encode_frame(&rgba, 840, 480, 12, 28, 60, 18, 1, 36);
        let text = String::from_utf8_lossy(&out).into_owned();
        let header = text
            .split('"')
            .nth(1)
            .and_then(|rest| rest.split('#').next())
            .unwrap_or("?")
            .to_string();
        assert_eq!(
            header, "0;0;720;411",
            "the frame fits the source height to the placement width at one scale"
        );
    }
}
