//! Sixel encoder used for host-side graphics output.
//!
//! This is the output-side counterpart to `decode`. It converts RGBA image bytes
//! into a canonical DEC Sixel frame:
//!
//! * the P1 header uses the 16-parameter layout real terminals read
//!   (`Pmacro;Pbgmode;Phgrid;…`), with a transparent background,
//! * the raster attributes declare the frame size with DECGRA (`"1;1;H;W`),
//! * every colour the frame paints is defined with explicit percent RGB
//!   (`#Pc;2;R;G;B`) before it is selected, because a terminal's Sixel palette
//!   is not guaranteed to contain the DEC 6x6x6 cube: Foot preloads only the
//!   16 theme colours, and xterm wraps register numbers by its register count,
//! * Raster Graphics uses ASCII band characters `? ! " # $ % &`, and RLE encodes
//!   runs as `char` for runs of 1-3 and `!` `run` `char` for runs of 4+,
//! * the frame closes with the DCS string terminator. Sixel has no image-delete
//!   sequence, so no erasure is emitted; the caller repaints the covered cells.
//!
//! Registers start at 1 (register 0 is the background hole) and stay within
//! 1..=216, which fits every terminal register count Herdr targets.

use std::collections::BTreeMap;

pub const BAND_PIXELS: usize = 6;
pub const MAX_REGISTERS: usize = 216;
pub const TRANSPARENT_INDEX: usize = usize::MAX;

/// DEC 6x6x6 quantization step: 0..=5.
pub fn quantize(value: u8) -> usize {
    ((value as u16 + 25) / 51) as usize
}

/// One cube level as an 8-bit intensity. Test-only: the encoder defines every
/// register with explicit RGB, so it never reads the cube table back.
#[cfg(test)]
pub fn cube_level(level: usize) -> u8 {
    (level.min(5) * 51) as u8
}

/// One cube level in the percent scale the DEC color selector uses.
pub fn cube_percent(level: usize) -> u8 {
    (level.min(5) * 20) as u8
}

/// The color register a pixel maps to, `None` when it is transparent.
///
/// The mapping is stable: the same quantized color always selects the same
/// register, so a repaint of the same image reuses the same definitions.
pub fn pixel_register(r: u8, g: u8, b: u8, a: u8) -> Option<usize> {
    if a < 128 {
        None
    } else {
        Some(1 + quantize(r) * 36 + quantize(g) * 6 + quantize(b))
    }
}

/// The RGB a register number was defined with, for tests and round-tripping.
#[cfg(test)]
pub fn register_rgb(register: usize) -> [u8; 3] {
    let value = register.saturating_sub(1);
    [
        cube_level(value / 36),
        cube_level((value / 6) % 6),
        cube_level(value % 6),
    ]
}

pub fn scale_rgba(
    src: &[u8],
    src_width: usize,
    src_height: usize,
    dst_width: usize,
    dst_height: usize,
) -> Vec<u8> {
    if src_width == 0 || src_height == 0 || dst_width == 0 || dst_height == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(dst_width * dst_height * 4);
    for y in 0..dst_height {
        let sy = (y * src_height) / dst_height;
        for x in 0..dst_width {
            let sx = (x * src_width) / dst_width;
            let offset = (sy * src_width + sx) * 4;
            if offset + 4 <= src.len() {
                out.extend_from_slice(&src[offset..offset + 4]);
            }
        }
    }
    out
}

fn indexed_image(rgba: &[u8], width: usize, height: usize) -> Vec<usize> {
    let mut indexed = vec![TRANSPARENT_INDEX; width * height];
    for (i, pixel) in rgba.chunks_exact(4).enumerate().take(width * height) {
        if let Some(idx) = pixel_register(pixel[0], pixel[1], pixel[2], pixel[3]) {
            indexed[i] = idx;
        }
    }
    indexed
}

