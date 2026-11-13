//! Sixel encoder used for host-side graphics output.
//!
//! This is the output-side counterpart to `decode`. It converts RGBA image bytes
//! into a canonical DEC Sixel frame:
//!
//! * the P1 header uses the 12-parameter DEC layout terminals parse, with the
//!   grid, the aspect ratio overrides and the graphic rectangle left neutral,
//!   and a transparent background,
//! * the raster attributes declare the frame size with DECGRA (`"0;0;W;H`),
//! * every colour the frame paints is defined with explicit percent RGB
//!   (`#Pc;2;R;G;B`) before it is selected, because a terminal's Sixel palette
//!   is not guaranteed to contain the DEC 6x6x6 cube: Foot preloads only the
//!   16 theme colours, and xterm wraps register numbers by its register count,
//! * the palette is built per frame from a 16-levels-per-channel histogram and
//!   each register is defined by the average colour of the pixels that use it,
//!   so a frame keeps the colours it actually has at the register budget
//!   instead of collapsing onto the DEC cube's 6 levels per channel,
//! * Raster Graphics uses ASCII band characters `? ! " # $ % &`, and RLE encodes
//!   runs as `char` for runs of 1-3 and `!` `run` `char` for runs of 4+,
//! * a band line never contains a space and never drops its tail: foot and xterm
//!   ignore one without moving the graphics pen, which folds the columns after a
//!   gap onto the columns before it, and a line that ends early leaves the pen
//!   short of the raster width, so unpainted columns are painted with the
//!   background register the P1 header declares transparent,
//! * the frame closes with the DCS string terminator. Sixel has no image-delete
//!   sequence, so no erasure is emitted; the caller repaints the covered cells.
//!
//! Registers start at 1 (register 0 is the background hole) and stay within
//! 1..=216, which fits every terminal register count Herdr targets.

use std::collections::BTreeMap;

pub const BAND_PIXELS: usize = 6;
pub const MAX_REGISTERS: usize = 216;
pub const TRANSPARENT_INDEX: usize = usize::MAX;
/// Alpha below this paints as the transparent background, not as a colour.
const TRANSPARENT_ALPHA: u8 = 128;
/// Histogram levels per channel when a frame's palette is built.
const BUCKET_LEVELS: usize = 16;
const BUCKET_SHIFT: u8 = 4;
const BUCKET_COUNT: usize = BUCKET_LEVELS * BUCKET_LEVELS * BUCKET_LEVELS;

/// The colour bucket a pixel belongs to while a frame's palette is built.
fn bucket_of(r: u8, g: u8, b: u8) -> usize {
    ((usize::from(r) >> BUCKET_SHIFT) << 8)
        | ((usize::from(g) >> BUCKET_SHIFT) << 4)
        | (usize::from(b) >> BUCKET_SHIFT)
}

/// The colours one frame paints, as the registers it defines.
///
/// Building the palette is a histogram pass plus a mapping pass, so its cost
/// scales with pixels and with the register budget, not with the image's colour
/// count.
pub struct Palette {
    /// The RGB each register is defined with, at index `register - 1`.
    rgb: Vec<[u8; 3]>,
    /// Bucket index -> register index, so the same frame always selects the
    /// same registers and a repaint reuses the same definitions.
    buckets: Vec<usize>,
}

impl Palette {
    /// The register a pixel paints with, `None` when it is transparent.
    fn register(&self, r: u8, g: u8, b: u8, a: u8) -> Option<usize> {
        if a < TRANSPARENT_ALPHA {
            None
        } else {
            Some(self.buckets[bucket_of(r, g, b)] + 1)
        }
    }

    /// The RGB a register was defined with, for tests and round-tripping.
    fn rgb(&self, register: usize) -> [u8; 3] {
        self.rgb
            .get(register.saturating_sub(1))
            .copied()
            .unwrap_or([0, 0, 0])
    }

    #[cfg(test)]
    fn register_count(&self) -> usize {
        self.rgb.len()
    }
}

