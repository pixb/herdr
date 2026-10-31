//! Hand-written DEC SIXEL decoder (zero dependencies).
//!
//! Decodes a Sixel payload (the bytes between the `ESC P … q` header and the
//! terminating ST) into RGBA pixels. The parser covers the subset actually
//! emitted by chafa, img2sixel and ImageMagick:
//!
//! * data chars `0x3f..=0x7e`: six bits each, bit 0 = topmost pixel of the band
//! * `!` `<N>`: repeat the next data char `N` times
//! * `#` `<Pc>` or `#` `<Pc>;<Pu>;<Px>;<Py>[;<Pz>]`: select color
//! * `"` `<P1>;<P2>;<P3>;<P4>`: raster attributes (`P2 == 1` = transparent bg)
//! * ` ` `<N>`: advance `N` pixels right
//! * `$`: return to column 0 within the same band
//! * `-`: move to the next band (6 rows down)
//!
//! Color index `0` resolves to the terminal background (or transparency), the
//! default palette covers DEC 16 colors, the 6x6x6 color cube and 24 grays.

use std::fmt;

/// Safety ceiling for a decoded frame: 4096 x 4096 RGBA (64 MiB).
const MAX_DECODED_BYTES: usize = 4096 * 4096 * 4;
/// Pixels per band (the DEC SIXEL band height).
const BAND_PIXELS: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// RGBA, row-major, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// No drawing characters present.
    Empty,
    /// Decoded size exceeds [`MAX_DECODED_BYTES`].
    TooLarge,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Empty => write!(f, "sixel payload contains no drawing data"),
            DecodeError::TooLarge => write!(f, "sixel image exceeds the decode size limit"),
        }
    }
}

/// Decode a Sixel payload into RGBA pixels.
///
/// `pixel_height` is the frame height in pixels derived from the band count
/// (band separators `-`), as reported by the frame scanner. `background` is
/// the terminal background color used for color index 0 and opaque fills.
pub(crate) fn decode_sixel(
    payload: &[u8],
    pixel_height: u32,
    background: Option<[u8; 4]>,
) -> Result<DecodedImage, DecodeError> {
    let (width, transparent_bg, has_drawing) = scan_pass(payload);
    if !has_drawing {
        return Err(DecodeError::Empty);
    }
    let width = width.max(1);
    let height = pixel_height.max(BAND_PIXELS as u32) as usize;
    let bytes = (width as usize)
        .saturating_mul(height)
        .saturating_mul(4);
    if bytes > MAX_DECODED_BYTES {
        return Err(DecodeError::TooLarge);
    }

    // Fill with the background (opaque) unless the raster attributes asked
    // for transparency.
    let fill = if transparent_bg {
        [0, 0, 0, 0]
    } else {
        background.unwrap_or([0, 0, 0, 255])
    };
    let mut rgba = vec![0u8; bytes];
    for px in rgba.chunks_exact_mut(4) {
        px.copy_from_slice(&fill);
    }

    let stride = width * 4;
    let mut band = 0usize;
    let mut x = 0usize;
    // Color 0 by default; `#` re-selects.
    let mut color = palette_color(0, background, transparent_bg);
    let mut i = 0;
    while i < payload.len() {
        let byte = payload[i];
        match byte {
            b'!' => {
                let (count, next) = digits(payload, i + 1);
                let repeated = payload.get(next).copied();
                i = next;
                if matches!(repeated, Some(0x3f..=0x7e)) {
                    let n = (count.max(1) as usize).min(MAX_DECODED_BYTES);
                    draw_run(&mut rgba, stride, width, band, x, n, color);
                    x = x.saturating_add(n);
                    i = i.saturating_add(1);
                }
            }
            b'#' => {
                if let Some((new_color, next)) = parse_color(payload, i + 1, background, transparent_bg) {
                    color = new_color;
                    i = next;
                } else {
                    i = i.saturating_add(1);
                }
            }
            // Raster attributes were consumed by the scan pass; skip the digits.
            b'"' => {
                let (_, next) = digits(payload, i + 1);
                i = next;
            }
            b' ' => {
                let (count, next) = digits(payload, i + 1);
                x = x.saturating_add(count as usize);
                i = next;
            }
            b'$' => {
                x = 0;
                i = i.saturating_add(1);
            }
            b'-' => {
                band = (band + 1).min(height.div_ceil(BAND_PIXELS).saturating_sub(1));
                x = 0;
                i = i.saturating_add(1);
            }
            0x3f..=0x7e => {
                let bits = u32::from(byte - 0x3f);
                for bit in 0..BAND_PIXELS {
                    if (bits >> bit) & 1 == 1 {
                        let row = band * BAND_PIXELS + bit;
                        if row < height {
                            let offset = row * stride + x.saturating_mul(4);
                            if offset + 4 <= rgba.len() {
                                rgba[offset..offset + 4].copy_from_slice(&color);
                            }
                        }
                    }
                }
                x = x.saturating_add(1);
                i = i.saturating_add(1);
            }
            _ => {
                i = i.saturating_add(1);
            }
        }
    }

    Ok(DecodedImage {
        width: width as u32,
        height: height as u32,
        rgba,
    })
}

