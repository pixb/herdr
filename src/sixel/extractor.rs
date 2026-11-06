//! Streaming recognition of DECSIXEL frames (`ESC P q … ESC \`) in the child
//! byte stream.
//!
//! The extractor is a side-channel scanner: it never consumes bytes (the
//! parser still receives the untouched stream) and only reports where complete
//! frames end and how tall their image is. The write path uses that to insert
//! a synthetic cursor advance after each frame, because the vendored VT core
//! ignores Sixel DCS entirely and leaves the cursor behind the image.
//!
//! Grammar handled (DEC STD 070, subset):
//! - DCS introduction `ESC P`, parameters, final byte `q` (0x71).
//! - Payload until ST (`ESC \`). `-` ends a band and moves to the next one;
//!   every band is 6 pixels tall. `!` repeat counts, `#` color selection,
//!   `"` raster attributes, spaces and `$` do not change the band count.
//! - Any other DCS final byte puts the scanner in an ignore state until ST,
//!   so payloads of unrelated DCS (tmux control mode, termcap replies, …)
//!   can never be misread as Sixel.

/// Defensive bound on one Sixel frame's payload text, not a functional limit.
/// A previewer raster sized to the preview area (a 708x252 px screenshot at
/// per-pixel color macros) reaches several mebibytes, so the bound sits under the
/// VT core's 16 MiB APC budget that the relayed image has to fit into anyway.
pub(crate) const MAX_SIXEL_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_IGNORED_DCS_BYTES: usize = 16 * 1024 * 1024;
const SIXEL_BAND_PIXELS: u32 = 6;

/// Conclusion for one complete Sixel frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SixelFrame {
    /// Offset just past ST (`ESC \`) within the input batch that finished it.
    pub tail_offset: usize,
    /// Image height in pixels (`(band count) * 6`). Zero only for empty frames,
    /// which are never reported.
    pub pixel_height: u32,
    /// The frame's data bytes (`ESC P…q` header and trailing ST excluded),
    /// accumulated across batches. This is the exact input for the decoder.
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    /// A lone `ESC` byte seen; waiting for the DCS introducer `P`.
    Escape,
    /// Inside the DCS parameter/final-byte phase.
    DcsIntro,
    /// A DCS whose final byte was not `q`; skip until ST without buffering.
    IgnoreDcs,
    /// IgnoreDcs with a pending `ESC` byte.
    IgnoreEscape,
    /// Buffering Sixel payload.
    Payload { bands: u32, has_drawing: bool },
    /// Payload with a pending `ESC` byte (candidate ST).
    PayloadEscape { bands: u32, has_drawing: bool },
}

#[derive(Debug, Default)]
pub(crate) struct SixelExtractor {
    state: State,
    /// Current frame's data bytes (`ESC P…q` and ST excluded), grown across
    /// batches until the frame completes or is discarded.
    payload: Vec<u8>,
    ignored_bytes: usize,
}