fn bucket_average(sum: &[u64; 3], count: u64) -> [u8; 3] {
    let count = count.max(1);
    [
        (sum[0] / count).min(255) as u8,
        (sum[1] / count).min(255) as u8,
        (sum[2] / count).min(255) as u8,
    ]
}

/// The kept register whose colour is nearest to a bucket that got no register.
fn nearest_register(rgb: &[[u8; 3]], average: &[u8; 3]) -> usize {
    let target = [
        i64::from(average[0]),
        i64::from(average[1]),
        i64::from(average[2]),
    ];
    rgb.iter()
        .enumerate()
        .min_by_key(|(_, color)| {
            let dr = i64::from(color[0]) - target[0];
            let dg = i64::from(color[1]) - target[1];
            let db = i64::from(color[2]) - target[2];
            dr * dr + dg * dg + db * db
        })
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// Build the palette for `width * height` RGBA pixels.
///
/// Colours are bucketed at 16 levels per channel. The buckets that carry the
/// most pixels get a register, numbered in bucket order up to `MAX_REGISTERS`;
/// a bucket that misses the budget paints with its nearest kept colour, because
/// the limit is the terminal's register count, not the image's colour count.
fn build_palette(rgba: &[u8], width: usize, height: usize) -> Palette {
    let mut counts = [0u64; BUCKET_COUNT];
    let mut sums = [[0u64; 3]; BUCKET_COUNT];
    for pixel in rgba.chunks_exact(4).take(width * height) {
        if pixel[3] < TRANSPARENT_ALPHA {
            continue;
        }
        let bucket = bucket_of(pixel[0], pixel[1], pixel[2]);
        counts[bucket] += 1;
        for channel in 0..3 {
            sums[bucket][channel] += u64::from(pixel[channel]);
        }
    }
    let mut used: Vec<usize> = (0..BUCKET_COUNT)
        .filter(|&bucket| counts[bucket] > 0)
        .collect();
    // Rarest buckets compete for the last registers, so the colours covering the
    // frame keep theirs. The bucket index breaks ties, keeping the palette
    // stable for the same frame.
    used.sort_by_key(|&bucket| (std::cmp::Reverse(counts[bucket]), bucket));
    let mut kept: Vec<usize> = used.iter().copied().take(MAX_REGISTERS).collect();
    kept.sort_unstable();
    let rgb: Vec<[u8; 3]> = kept
        .iter()
        .map(|&bucket| bucket_average(&sums[bucket], counts[bucket]))
        .collect();
    let mut buckets = vec![0usize; BUCKET_COUNT];
    for (index, &bucket) in kept.iter().enumerate() {
        buckets[bucket] = index;
    }
    for &bucket in used.iter().skip(MAX_REGISTERS) {
        let average = bucket_average(&sums[bucket], counts[bucket]);
        buckets[bucket] = nearest_register(&rgb, &average);
    }
    Palette { rgb, buckets }
}

/// DEC 6x6x6 quantization step: 0..=5.
///
/// Test-only: the frame palette is built per frame, so encoding no longer maps
/// through the cube. Kept to assert the cube stays the floor the decoder and
/// older frames assume.
#[cfg(test)]
pub fn quantize(value: u8) -> usize {
    ((value as u16 + 25) / 51) as usize
}

/// One cube level as an 8-bit intensity. Test-only: the encoder defines every
/// register with explicit RGB, so it never reads the cube table back.
#[cfg(test)]
pub fn cube_level(level: usize) -> u8 {
    (level.min(5) * 51) as u8
}

/// The DEC cube register a pixel would map to, `None` when it is transparent.
#[cfg(test)]
pub fn pixel_register(r: u8, g: u8, b: u8, a: u8) -> Option<usize> {
    if a < TRANSPARENT_ALPHA {
        None
    } else {
        Some(1 + quantize(r) * 36 + quantize(g) * 6 + quantize(b))
    }
}

/// The RGB a cube register number was defined with, for tests.
#[cfg(test)]
pub fn register_rgb(register: usize) -> [u8; 3] {
    let value = register.saturating_sub(1);
    [
        cube_level(value / 36),
        cube_level((value / 6) % 6),
        cube_level(value % 6),
    ]
}

/// Resample RGBA to `dst_width * dst_height` with an area (box) average.
///
/// A relayed frame is decoded at the previewer's pixel size and painted on the
/// host's cell grid, so the scale is almost never an integer. Nearest-neighbour
/// sampling drops whole source rows and columns there, which shows as tearing
/// along the six-pixel bands and shimmering detail. Averaging the source pixels
/// each target pixel covers keeps the frame smooth, matching what a previewer's
/// own encoder does when it writes straight to the terminal. Colors are averaged
/// premultiplied by alpha so a transparent edge does not darken its neighbours.
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
    let end_column = |x: usize| ((x + 1) * src_width).div_ceil(dst_width);
    let end_row = |y: usize| ((y + 1) * src_height).div_ceil(dst_height);
    let mut out = Vec::with_capacity(dst_width * dst_height * 4);
    for y in 0..dst_height {
        let sy0 = (y * src_height) / dst_height;
        let sy1 = end_row(y).clamp(sy0 + 1, src_height);
        for x in 0..dst_width {
            let sx0 = (x * src_width) / dst_width;
            let sx1 = end_column(x).clamp(sx0 + 1, src_width);
            let mut premultiplied = [0u64; 3];
            let mut alpha = 0u64;
            let mut covered = 0u64;
            for row in sy0..sy1 {
                for column in sx0..sx1 {
                    let offset = (row * src_width + column) * 4;
                    let Some(pixel) = src.get(offset..offset + 4) else {
                        continue;
                    };
                    let a = u64::from(pixel[3]);
                    for channel in 0..3 {
                        premultiplied[channel] += u64::from(pixel[channel]) * a;
                    }
                    alpha += a;
                    covered += 1;
                }
            }
            let a = (alpha / covered.max(1)) as u8;
            if a == 0 {
                out.extend([0, 0, 0, 0]);
                continue;
            }
            let alpha_u64 = u64::from(a);
            let channels = premultiplied.map(|sum| {
                (sum / covered.max(1) / alpha_u64.max(1))
                    .min(255)
                    .try_into()
                    .unwrap_or(255u8)
            });
            out.extend([channels[0], channels[1], channels[2], a]);
        }
    }
    out
}