/// First pass: derive the image width, the raster transparency flag, and
/// whether any drawing data exists. Costs one linear scan; the second pass
/// can then preallocate an exact buffer.
///
/// Width follows libsixel's `max_x`: only columns that actually paint a
/// pixel count. A trailing `?` (all six rows clear) advances the pen but
/// must not widen the image.
fn scan_pass(payload: &[u8]) -> (usize, bool, bool) {
    let mut width = 0usize;
    let mut x = 0usize;
    let mut max_drawn_x = 0usize;
    let mut transparent_bg = false;
    let mut has_drawing = false;
    let mut i = 0;
    while i < payload.len() {
        let byte = payload[i];
        match byte {
            b' ' => {
                let (count, next) = digits(payload, i + 1);
                x = x.saturating_add(count as usize);
                i = next;
            }
            b'!' => {
                let (count, next) = digits(payload, i + 1);
                if matches!(payload.get(next), Some(0x3f..=0x7e)) {
                    has_drawing = true;
                    let run = count.max(1) as usize;
                    if payload[next] != 0x3f {
                        max_drawn_x = max_drawn_x.max(x + run);
                    }
                    x = x.saturating_add(run);
                    i = next + 1;
                } else {
                    i = next;
                }
            }
            0x3f..=0x7e => {
                has_drawing = true;
                if byte != 0x3f {
                    max_drawn_x = max_drawn_x.max(x + 1);
                }
                x = x.saturating_add(1);
                i = i.saturating_add(1);
            }
            b'$' | b'-' => {
                width = width.max(max_drawn_x);
                x = 0;
                i = i.saturating_add(1);
            }
            b'"' => {
                let (attrs, next) = raster_attrs(payload, i + 1);
                if attrs.len() >= 2 && attrs[1] == 1 {
                    transparent_bg = true;
                }
                i = next;
            }
            _ => {
                i = i.saturating_add(1);
            }
        }
    }
    (width.max(max_drawn_x), transparent_bg, has_drawing)
}

/// Accumulate a run of decimal digits starting at `i`; returns `(value, end)`.
fn digits(payload: &[u8], mut i: usize) -> (u32, usize) {
    let mut value = 0u32;
    while i < payload.len() && payload[i].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add(u32::from(payload[i] - b'0'));
        i += 1;
    }
    (value, i)
}

/// Parse `#` color selectors: either a single palette index (`#Pc`) or the
/// direct color form `#Pc;Pu;Px;Py[;Pz]` with 0..=100 components.
fn parse_color(
    payload: &[u8],
    mut i: usize,
    background: Option<[u8; 4]>,
    transparent_bg: bool,
) -> Option<([u8; 4], usize)> {
    let mut fields = Vec::new();
    let mut value = 0u32;
    let mut saw_digit = false;
    while i < payload.len() {
        let byte = payload[i];
        if byte.is_ascii_digit() {
            value = value
                .saturating_mul(10)
                .saturating_add(u32::from(byte - b'0'));
            saw_digit = true;
            i += 1;
        } else if byte == b';' {
            fields.push(value);
            value = 0;
            saw_digit = false;
            i += 1;
        } else {
            break;
        }
    }
    if saw_digit {
        fields.push(value);
    }
    match fields.len() {
        1 => Some((palette_color(fields[0], background, transparent_bg), i)),
        3..=5 => {
            // Pc;Pu;Px;Py (+ optional Pz alpha).
            let r = scale_component(fields.get(1).copied().unwrap_or(0));
            let g = scale_component(fields.get(2).copied().unwrap_or(0));
            let b = scale_component(fields.get(3).copied().unwrap_or(0));
            let a = if fields.len() >= 5 {
                scale_component(fields[4])
            } else {
                255
            };
            Some(([r, g, b, a], i))
        }
        _ => None,
    }
}

/// Parse `"` raster attributes `P1;P2;P3;P4` (trailing fields may be omitted).
fn raster_attrs(payload: &[u8], mut i: usize) -> (Vec<u32>, usize) {
    let mut attrs = Vec::new();
    let mut value = 0u32;
    let mut saw_digit = false;
    while i < payload.len() {
        let byte = payload[i];
        if byte.is_ascii_digit() {
            value = value
                .saturating_mul(10)
                .saturating_add(u32::from(byte - b'0'));
            saw_digit = true;
            i += 1;
        } else if byte == b';' {
            attrs.push(value);
            value = 0;
            saw_digit = false;
            i += 1;
        } else {
            break;
        }
    }
    if saw_digit {
        attrs.push(value);
    }
    (attrs, i)
}