/// Collects one band's colour lines: `(register, mask bytes)` where each mask
/// byte has bit `y` set when the pixel at column `x`, row `band * 6 + y` uses
/// that register.
pub fn encode_band(
    indexed: &[usize],
    width: usize,
    height: usize,
    band: usize,
    lines: &mut Vec<(usize, Vec<u8>)>,
) {
    let rows = BAND_PIXELS.min(height.saturating_sub(band * BAND_PIXELS));
    if rows == 0 {
        return;
    }
    let mut masks: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    for x in 0..width {
        for y in 0..rows {
            let idx = indexed[(band * BAND_PIXELS + y) * width + x];
            if idx == TRANSPARENT_INDEX {
                continue;
            }
            let mask = masks.entry(idx).or_insert_with(|| vec![0u8; width]);
            mask[x] |= 1 << y;
        }
    }
    for (idx, mask) in masks {
        lines.push((idx, mask));
    }
}

/// Encodes one colour's band line in canonical RLE: each column's mask byte (bit
/// `y` = row `band * 6 + y`) maps to the band character `mask + 0x3f`, unpainted
/// runs are spaces that advance the pen, and runs of 4+ identical masks collapse
/// to `!` `count` `char`. Trailing empty columns are dropped, keeping the frame
/// compact.
pub fn rle_band_line(mask: &[u8], width: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < width {
        let mask_byte = mask[i];
        let mut run = 1;
        while i + run < width && mask[i + run] == mask_byte {
            run += 1;
        }
        if mask_byte == 0 {
            out.extend(std::iter::repeat_n(b' ', run));
        } else if run >= 4 {
            out.push(b'!');
            out.extend(run.to_string().into_bytes());
            out.push(mask_byte + 0x3f);
        } else {
            for _ in 0..run {
                out.push(mask_byte + 0x3f);
            }
        }
        i += run;
    }
    let last = out.iter().rposition(|b| *b != b' ');
    if let Some(last) = last {
        out.truncate(last + 1);
    }
    out
}

/// The registers a body paints, ascending, so definitions precede use.
pub fn used_registers(indexed: &[usize]) -> Vec<usize> {
    let mut used: Vec<usize> = indexed
        .iter()
        .filter(|&&idx| idx != TRANSPARENT_INDEX)
        .copied()
        .collect();
    used.sort_unstable();
    used.dedup();
    used
}

/// Define every register the body paints with explicit percent RGB. Terminals
/// that do not preload the DEC cube palette render the frame correctly only
/// because of this block.
pub fn color_definitions(used: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    for &register in used.iter().filter(|&&register| register <= MAX_REGISTERS) {
        let value = register.saturating_sub(1);
        let (qr, qg, qb) = (value / 36, (value / 6) % 6, value % 6);
        out.push(b'#');
        out.extend(register.to_string().into_bytes());
        out.extend(b";2;");
        out.extend(cube_percent(qr).to_string().into_bytes());
        out.push(b';');
        out.extend(cube_percent(qg).to_string().into_bytes());
        out.push(b';');
        out.extend(cube_percent(qb).to_string().into_bytes());
    }
    out
}

fn encode_body(indexed: &[usize], width: usize, height: usize) -> Vec<u8> {
    let mut body = Vec::new();
    let mut painted = false;
    for band in 0..height.div_ceil(BAND_PIXELS) {
        let mut lines = Vec::new();
        encode_band(indexed, width, height, band, &mut lines);
        for (idx, mask) in lines {
            let line = rle_band_line(&mask, width);
            if line.is_empty() {
                continue;
            }
            painted = true;
            body.push(b'#');
            body.extend(idx.to_string().into_bytes());
            body.extend(line);
            body.push(b'$');
        }
        body.push(b'-');
    }
    if !painted {
        return Vec::new();
    }
    body
}

/// The 16-parameter P1 header, matching the layout libsixel emits. Terminals
/// read only the first three (`Pmacro`, `Pbgmode`, `Phgrid`) and ignore the rest,
/// so the pane size fields are informational.
fn frame_header(width: usize, height: usize) -> Vec<u8> {
    format!("\x1bP0;1;0;1;1;{width};{height};0;{width};{height};0;0;0;0;1;1q").into_bytes()
}

/// DECGRA raster attributes: `PAN;PAD;V;H` with square pixels and the declared
/// vertical and horizontal extents.
fn raster_attributes(width: usize, height: usize) -> Vec<u8> {
    format!("\"1;1;{height};{width}").into_bytes()
}