impl SixelExtractor {
    /// Scan one batch of child bytes, appending a [`SixelFrame`] for every
    /// Sixel frame completed within it. Frames may start in an earlier batch;
    /// state carries over between calls.
    pub(crate) fn feed(&mut self, bytes: &[u8], out: &mut Vec<SixelFrame>) {
        for (index, &byte) in bytes.iter().enumerate() {
            match self.state {
                State::Idle => {
                    if byte == 0x1b {
                        self.state = State::Escape;
                    }
                }
                State::Escape => match byte {
                    // Consecutive ESC bytes stay in the same pending state.
                    0x1b => {}
                    0x50 => self.state = State::DcsIntro, // 'P'
                    _ => self.state = State::Idle,
                },
                State::DcsIntro => match byte {
                    b'0'..=b'9' | b';' | b':' => {}
                    b'q' => {
                        self.state = State::Payload {
                            bands: 0,
                            has_drawing: false,
                        };
                        self.payload.clear();
                    }
                    _ => {
                        self.state = State::IgnoreDcs;
                        self.ignored_bytes = 1;
                        if self.ignored_bytes > MAX_IGNORED_DCS_BYTES {
                            self.state = State::Idle;
                        }
                    }
                },
                State::IgnoreDcs => match byte {
                    0x1b => self.state = State::IgnoreEscape,
                    _ => {
                        self.ignored_bytes = self.ignored_bytes.saturating_add(1);
                        if self.ignored_bytes > MAX_IGNORED_DCS_BYTES {
                            self.state = State::Idle;
                        }
                    }
                },
                State::IgnoreEscape => match byte {
                    0x5c => {
                        // ST terminates the ignored DCS.
                        self.state = State::Idle;
                    }
                    _ => {
                        self.state = State::IgnoreDcs;
                        self.ignored_bytes = self.ignored_bytes.saturating_add(1);
                        if self.ignored_bytes > MAX_IGNORED_DCS_BYTES {
                            self.state = State::Idle;
                        }
                    }
                },
                State::Payload { bands, has_drawing } => match byte {
                    0x1b => {
                        self.state = State::PayloadEscape { bands, has_drawing };
                    }
                    b'-' => {
                        self.state = State::Payload {
                            bands: bands.saturating_add(1),
                            has_drawing: true,
                        };
                        self.bump_payload(byte);
                    }
                    0x3f..=0x7e => {
                        self.state = State::Payload {
                            bands,
                            has_drawing: true,
                        };
                        self.bump_payload(byte);
                    }
                    _ => self.bump_payload(byte),
                },
                State::PayloadEscape { bands, has_drawing } => match byte {
                    0x5c => {
                        if has_drawing {
                            let pixel_height = bands.saturating_add(1) * SIXEL_BAND_PIXELS;
                            out.push(SixelFrame {
                                tail_offset: index + 1,
                                pixel_height,
                                payload: std::mem::take(&mut self.payload),
                            });
                        } else {
                            self.payload.clear();
                        }
                        self.state = State::Idle;
                    }
                    // A non-ST ESC inside the payload aborts the frame. The
                    // byte itself is re-interpreted as plain input from here.
                    _ => {
                        self.state = State::Idle;
                        self.payload.clear();
                        self.feed(&[byte], out);
                    }
                },
            }
        }
    }

    fn bump_payload(&mut self, byte: u8) {
        if self.payload.len() >= MAX_SIXEL_PAYLOAD_BYTES {
            // Oversized or unterminated payload: discard the whole frame.
            self.payload.clear();
            self.state = State::Idle;
            return;
        }
        self.payload.push(byte);
    }
}

/// Convert a pixel height to text rows (rounded up). `cell_height_px` of zero
/// falls back to one row per pixel.
pub(crate) fn pixel_height_to_rows(pixel_height: u32, cell_height_px: u32) -> u32 {
    pixel_height.div_ceil(cell_height_px.max(1)).max(1)
}

/// Synthetic cursor sequence placing the parser cursor at the start of the
/// line below the image, matching the DECSDM reset post-condition that
/// `chafa`/`img2sixel` rely on.
pub(crate) fn synthetic_cursor_advance(rows: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(rows.to_string().as_bytes());
    out.extend_from_slice(b"B\x1b[1G");
    out
}

/// Whether the child appears to position the cursor itself right after the
/// frame. When it does, Herdr skips its own synthetic advance to avoid moving
/// twice.
pub(crate) fn needs_synthetic_cursor(peek: &[u8]) -> bool {
    let Some(first) = peek.first() else {
        return true;
    };
    if matches!(*first, b'\r' | b'\n') {
        return false;
    }
    if peek.starts_with(b"\x1b[?") {
        // DEC private control; not a plain cursor positioning.
        return true;
    }
    if peek.starts_with(b"\x1b[") {
        let rest = &peek[2..];
        let mut index = 0;
        while index < rest.len() && (rest[index].is_ascii_digit() || rest[index] == b';') {
            index += 1;
        }
        if index < rest.len()
            && matches!(
                rest[index],
                b'H' | b'f' | b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'G'
            )
        {
            return false;
        }
        return true;
    }
    true
}

/// Screen-erase sequences that must also clear Sixel images on the host.
/// Parsed independently of the VT core so the Sixel layer can stay in sync
/// with child-initiated clears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClearKind {
    /// ED `CSI 2 J`: clear the whole screen.
    FullScreen,
    /// ED `CSI 3 J`: clear the scrollback.
    Scrollback,
    /// DECSDM `CSI ? 80 h/l`: Sixel display/erase mode toggle.
    DecsdmErase,
}