fn indexed_image(rgba: &[u8], width: usize, height: usize, palette: &Palette) -> Vec<usize> {
    let mut indexed = vec![TRANSPARENT_INDEX; width * height];
    for (i, pixel) in rgba.chunks_exact(4).enumerate().take(width * height) {
        if let Some(idx) = palette.register(pixel[0], pixel[1], pixel[2], pixel[3]) {
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
/// The line covers the **whole declared width**. Columns the colour does not
/// paint are painted with the background register, which the P1 header declares
/// transparent: they are never left as spaces, because foot's and xterm's
/// decoders ignore a space without moving the graphics pen and fold the columns
/// after a gap onto the columns before it, and a tail is never dropped, because
/// a terminal that does not return the pen to the left margin at a band
/// boundary starts the next band where this line stopped and the frame shears
/// band by band.
pub fn color_band_line(register: usize, mask: &[u8], width: usize) -> Vec<u8> {
    if mask[..width].iter().all(|mask_byte| *mask_byte == 0) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut selected: Option<usize> = None;
    let mut i = 0;
    while i < width {
        let mask_byte = mask[i];
        let mut run = 1;
        while i + run < width && mask[i + run] == mask_byte {
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
/// that do not preload a palette render the frame correctly only because of
/// this block. Percent is the whole-number scale chafa and libsixel write, so
/// any terminal that accepts a relayed frame accepts these definitions too.
pub fn color_definitions(used: &[usize], palette: &Palette) -> Vec<u8> {
    let mut out = Vec::new();
    for &register in used.iter().filter(|&&register| register <= MAX_REGISTERS) {
        let [r, g, b] = palette.rgb(register);
        out.push(b'#');
        out.extend(register.to_string().into_bytes());
        out.extend(b";2;");
        for value in [r, g, b] {
            // Whole-number percent: 8-bit intensities survive to within one
            // level and stay in the integer form every target parses.
            out.extend(
                ((u32::from(value) * 100 + 127) / 255)
                    .to_string()
                    .into_bytes(),
            );
            out.push(b';');
        }
        out.truncate(out.len() - 1);
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
/// thin strip. `PAN` and `PAD` are offsets from the graphics position: Foot moves
/// a nonzero `PAD` down by a whole text row, so the frame is declared at the
/// position it is written at. `PAD == 0` keeps the 1:1 put handler on Foot 1.28.
fn raster_attributes(width: usize, height: usize) -> Vec<u8> {
    format!("\"0;0;{width};{height}").into_bytes()
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
    let palette = build_palette(&scaled, dst_width, dst_height);
    let indexed = indexed_image(&scaled, dst_width, dst_height, &palette);
    let body = encode_body(&indexed, dst_width, dst_height);
    if body.is_empty() {
        return Vec::new();
    }
    let mut out = frame_header();
    out.extend(raster_attributes(dst_width, dst_height));
    out.extend(color_definitions(&used_registers(&indexed), &palette));
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
            body.contains("\"0;0;12;12"),
            "DECGRA declares square pixels and the 12x12 extent at the graphics position: {body}"
        );
        let scaled = scale_rgba(&rgba, 12, 12, 12, 12);
        let palette = build_palette(&scaled, 12, 12);
        let registers = used_registers(&indexed_image(&scaled, 12, 12, &palette));
        for &register in &registers {
            let [r, g, b] = palette.rgb(register);
            let definition = format!(
                "#{register};2;{};{};{}",
                (u32::from(r) * 100 + 127) / 255,
                (u32::from(g) * 100 + 127) / 255,
                (u32::from(b) * 100 + 127) / 255
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
    fn a_frame_keeps_the_colours_the_dec_cube_would_merge() {
        // Two greys inside one DEC cube level. Cube registers map both to the
        // same colour, so an edge between them disappears; a frame palette owns
        // a register per bucket and keeps the edge.
        let mut rgba = Vec::new();
        for _ in 0..6usize {
            for x in 0..16usize {
                let v = if x < 8 { 30u8 } else { 45u8 };
                rgba.extend([v, v, v, 255]);
            }
        }
        let merged = [
            register_rgb(pixel_register(30, 30, 30, 255).unwrap()),
            register_rgb(pixel_register(45, 45, 45, 255).unwrap()),
        ];
        assert_eq!(
            merged[0], merged[1],
            "the compatibility cube merges both greys"
        );
        let decoded = decode_sixel(&encode_frame(&rgba, 16, 6, 16, 6), 6, None).expect("frame");
        let mut colors: Vec<[u8; 3]> = decoded
            .rgba
            .chunks(4)
            .map(|p| [p[0], p[1], p[2]])
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        colors.sort();
        assert_eq!(
            colors,
            [[30, 30, 30], [45, 45, 45]],
            "both greys survive the round trip"
        );
    }

    #[test]
    fn the_palette_caps_at_the_register_budget_and_keeps_the_frequent_colours() {
        let mut rgba = Vec::new();
        for y in 0..20usize {
            for x in 0..20usize {
                // A colour covering 80 pixels, then one colour per pixel.
                let (r, g, b) = if x < 4 {
                    (10, 10, 10)
                } else {
                    (
                        ((x * 13 + y * 7) % 256) as u8,
                        ((x * 29) % 256) as u8,
                        ((y * 37) % 256) as u8,
                    )
                };
                rgba.extend([r, g, b, 255]);
            }
        }
        let palette = build_palette(&rgba, 20, 20);
        assert_eq!(
            palette.register_count(),
            MAX_REGISTERS,
            "the budget is the terminal's register count, not the image's colour count"
        );
        assert!(
            palette.rgb.contains(&[10, 10, 10]),
            "the colour covering a fifth of the frame keeps a register"
        );
        let text = String::from_utf8(encode_frame(&rgba, 20, 20, 20, 20)).expect("utf8");
        let definitions = text
            .split('#')
            .filter(|s| s.starts_with(char::is_numeric) && s.contains(";2;"))
            .count();
        assert_eq!(definitions, MAX_REGISTERS, "one definition per register");
    }

    #[test]
    fn definitions_use_the_whole_number_percent_scale() {
        let text =
            String::from_utf8(encode_frame(&rgba_pattern(24, 12), 24, 12, 24, 12)).expect("utf8");
        let definitions: Vec<&str> = text
            .split('#')
            .filter(|s| s.starts_with(char::is_numeric) && s.contains(";2;"))
            .collect();
        assert!(
            definitions.len() > 1,
            "a pattern frame defines several colors"
        );
        for definition in definitions {
            let fields: Vec<&str> = definition.split(';').take(5).collect();
            assert_eq!(fields[1], "2", "RGB color-space field: {definition}");
            assert_eq!(
                fields.len(),
                5,
                "register, color space, and three channels: {fields:?}"
            );
            let register: usize = fields[0].parse().expect("register number");
            assert!(
                (1..=MAX_REGISTERS).contains(&register),
                "register {register} stays in 1..={MAX_REGISTERS}"
            );
            for field in &fields[2..5] {
                let percent: u8 = field.parse().expect("whole number percent");
                assert!(percent <= 100, "{definition} leaves the percent scale");
            }
        }
    }

    #[test]
    fn repainting_the_same_frame_reuses_the_same_definitions() {
        let rgba = rgba_pattern(30, 12);
        let first = encode_frame(&rgba, 30, 12, 30, 12);
        let second = encode_frame(&rgba, 30, 12, 30, 12);
        assert_eq!(
            first, second,
            "the palette is built deterministically, so a repaint replays byte-identically"
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
            "#9@@#0!8?",
            "runs below four repeat the band character and the tail is painted with the transparent background register so the line spans the whole width"
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
                    columns += run;
                    i += 1;
                }
                b'?'..=b'~' => {
                    columns += 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        columns
    }

    #[test]
    fn every_band_line_covers_its_painted_width() {
        let rgba = rgba_pattern(37, 18);
        let payload = encode_frame(&rgba, 37, 18, 37, 18);
        let text = String::from_utf8_lossy(&payload);
        let body = &text[text.rfind('"').expect("decstra") + 1..];
        let body = body
            .strip_suffix("\x1b\\")
            .expect("the frame closes with the ST");
        let mut checked = 0usize;
        for line in body.split(['-', '$']) {
            let line = line.trim_start_matches('\'');
            if line.is_empty() {
                continue;
            }
            let columns = band_line_columns(line);
            assert_eq!(
                columns, 37,
                "a band line spans the whole declared width, so a terminal that keeps the \
                 graphics pen where a line ended still starts the next band at the left \
                 margin: {line}"
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
        let mut sum = 0i64;
        let mut worst = 0i32;
        for y in 0..12 {
            for x in 0..24 {
                let i = (y * 24 + x) * 4;
                let got = [decoded.rgba[i], decoded.rgba[i + 1], decoded.rgba[i + 2]];
                let expected = [rgba[i], rgba[i + 1], rgba[i + 2]];
                for k in 0..3 {
                    let diff = i32::from(got[k]) - i32::from(expected[k]);
                    sum += diff.abs() as i64;
                    worst = worst.max(diff.abs());
                }
                assert_eq!(decoded.rgba[i + 3], 255, "alpha x={x} y={y}");
            }
        }
        // 288 source colours overflow the register budget, so rare buckets paint
        // with their nearest kept colour. Measured here: mean 4.0, worst 55. The
        // cube path was fixed at 6 levels per channel and could not keep a colour
        // the frame actually carries.
        let mean = sum / (24 * 12 * 3);
        assert!(mean <= 6, "mean channel error {mean}, measured 4");
        assert!(
            worst <= 60,
            "worst channel error {worst}, measured 55 on an over-budget frame"
        );
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

        // The frame paints the colours its palette defines, and the decoder
        // reproduces them exactly. Measured spread against the source for this
        // frame: mean 2.3, worst 7; the DEC cube path measured a worst of 27.
        let palette = build_palette(&rgba, width, height);
        // The definitions are written in whole percent, which is what every
        // target terminal parses, so a register decodes to its percent-rounded
        // colour: 200 is declared 78 and reads back 198.
        let declared =
            |value: u8| -> u8 { ((u32::from(value) * 100 + 127) / 255 * 255 / 100) as u8 };
        let mut worst = 0i32;
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * 4;
                assert_eq!(
                    decoded.rgba[i + 3],
                    255,
                    "pixel ({x},{y}) must be opaque, source rgba {:?}",
                    &rgba[i..i + 4]
                );
                let register = palette
                    .register(rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
                    .expect("a painted pixel selects a register");
                let [r, g, b] = palette.rgb(register).map(declared);
                assert_eq!(
                    [decoded.rgba[i], decoded.rgba[i + 1], decoded.rgba[i + 2]],
                    [r, g, b],
                    "pixel ({x},{y}) must decode to the colour its register declares"
                );
                for channel in 0usize..3 {
                    let diff =
                        (i32::from(decoded.rgba[i + channel]) - i32::from(rgba[i + channel])).abs();
                    worst = worst.max(diff);
                }
            }
        }
        assert!(
            worst <= 8,
            "the frame stays within 8 levels of the source, worst {worst}"
        );
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
                        // 480 painted colours against a 216 register budget: rare
                        // buckets take their nearest kept colour. Measured worst
                        // spread against the source is 10, the cube path was 27.
                        let diff = (i32::from(decoded.rgba[i + channel])
                            - i32::from(rgba[i + channel]))
                        .abs();
                        assert!(
                            diff <= 12,
                            "pixel ({x},{y}) channel {channel} round trip: source {} decoded {}",
                            rgba[i + channel],
                            decoded.rgba[i + channel],
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

        // The palette is built from the frame's own colours, so re-encoding a
        // previewer frame at its own size is lossless: every colour it carries
        // already owns a register. Alpha is preserved exactly: transparent pixels
        // stay transparent, painted pixels stay opaque.
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
                [first[0], first[1], first[2], first[3]],
                "a previewer frame re-encoded at its own size keeps its pixels: first pass \
                 {first:?} second pass {second:?}",
            );
        }
    }

    #[test]
    fn scale_averages_the_source_pixels_a_target_pixel_covers() {
        // A relayed frame is scaled by a non-integer factor onto the host grid.
        // Nearest-neighbour sampling copies one source pixel and drops the rest,
        // which is what tears the frame along its six-pixel bands.
        let src: Vec<u8> = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
        ]
        .concat();
        let out = scale_rgba(&src, 3, 2, 2, 1);
        assert_eq!(out.len(), 8, "two target pixels");
        assert_eq!(
            &out[0..4],
            &[127, 127, 0, 255],
            "the source pixels one target pixel covers are averaged, not sampled"
        );
    }

    #[test]
    fn decgra_declares_the_horizontal_extent_before_the_vertical_extent() {
        let rgba = rgba_pattern(20, 6);
        let payload = encode_frame(&rgba, 20, 6, 20, 6);
        let body = String::from_utf8_lossy(&payload);
        assert!(
            body.contains("\"0;0;20;6"),
            "a 20x6 frame declares the 20px horizontal extent then the 6px vertical extent, the order chafa, foot, xterm and libsixel parse: {body}"
        );
        assert!(
            !body.contains("\"0;0;6;20"),
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
