//! Sixel encoder used for host-side graphics output.
//!
//! This is the output-side counterpart to `decode`. It converts RGBA image bytes
//! into a canonical DEC Sixel frame:
//!
//! * the P1 header uses the 12-parameter DEC layout terminals parse, with the
//!   grid, the aspect ratio overrides and the graphic rectangle left neutral,
//!   and a transparent background,
//! * the raster attributes declare the frame size with DECGRA (`"1;1;W;H`),
//! * every colour the frame paints is defined with explicit percent RGB
//!   (`#Pc;2;R;G;B`) before it is selected, because a terminal's Sixel palette
//!   is not guaranteed to contain the DEC 6x6x6 cube: Foot preloads only the
//!   16 theme colours, and xterm wraps register numbers by its register count,
//! * Raster Graphics uses ASCII band characters `? ! " # $ % &`, and RLE encodes
//!   runs as `char` for runs of 1-3 and `!` `run` `char` for runs of 4+,
//! * a band line never contains a space: foot and xterm ignore one without
//!   moving the graphics pen, which folds the columns after a gap onto the
//!   columns before it, so gaps are painted with the background register the
//!   P1 header declares transparent,
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

fn push_rle(out: &mut Vec<u8>, run: usize, ch: u8) {
    if run >= 4 {
        out.push(b'!');
        out.extend(run.to_string().into_bytes());
        out.push(ch);
    } else {
        for _ in 0..run {
            out.push(ch);
        }
    }
}