pub(crate) fn classify_clear(bytes: &[u8]) -> Option<ClearKind> {
    let (body, private) = if let Some(body) = bytes.strip_prefix(b"\x1b[?") {
        (body, true)
    } else if let Some(body) = bytes.strip_prefix(b"\x1b[") {
        (body, false)
    } else {
        return None;
    };

    let mut last_number: Option<u32> = None;
    let mut current: u32 = 0;
    let mut saw_digit = false;
    for &byte in body {
        if byte.is_ascii_digit() {
            current = current
                .saturating_mul(10)
                .saturating_add(u32::from(byte - b'0'));
            saw_digit = true;
        } else if byte == b';' {
            last_number = Some(current);
            current = 0;
            saw_digit = false;
        } else {
            if saw_digit {
                last_number = Some(current);
            }
            if private {
                if last_number == Some(80) && matches!(byte, b'h' | b'l') {
                    return Some(ClearKind::DecsdmErase);
                }
            } else if byte == b'J' {
                return match last_number.unwrap_or(0) {
                    2 => Some(ClearKind::FullScreen),
                    3 => Some(ClearKind::Scrollback),
                    _ => None,
                };
            }
            return None;
        }
    }
    None
}

/// First screen-erase sequence anywhere in a batch, if any. The write path
/// deletes this pane's relayed Sixel images when the child clears the screen
/// or toggles DECSDM, because the VT core keeps Kitty placements on ED.
pub(crate) fn scan_clear(bytes: &[u8]) -> Option<ClearKind> {
    let mut offset = 0;
    while let Some(relative) = bytes[offset..].iter().position(|&b| b == 0x1b) {
        let start = offset + relative;
        if let Some(kind) = classify_clear(&bytes[start..]) {
            return Some(kind);
        }
        offset = start + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(bytes: &[u8]) -> Vec<SixelFrame> {
        let mut extractor = SixelExtractor::default();
        let mut out = Vec::new();
        extractor.feed(bytes, &mut out);
        out
    }

    fn frames_across(chunks: &[&[u8]]) -> Vec<SixelFrame> {
        let mut extractor = SixelExtractor::default();
        let mut out = Vec::new();
        for chunk in chunks {
            extractor.feed(chunk, &mut out);
        }
        out
    }

    #[test]
    fn extracts_complete_frame_with_band_height() {
        // Three '-' separators => four bands => 24px.
        let out = frames(b"\x1bPq#1;2;100;100~~~---~\x1b\\");
        assert_eq!(
            out,
            [SixelFrame {
                tail_offset: 24,
                pixel_height: 24,
                payload: b"#1;2;100;100~~~---~".to_vec(),
            }]
        );
    }

    #[test]
    fn frame_params_before_final_q_are_ignored() {
        let out = frames(b"\x1bP0;1q~-~\x1b\\");
        assert_eq!(
            out,
            [SixelFrame {
                tail_offset: 11,
                pixel_height: 12,
                payload: b"~-~".to_vec(),
            }]
        );
    }

    #[test]
    fn non_sixel_dcs_is_ignored_until_st() {
        // tmux control mode payload contains ESC P…; it must not start a frame.
        let out = frames(b"before\x1bP1000pA\x1bPq~\x1b\\after");
        assert!(out.is_empty());
    }

    #[test]
    fn real_sixel_after_ignored_dcs_is_detected() {
        let out = frames(b"before\x1bP1000pA\x1b\\\x1bPq~\x1b\\after");
        assert_eq!(
            out,
            [SixelFrame {
                tail_offset: 22,
                pixel_height: 6,
                payload: b"~".to_vec(),
            }]
        );
    }

    #[test]
    fn frame_completed_across_chunks_reports_relative_tail() {
        let first = b"\x1bPq#1;2;100;100~~~-";
        let out = frames_across(&[first, b"~~\x1b\\tail"]);
        assert_eq!(
            out,
            [SixelFrame {
                tail_offset: 4,
                pixel_height: 12,
                payload: b"#1;2;100;100~~~-~~".to_vec(),
            }]
        );
    }

    #[test]
    fn repeat_counts_do_not_change_band_height() {
        let out = frames(b"\x1bPq!20~-$-~\x1b\\");
        assert_eq!(
            out,
            [SixelFrame {
                tail_offset: 13,
                pixel_height: 18,
                payload: b"!20~-$-~".to_vec(),
            }]
        );
    }

    #[test]
    fn empty_payload_is_not_a_frame() {
        assert!(frames(b"\x1bPq\x1b\\").is_empty());
        assert!(frames(b"\x1bPq#1;2;100;100\x1b\\").is_empty());
    }

    #[test]
    fn escape_inside_payload_aborts_and_reinterprets() {
        // ESC followed by non-ST inside a payload aborts the frame.
        let out = frames(b"\x1bPq~~\x1b[2J~\x1b\\");
        assert!(out.is_empty());
    }

    #[test]
    fn oversized_payload_is_discarded_and_recovers() {
        let mut huge = Vec::new();
        huge.extend_from_slice(b"\x1bPq");
        huge.extend(std::iter::repeat_n(b'~', MAX_SIXEL_PAYLOAD_BYTES + 16));
        huge.extend_from_slice(b"\x1b\\");
        let mut extractor = SixelExtractor::default();
        let mut out = Vec::new();
        extractor.feed(&huge, &mut out);
        assert!(out.is_empty());
        // The scanner recovered and can see a fresh frame afterwards.
        extractor.feed(b"\x1bPq~-~\x1b\\", &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn repeated_frames_in_one_batch() {
        let out = frames(b"\x1bPq~\x1b\\x\x1bPq~-~\x1b\\");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].tail_offset, 6);
        assert_eq!(out[0].pixel_height, 6);
        assert_eq!(out[0].payload, b"~");
        assert_eq!(out[1].tail_offset, 15);
        assert_eq!(out[1].pixel_height, 12);
        assert_eq!(out[1].payload, b"~-~");
    }

    #[test]
    fn synthetic_advance_matches_decsdm_post_condition() {
        assert_eq!(synthetic_cursor_advance(3), b"\x1b[3B\x1b[1G");
        assert_eq!(pixel_height_to_rows(18, 10), 2);
        assert_eq!(pixel_height_to_rows(6, 16), 1);
        assert_eq!(pixel_height_to_rows(0, 0), 1);
    }

    #[test]
    fn synthetic_cursor_is_skipped_when_child_positions_itself() {
        assert!(!needs_synthetic_cursor(b"\r"));
        assert!(!needs_synthetic_cursor(b"\r\n"));
        assert!(!needs_synthetic_cursor(b"\n"));
        assert!(!needs_synthetic_cursor(b"\x1b[1;2H"));
        assert!(!needs_synthetic_cursor(b"\x1b[3B"));
        assert!(!needs_synthetic_cursor(b"\x1b[1G"));
        assert!(needs_synthetic_cursor(b""));
        assert!(needs_synthetic_cursor(b"text"));
        assert!(needs_synthetic_cursor(b"\x1b[2J"));
        assert!(needs_synthetic_cursor(b"\x1b[?80l"));
        assert!(needs_synthetic_cursor(b"\x1b]0;title\x07"));
    }

    #[test]
    fn clear_classification_covers_ed_and_decsdm() {
        assert_eq!(classify_clear(b"\x1b[2J"), Some(ClearKind::FullScreen));
        assert_eq!(classify_clear(b"\x1b[3J"), Some(ClearKind::Scrollback));
        assert_eq!(classify_clear(b"\x1b[?80h"), Some(ClearKind::DecsdmErase));
        assert_eq!(classify_clear(b"\x1b[?80l"), Some(ClearKind::DecsdmErase));
        assert_eq!(classify_clear(b"\x1b[1J"), None);
        assert_eq!(classify_clear(b"\x1b[J"), None);
        assert_eq!(classify_clear(b"plain text"), None);
        assert_eq!(classify_clear(b"\x1b[2;1J"), None);
    }

    #[test]
    fn scan_clear_finds_erase_sequences_inside_a_batch() {
        assert_eq!(scan_clear(b"text\x1b[2Jmore"), Some(ClearKind::FullScreen));
        assert_eq!(
            scan_clear(b"a\x1b]0;title\x07\x1b[?80l"),
            Some(ClearKind::DecsdmErase)
        );
        assert_eq!(scan_clear(b"\x1b[1J"), None);
        assert_eq!(scan_clear(b"no clear here"), None);
        assert_eq!(scan_clear(b""), None);
    }
}