/// Encodes `src` scaled to `dst_width` by `dst_height` as one self-contained
/// Sixel frame.
///
/// Returns empty for fully transparent frames and zero-sized targets.
pub fn encode_frame(
    src: &[u8],
    src_width: usize,
    src_height: usize,
    dst_width: usize,
    dst_height: usize,
) -> Vec<u8> {
    let scaled = scale_rgba(src, src_width, src_height, dst_width, dst_height);
    if scaled.is_empty() {
        return Vec::new();
    }
    let indexed = indexed_image(&scaled, dst_width, dst_height);
    let body = encode_body(&indexed, dst_width, dst_height);
    if body.is_empty() {
        return Vec::new();
    }
    let mut out = frame_header(dst_width, dst_height);
    out.extend(raster_attributes(dst_width, dst_height));
    out.extend(color_definitions(&used_registers(&indexed)));
    out.extend(body);
    out.extend(b"\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sixel::decode::decode_sixel;

    fn rgba_pattern(w: usize, h: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for y in 0..h {
            for x in 0..w {
                v.push((x * 255 / w.max(1)) as u8);
                v.push((y * 255 / h.max(1)) as u8);
                v.push(((x + y) * 255 / (w + h).max(1)) as u8);
                v.push(255);
            }
        }
        v
    }

    /// Prints the encoded frame and decoded pixels for manual inspection.
    /// Quiet by default; run with `HERDR_SIXEL_DUMP=1` to dump.
    #[test]
    fn debug_dump() {
        if std::env::var_os("HERDR_SIXEL_DUMP").is_none() {
            return;
        }
        let rgba = rgba_pattern(24, 12);
        let payload = encode_frame(&rgba, 24, 12, 24, 12);
        let decoded = decode_sixel(&payload, 12, None).unwrap();
        println!("payload: {}", String::from_utf8_lossy(&payload));
        for y in 0..12 {
            let mut row = Vec::new();
            for x in 0..24 {
                let i = (y * 24 + x) * 4;
                row.push([decoded.rgba[i], decoded.rgba[i + 1], decoded.rgba[i + 2]]);
            }
            println!("{y} {row:?}");
        }
    }

    fn header_fields(payload: &[u8]) -> Vec<String> {
        let quote = payload
            .iter()
            .position(|b| *b == b'q')
            .expect("header terminator");
        String::from_utf8_lossy(&payload[2..quote])
            .split(';')
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn quantize_is_the_cube_step() {
        assert_eq!(quantize(0), 0);
        assert_eq!(quantize(255), 5);
        assert_eq!(quantize(128), 3);
        assert_eq!(pixel_register(0, 0, 0, 0), None);
        assert_eq!(pixel_register(255, 255, 255, 255), Some(216));
        assert_eq!(pixel_register(0, 0, 0, 255), Some(1));
        let register = pixel_register(128, 64, 32, 255).unwrap();
        assert!(
            (1..=MAX_REGISTERS).contains(&register),
            "register={register}"
        );
    }

    #[test]
    fn scale_drops_out_of_bounds_sources() {
        assert!(scale_rgba(&[], 0, 0, 10, 10).is_empty());
        assert!(scale_rgba(&[0u8; 4], 1, 1, 0, 10).is_empty());
        assert!(encode_frame(&[], 0, 0, 10, 10).is_empty());
        assert!(encode_frame(&[0u8; 4], 1, 1, 0, 10).is_empty());
    }

    #[test]
    fn frame_uses_the_canonical_header_and_explicit_colors() {
        let rgba = rgba_pattern(12, 12);
        let payload = encode_frame(&rgba, 12, 12, 12, 12);
        assert_eq!(&payload[..2], b"\x1bP", "opens as classic Sixel DCS");
        assert!(
            !payload.windows(4).any(|window| window == b"\x1b[2q"),
            "no erase sequence is emitted: Sixel has none"
        );
        assert!(
            payload.ends_with(b"\x1b\\"),
            "the frame closes with the DCS string terminator"
        );

        let fields = header_fields(&payload);
        assert_eq!(fields.len(), 16, "the P1 header has 16 parameters");
        assert_eq!(fields[0], "0", "Pmacro: the default aspect macro");
        assert_eq!(
            fields[1], "1",
            "Pbgmode: transparent background keeps pane background visible"
        );
        assert_eq!(fields[5], "12", "the declared pane width");
        assert_eq!(fields[6], "12", "the declared pane height");

        let body = String::from_utf8_lossy(&payload);
        assert!(
            body.contains("\"1;1;12;12"),
            "DECGRA declares square pixels and the 12x12 extent: {body}"
        );
        let registers = used_registers(&indexed_image(&scale_rgba(&rgba, 12, 12, 12, 12), 12, 12));
        for &register in &registers {
            let [r, g, b] = register_rgb(register);
            let definition = format!(
                "#{register};2;{};{};{}",
                r as usize * 100 / 255,
                g as usize * 100 / 255,
                b as usize * 100 / 255
            );
            assert!(
                body.contains(&definition),
                "register {register} is defined with explicit RGB: {body}"
            );
        }
        assert_eq!(
            body.matches('#').count(),
            registers.len() + body.matches('$').count(),
            "every register is defined once, and every painted line selects one and ends with `$`"
        );
    }

    #[test]
    fn band_lines_use_canonical_band_characters() {
        let indexed = vec![17usize; 10 * 6];
        let mut lines = Vec::new();
        encode_band(&indexed, 10, 6, 0, &mut lines);
        assert_eq!(lines.len(), 1, "one colour covers the band");
        assert_eq!(
            String::from_utf8(rle_band_line(&lines[0].1, 10)).unwrap(),
            "!10~",
            "the six-row band character is 0x7e and runs above 3 collapse"
        );
        let mut mask = vec![0u8; 10];
        mask[0] = 1;
        mask[1] = 1;
        mask[2] = 0;
        assert_eq!(
            String::from_utf8(rle_band_line(&mask, 10)).unwrap(),
            "@@",
            "runs below four repeat the band character, empty columns advance the pen"
        );
    }

    #[test]
    fn frame_round_trips_through_the_decoder() {
        let rgba = rgba_pattern(24, 12);
        let payload = encode_frame(&rgba, 24, 12, 24, 12);
        let decoded = decode_sixel(&payload, 12, None).expect("decode frame");
        assert_eq!((decoded.width, decoded.height), (24, 12));
        for y in 0..12 {
            for x in 0..24 {
                let i = (y * 24 + x) * 4;
                let register = pixel_register(rgba[i], rgba[i + 1], rgba[i + 2], 255).unwrap();
                let expected = register_rgb(register);
                let got = [decoded.rgba[i], decoded.rgba[i + 1], decoded.rgba[i + 2]];
                for k in 0..3 {
                    let diff = i32::from(got[k]) - i32::from(expected[k]);
                    assert!(
                        diff.abs() <= 51,
                        "channel {k} x={x} y={y} got {got:?} expected {expected:?}"
                    );
                }
                assert_eq!(decoded.rgba[i + 3], 255, "alpha x={x} y={y}");
            }
        }
    }

    #[test]
    fn transparent_pixels_are_omitted() {
        let rgba = vec![0u8; 4 * 10 * 6];
        assert!(encode_frame(&rgba, 10, 6, 10, 6).is_empty());
    }

    #[test]
    fn long_runs_collapse_within_their_band() {
        let mut indexed = vec![TRANSPARENT_INDEX; 100 * 6];
        indexed[..100].fill(17); // row 0 of band 0 only
        let payload = encode_frame(&indexed_to_rgba(&indexed), 100, 6, 100, 6);
        let body = String::from_utf8_lossy(&payload);
        assert!(
            body.contains("!100@"),
            "the 100-pixel band line collapses to one run"
        );
        assert!(
            body.matches('@').count() == 1,
            "the painted band emits exactly one band character: {body}"
        );
    }

    fn indexed_to_rgba(indexed: &[usize]) -> Vec<u8> {
        let mut rgba = Vec::new();
        for &idx in indexed {
            if idx == TRANSPARENT_INDEX {
                rgba.extend([0, 0, 0, 0]);
            } else {
                rgba.extend(register_rgb(idx));
                rgba.push(255);
            }
        }
        rgba
    }
}