fn scale_component(value: u32) -> u8 {
    (value.min(100).saturating_mul(255) / 100) as u8
}

/// The default DEC palette: 16 hue-ring colors, a 6x6x6 cube, then 24 grays.
fn palette_color(index: u32, background: Option<[u8; 4]>, transparent_bg: bool) -> [u8; 4] {
    match index {
        0 if transparent_bg => [0, 0, 0, 0],
        0 => background.unwrap_or([0, 0, 0, 255]),
        1..=15 => {
            let c = DEC_BASE[index as usize];
            [c[0], c[1], c[2], 255]
        }
        16..=231 => {
            let v = index - 16;
            let r = CUBE[(v / 36) as usize];
            let g = CUBE[((v / 6) % 6) as usize];
            let b = CUBE[(v % 6) as usize];
            [r, g, b, 255]
        }
        232..=255 => {
            let gray = 11 * (index - 232);
            [gray as u8, gray as u8, gray as u8, 255]
        }
        _ => [0, 0, 0, 255],
    }
}

fn draw_run(
    rgba: &mut [u8],
    stride: usize,
    width: usize,
    band: usize,
    start_x: usize,
    count: usize,
    color: [u8; 4],
) {
    for n in 0..count {
        let x = start_x.saturating_add(n);
        if x >= width {
            break;
        }
        let offset = band * BAND_PIXELS * stride + x * 4;
        if offset + 4 <= rgba.len() {
            rgba[offset..offset + 4].copy_from_slice(&color);
        }
    }
}

// DEC default palette (VT340 hue ring), matching libsixel's table exactly:
// 16 colors, then a 6x6x6 cube, then a 24-step grayscale ramp.
const DEC_BASE: [[u8; 3]; 16] = [
    [0, 0, 0],          //  0 black
    [51, 51, 204],      //  1 blue
    [204, 33, 33],      //  2 red
    [51, 204, 51],      //  3 green
    [204, 51, 204],     //  4 magenta
    [51, 204, 204],     //  5 cyan
    [204, 204, 51],     //  6 yellow
    [135, 135, 135],    //  7 gray 50%
    [66, 66, 66],       //  8 gray 25%
    [84, 84, 153],      //  9 blue (bright)
    [153, 66, 66],      // 10 red (bright)
    [84, 153, 84],      // 11 green (bright)
    [153, 84, 153],     // 12 magenta (bright)
    [84, 153, 153],     // 13 cyan (bright)
    [153, 153, 84],     // 14 yellow (bright)
    [204, 204, 204],    // 15 gray 75%
];
const CUBE: [u8; 6] = [0, 51, 102, 153, 204, 255];

#[cfg(test)]
mod tests {
    use super::*;

    fn px(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let offset = (y * width + x) * 4;
        rgba[offset..offset + 4].try_into().unwrap()
    }

    /// Transparent fill + an explicit color, so painted pixels are distinct
    /// from untouched ones.
    fn red() -> [u8; 4] {
        [255, 0, 0, 255]
    }

    #[test]
    fn data_char_paints_six_rows_of_one_column() {
        // 0x7e - 0x3f = 0x3f = all six bits set.
        let image = decode_sixel(b"\"1;1#1;100;0;0~", 6, None).unwrap();
        assert_eq!((image.width, image.height), (1, 6));
        for y in 0..6 {
            assert_eq!(px(&image.rgba, 1, 0, y), red());
        }
    }

    #[test]
    fn bit_pattern_maps_to_individual_rows() {
        // 0x42 - 0x3f = 0x03 => bits 0 and 1 set => rows 0 and 1 painted.
        let image = decode_sixel(b"\"1;1#1;100;0;0\x42", 6, None).unwrap();
        assert_eq!(px(&image.rgba, 1, 0, 0), red());
        assert_eq!(px(&image.rgba, 1, 0, 1), red());
        assert_eq!(px(&image.rgba, 1, 0, 2), [0, 0, 0, 0]);
        assert_eq!(px(&image.rgba, 1, 0, 5), [0, 0, 0, 0]);
    }