/// Encodes one colour's band line: each column's mask byte (bit `y` = row
/// `band * 6 + y`) maps to the band character `mask + 0x3f`, runs of 4+
/// identical masks collapse to `!` `count` `char`, and the line selects the
/// registers it paints.
///
/// Columns the colour does not paint are filled with the background register,
/// which the frame declares transparent in the P1 header. They are never left
/// as spaces: foot's and xterm's decoders ignore a space without moving the
/// graphics pen, so a space gap folds the columns after it back onto the
/// columns before it. Trailing gaps are dropped, keeping the frame compact.
pub fn color_band_line(register: usize, mask: &[u8], width: usize) -> Vec<u8> {
    let Some(last) = mask[..width].iter().rposition(|m| *m != 0) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut selected: Option<usize> = None;
    let mut i = 0;
    while i <= last {
        let mask_byte = mask[i];
        let mut run = 1;
        while i + run <= last && mask[i + run] == mask_byte {
            run += 1;
        }
        let target = if mask_byte == 0 { 0 } else { register };
        if selected != Some(target) {
            out.push(b'#');
            out.extend(target.to_string().into_bytes());
            selected = Some(target);
        }
        push_rle(&mut out, run, mask_byte + 0x3f);
        i += run;
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
            let line = color_band_line(idx, &mask, width);
            if line.is_empty() {
                continue;
            }
            painted = true;
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

/// The P1 header in the layout terminals actually parse: the DEC 12-parameter
/// form `Pmacro;Pbgmode;Phgrid;Pvgrid;Pheight;Pwidth;Pheight_size;Pwidth_size;
/// Pleft;Ptop;Preduce;Pcenter`.
///
/// Foot reads only the first three parameters (`dcs.c`); xterm reads all twelve
/// and uses slots 3 and 4 as a *grid* that the image is scaled to fit, slots 5
/// and 6 as the pixel aspect ratio, and slots 7..10 as a declared graphic
/// rectangle. The grid, the aspect overrides and the rectangle are therefore all
/// left neutral here, and the frame size is declared once by DECGRA.
fn frame_header() -> Vec<u8> {
    // Pmacro 0: terminal default aspect; Pbgmode 1: transparent background, so
    // the pane's own background stays visible under unpainted pixels.
    b"\x1bP0;1;0;0;1;1;0;0;0;0;0;0q".to_vec()
}

/// DECGRA raster attributes: `PAN;PAD;W;H` with square pixels and the horizontal
/// extent before the vertical extent, the order DEC defines and chafa, foot,
/// xterm and libsixel all parse (`pan;pad;pixel horizontal extent;pixel vertical
/// extent`). Declaring them the other way round makes the terminal preallocate a
/// transposed raster buffer, so a frame wider than it is tall is clipped to a
/// thin strip. `PAD == 1` keeps foot on its 1:1 put handler.
fn raster_attributes(width: usize, height: usize) -> Vec<u8> {
    format!("\"1;1;{width};{height}").into_bytes()
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
    let mut out = frame_header();
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

    fn solid_rgba(w: usize, h: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 4);
        for _ in 0..(w * h) {
            v.extend([r, g, b, 255]);
        }
        v
    }

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

    /// Prints the encoded frame for manual inspection on a real terminal.
    /// Quiet by default; run with `HERDR_SIXEL_DUMP=1` for the 24x12 frame,
    /// `HERDR_SIXEL_DUMP=WIDTHxHEIGHT` for a gradient frame of that pixel size,
    /// or `HERDR_SIXEL_DUMP=solid:WIDTHxHEIGHT` for a single-colour frame, which
    /// is the only shape a screenshot measurement can locate unambiguously.
    #[test]
    fn debug_dump() {
        let Some(env) = std::env::var_os("HERDR_SIXEL_DUMP") else {
            return;
        };
        let spec = env.to_string_lossy().to_string();
        let solid = spec.strip_prefix("solid:");
        let pattern = spec.strip_prefix("pattern:");
        let spec = solid.unwrap_or(pattern.unwrap_or(&spec));
        let (width, height): (usize, u32) = match spec.split_once('x') {
            Some((w, h)) => (w.parse().unwrap_or(24), h.parse().unwrap_or(12)),
            None => (24, 12),
        };
        let height = height as usize;
        let rgba = if let Some(solid) = solid {
            let _ = solid;
            solid_rgba(width, height, 255, 0, 255)
        } else if pattern.is_some() {
            pattern_rgba(width, height)
        } else {
            rgba_pattern(width, height)
        };
        let payload = encode_frame(&rgba, width, height, width, height);
        println!("payload: {}", String::from_utf8_lossy(&payload));
        let (scan_w, transparent, drawing) = crate::sixel::decode::scan_pass(&payload);
        println!("scan width={scan_w} transparent={transparent} drawing={drawing}");

        let decoded = decode_sixel(&payload, height as u32, None).unwrap();
        for y in 0..height {
            let mut row = Vec::new();
            for x in 0..width {
                let i = (y * width + x) * 4;
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
        assert_eq!(
            fields.len(),
            12,
            "the P1 header uses the 12-parameter DEC layout terminals parse"
        );
        assert_eq!(fields[0], "0", "Pmacro: the terminal default aspect");
        assert_eq!(
            fields[1], "1",
            "Pbgmode: transparent background keeps the pane background visible"
        );
        assert_eq!(
            fields[2], "0",
            "Phgrid: no grid, so the image is never scaled to fit cells"
        );
        assert_eq!(fields[3], "0", "Pvgrid: no grid");
        assert_eq!(fields[4], "1", "Pheight: square pixels");
        assert_eq!(fields[5], "1", "Pwidth: square pixels");
        assert_eq!(
            fields[6], "0",
            "Pheight_size: no declared graphic rectangle"
        );
        assert_eq!(fields[7], "0", "Pwidth_size: no declared graphic rectangle");
        assert_eq!(fields[8], "0", "Pleft: no declared graphic rectangle");
        assert_eq!(fields[9], "0", "Ptop: no declared graphic rectangle");

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
            assert_eq!(
                body.matches(&definition).count(),
                1,
                "register {register} is defined with explicit RGB exactly once: {body}"
            );
        }
        let selects = body.bytes().filter(|b| *b == b'#').count() - registers.len();
        assert!(
            selects >= body.matches('$').count(),
            "every painted line selects the register it paints: {body}"
        );
    }

    #[test]
    fn band_lines_use_canonical_band_characters() {
        let indexed = vec![17usize; 10 * 6];
        let mut lines = Vec::new();
        encode_band(&indexed, 10, 6, 0, &mut lines);
        assert_eq!(lines.len(), 1, "one colour covers the band");
        assert_eq!(
            String::from_utf8(color_band_line(17, &lines[0].1, 10)).unwrap(),
            "#17!10~",
            "the six-row band character is 0x7e and runs above 3 collapse"
        );
        let mut mask = vec![0u8; 10];
        mask[0] = 1;
        mask[1] = 1;
        mask[2] = 0;
        assert_eq!(
            String::from_utf8(color_band_line(9, &mask, 10)).unwrap(),
            "#9@@",
            "runs below four repeat the band character and trailing gaps are dropped"
        );
    }

    #[test]
    fn an_interior_gap_advances_the_pen_with_the_background_register() {
        let mut mask = vec![0u8; 10];
        mask[0] = 1;
        mask[1] = 1;
        mask[8] = 1;
        mask[9] = 1;
        let line = String::from_utf8(color_band_line(5, &mask, 10)).unwrap();
        assert_eq!(line, "#5@@#0!6?#5@@");
        assert!(
            !line.contains(' '),
            "a decoder that ignores spaces must still advance the pen"
        );
        assert_eq!(
            band_line_columns(&line),
            10,
            "the painted columns land where the raster has them: {line}"
        );
    }

    /// Counts the columns a band line moves the graphics pen across, the way a
    /// decoder that ignores everything but band characters sees them.
    fn band_line_columns(line: &str) -> usize {
        let bytes = line.as_bytes();
        let mut columns = 0usize;
        let mut i = 0usize;
        let mut first_column = None;
        while i < bytes.len() {
            match bytes[i] {
                b'#' => {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                b'!' => {
                    i += 1;
                    let mut run = 0usize;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        run = run * 10 + usize::from(bytes[i] - b'0');
                        i += 1;
                    }
                    let painted = bytes.get(i).copied().unwrap_or(b'?') != b'?';
                    if painted {
                        first_column.get_or_insert(columns);
                    }
                    columns += run;
                    i += 1;
                }
                b'?'..=b'~' => {
                    if bytes[i] != b'?' {
                        first_column.get_or_insert(columns);
                    }
                    columns += 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        first_column.map_or(0, |first| columns - first)
    }

    #[test]
    fn every_band_line_covers_its_painted_width() {
        let rgba = rgba_pattern(37, 18);
        let payload = encode_frame(&rgba, 37, 18, 37, 18);
        let text = String::from_utf8_lossy(&payload);
        let body = &text[text.rfind('"').expect("decstra") + 1..];
        let mut checked = 0usize;
        for line in body.split(['-', '$']) {
            let line = line.trim_start_matches('\'');
            if line.is_empty() {
                continue;
            }
            let columns = band_line_columns(line);
            assert!(
                columns > 0 && columns <= 37,
                "a band line stays inside the declared raster: {line}"
            );
            checked += 1;
        }
        assert!(checked > 0, "the frame has band lines: {text}");
    }

    #[test]
    fn encoded_frames_contain_no_space_gaps() {
        let rgba = rgba_pattern(30, 12);
        let payload = encode_frame(&rgba, 30, 12, 20, 10);
        assert!(payload.windows(2).any(|w| w == b"\x1bP"));
        assert!(
            !payload.contains(&b' '),
            "{}",
            String::from_utf8_lossy(&payload)
        );
    }

    /// Drops every space from a frame, the way foot and xterm read them: their
    /// Sixel handlers ignore a space without moving the graphics pen, so a
    /// decoder fed this stream is stricter than one that honours the skip.
    fn without_spaces(payload: &[u8]) -> Vec<u8> {
        payload.iter().copied().filter(|b| *b != b' ').collect()
    }

    #[test]
    fn frames_survive_a_decoder_that_ignores_spaces() {
        let rgba = rgba_pattern(24, 12);
        let payload = encode_frame(&rgba, 24, 12, 24, 12);
        let strict = decode_sixel(&without_spaces(&payload), 12, None).expect("decode frame");
        let lenient = decode_sixel(&payload, 12, None).expect("decode frame");
        assert_eq!(
            strict.width, lenient.width,
            "ignoring the space gaps must not fold the image narrower"
        );
        assert_eq!(
            strict.rgba, lenient.rgba,
            "the pen lands on the same columns with or without the gaps"
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

    /// Encode real RGBA, decode with Herdr's decoder, and compare every pixel.
    ///
    /// The encoder writes quantized cube colours (`quantize` then `cube_percent`),
    /// so a decoded pixel is the quantized value, not the original: this test pins
    /// that the decoder reproduces exactly what the encoder wrote, pixel by pixel,
    /// for a pattern with many registers and RLE runs of every length.
    /// Encode real RGBA, decode with Herdr's decoder, and compare every pixel.
    ///
    /// The encoder writes quantized cube colours (`quantize` then `cube_percent`),
    /// so a decoded pixel is the quantized value, not the original: this test pins
    /// that the decoder reproduces exactly what the encoder wrote, pixel by pixel,
    /// for a pattern with many registers and RLE runs of every length.
    #[test]
    fn frame_round_trips_pixel_for_pixel() {
        let width = 100usize;
        let height = 24usize;
        let rgba = pattern_rgba(width, height);
        let payload = encode_frame(&rgba, width, height, width, height);
        let decoded = decode_sixel(&payload, height as u32, None).expect("frame decodes");
        assert_eq!(
            (decoded.width as usize, decoded.height as usize),
            (width, height)
        );

        // A channel the encoder quantizes lands at `cube_percent(level) * 255 / 100`,
        // which is exactly `level * 51`.
        let want = |value: u8| (quantize(value) * 51) as u8;
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * 4;
                assert_eq!(
                    decoded.rgba[i + 3],
                    255,
                    "pixel ({x},{y}) must be opaque, source rgba {src:?}",
                    src = &rgba[i..i + 4]
                );
                for channel in 0usize..3 {
                    assert_eq!(
                        decoded.rgba[i + channel],
                        want(rgba[i + channel]),
                        "pixel ({x},{y}) channel {channel}: source {} decoded {}",
                        rgba[i + channel],
                        decoded.rgba[i + channel],
                    );
                }
            }
        }
    }

    /// The same pixel-for-pixel round trip on a frame with transparent pixels:
    /// the encoder omits them, the decoder must leave them transparent, and the
    /// painted pixels must still land at their exact columns.
    #[test]
    fn round_trip_keeps_transparency_and_columns() {
        let width = 40usize;
        let height = 12usize;
        let mut rgba = Vec::new();
        for y in 0..height {
            for x in 0..width {
                if x % 4 < 2 {
                    // A transparent stripe every four columns.
                    rgba.extend([0, 0, 0, 0]);
                } else {
                    rgba.extend([
                        (x * 7 % 256) as u8,
                        (y * 11 % 256) as u8,
                        ((x + y) * 13 % 256) as u8,
                        255,
                    ]);
                }
            }
        }
        let payload = encode_frame(&rgba, width, height, width, height);
        let decoded = decode_sixel(&payload, height as u32, None).expect("frame decodes");
        assert_eq!(
            (decoded.width as usize, decoded.height as usize),
            (width, height)
        );

        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * 4;
                if rgba[i + 3] == 0 {
                    assert_eq!(
                        decoded.rgba[i + 3],
                        0,
                        "transparent pixel ({x},{y}) must stay transparent"
                    );
                } else {
                    assert_eq!(decoded.rgba[i + 3], 255, "pixel ({x},{y}) must be opaque");
                    for channel in 0usize..3 {
                        assert_eq!(
                            decoded.rgba[i + channel],
                            (quantize(rgba[i + channel]) * 51) as u8,
                            "pixel ({x},{y}) channel {channel} round trip"
                        );
                    }
                }
            }
        }
    }

    /// Round trip the pixels Herdr decodes from the captured chafa preview frame:
    /// re-encode them with Herdr's encoder, decode again, and compare pixel by
    /// pixel. This proves the encoder and decoder agree on real previewer data,
    /// including the transparent background runs chafa emits for background colours.
    #[test]
    fn captured_preview_frame_reencodes_to_the_same_pixels() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sixel/");
        let stream = std::fs::read(format!("{dir}lf_preview_frame.six")).expect("fixture present");
        let q = stream
            .iter()
            .position(|byte| *byte == b'q')
            .expect("preamble terminator");
        let st = stream[q..]
            .iter()
            .position(|byte| *byte == b'\\')
            .expect("string terminator");
        let payload = &stream[q + 1..q + 1 + st];
        let bands = payload.iter().filter(|byte| **byte == b'-').count() + 1;
        let image = decode_sixel(payload, (bands * 6) as u32, None).expect("frame decodes");

        let iw = image.width as usize;
        let ih = image.height as usize;
        let payload = encode_frame(&image.rgba, iw, ih, iw, ih);
        let again = decode_sixel(&payload, image.height, None).expect("re-encoded frame decodes");
        assert_eq!((again.width, again.height), (image.width, image.height));

        // The encoder writes colors as 6x6x6 cube percentages, so a second pass
        // quantizes chafa's colors to the nearest cube level. Alpha is preserved
        // exactly: transparent pixels stay transparent, painted pixels stay opaque.
        let quantized = |v: u8| -> u8 {
            let level = (v as u16 + 25) / 51;
            (level * 20 * 255 / 100) as u8
        };
        let painted_first = image.rgba.chunks(4).filter(|p| p[3] == 255).count();
        assert!(
            painted_first > 250,
            "real previewer data paints {painted_first}"
        );
        for i in 0..(iw * ih) {
            let x = i % iw;
            let y = i / iw;
            let first = &image.rgba[i * 4..i * 4 + 4];
            let second = &again.rgba[i * 4..i * 4 + 4];
            assert_eq!(
                second[3], first[3],
                "alpha must survive the round trip at ({x},{y})"
            );
            assert_eq!(
                second,
                [
                    quantized(first[0]),
                    quantized(first[1]),
                    quantized(first[2]),
                    first[3]
                ],
                "pixel ({x},{y}) must land on the 6x6x6 cube: first pass {:?} second pass {:?}",
                first,
                second,
            );
        }
    }

    #[test]
    fn decgra_declares_the_horizontal_extent_before_the_vertical_extent() {
        let rgba = rgba_pattern(20, 6);
        let payload = encode_frame(&rgba, 20, 6, 20, 6);
        let body = String::from_utf8_lossy(&payload);
        assert!(
            body.contains("\"1;1;20;6"),
            "a 20x6 frame declares the 20px horizontal extent then the 6px vertical extent, the order chafa, foot, xterm and libsixel parse: {body}"
        );
        assert!(
            !body.contains("\"1;1;6;20"),
            "the extent is never declared height first, which makes the terminal preallocate a transposed raster and clip a wide frame to a strip: {body}"
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

    /// Three vertical colour blocks plus a brightness ramp: many registers and
    /// RLE runs of every length, with the same colour repeating in a band, and the
    /// blocks rotated by row so band lines differ between rows.
    fn pattern_rgba(width: usize, height: usize) -> Vec<u8> {
        let mut rgba = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let base: (u8, u8, u8) = match (x + y) % 3 {
                    0 => (200, 30, 30),
                    1 => (30, 160, 30),
                    _ => (30, 30, 200),
                };
                let ramp = (x * 24 / width) as u8;
                rgba.extend([base.0 + ramp, base.1 + ramp, base.2 + ramp, 255]);
            }
        }
        rgba
    }
}