    #[test]
    fn decodes_chafa_style_realistic_frame_fixture() {
        // tests/fixtures/sixel/rainbow_palette.six is produced by
        // gen_rainbow.py the way libsixel/chafa emit frames: `#N` selects a
        // palette entry, then `!M~` paints M solid six-row columns in it.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sixel/rainbow_palette.six"
        );
        let payload = std::fs::read(path).expect("fixture present");
        let image = decode_sixel(&payload, 144, None).expect("frame decodes");
        assert_eq!((image.width, image.height), (128, 144));

        // DEC default palette (libsixel hue-ring table): 1..7 dark, 8..15
        // bright, 16.. cube, 232.. grayscale at 11-step.
        // Band 0 sweeps dark colors: col c paints palette 1 + (c % 7).
        assert_eq!(px(&image.rgba, 128, 0, 0), [51, 51, 204, 255]); // #1
        assert_eq!(px(&image.rgba, 128, 1, 0), [204, 33, 33, 255]); // #2
        assert_eq!(px(&image.rgba, 128, 6, 0), [135, 135, 135, 255]); // #7
        assert_eq!(px(&image.rgba, 128, 7, 0), [51, 51, 204, 255]); // wraps to #1
        // Band 8 sweeps bright colors: col c paints palette 8 + (c % 8).
        assert_eq!(px(&image.rgba, 128, 0, 8 * 6), [66, 66, 66, 255]); // #8
        assert_eq!(px(&image.rgba, 128, 1, 8 * 6), [84, 84, 153, 255]); // #9
        // Band 16 enters the 6x6x6 cube at #16: col 40 -> index 56.
        assert_eq!(px(&image.rgba, 128, 40, 16 * 6), [51, 0, 204, 255]);
        // Band 20 is the grayscale ramp: col 0 -> index 252 -> gray 220.
        assert_eq!(px(&image.rgba, 128, 0, 20 * 6), [220, 220, 220, 255]);
    }

    #[test]
    fn empty_bitmask_advances_but_does_not_widen() {
        // `?` (0x3f) paints nothing; two painted columns then a trailing
        // transparent column must yield width 2, matching libsixel max_x.
        let image = decode_sixel(b"#1;100;0;0~!2~\x3f", 6, None).unwrap();
        assert_eq!((image.width, image.height), (3, 6));
        assert_eq!(px(&image.rgba, 3, 0, 0), red());
        assert_eq!(px(&image.rgba, 3, 2, 0), red());
    }

    #[test]
    fn direct_color_paints_opaque_rgb() {
        let image = decode_sixel(b"#1;100;0;0~", 6, None).unwrap();
        assert_eq!(px(&image.rgba, 1, 0, 0), red());
    }

    #[test]
    fn alpha_component_is_respected() {
        // Pz = 50 => alpha 127.
        let image = decode_sixel(b"#1;100;0;0;50~", 6, None).unwrap();
        assert_eq!(px(&image.rgba, 1, 0, 0), [255, 0, 0, 127]);
    }

    #[test]
    fn repeat_count_widens_the_image() {
        let image = decode_sixel(b"#1;100;0;0!3~", 6, None).unwrap();
        assert_eq!(image.width, 3);
        assert_eq!(px(&image.rgba, 3, 0, 0), red());
        assert_eq!(px(&image.rgba, 3, 2, 0), red());
    }

    #[test]
    fn spaces_advance_the_pen() {
        let image = decode_sixel(b"\"1;1#1;100;0;0 2~", 6, None).unwrap();
        assert_eq!(image.width, 3);
        assert_eq!(px(&image.rgba, 3, 0, 0), [0, 0, 0, 0]);
        assert_eq!(px(&image.rgba, 3, 2, 0), red());
    }

    #[test]
    fn background_color_is_used_for_index_zero() {
        let image = decode_sixel(b"#0~", 6, Some([9, 8, 7, 255])).unwrap();
        assert_eq!(px(&image.rgba, 1, 0, 0), [9, 8, 7, 255]);
    }

    #[test]
    fn transparent_raster_makes_fill_transparent() {
        let image = decode_sixel(b"\"1;1#0~", 6, Some([9, 8, 7, 255])).unwrap();
        assert_eq!(px(&image.rgba, 1, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn band_separator_stacks_rows() {
        // Band 0 paints rows 0..5; after `-`, band 1 paints row 6 (bit 0).
        let image = decode_sixel(b"#1;100;0;0~-\x40", 12, None).unwrap();
        assert_eq!(image.height, 12);
        assert_eq!(px(&image.rgba, 1, 0, 0), red());
        assert_eq!(px(&image.rgba, 1, 0, 6), red());
    }

    #[test]
    fn empty_payload_is_rejected() {
        assert_eq!(decode_sixel(b"", 0, None), Err(DecodeError::Empty));
        assert_eq!(
            decode_sixel(b"#1;100;0;0", 6, None),
            Err(DecodeError::Empty)
        );
    }
}
