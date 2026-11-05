//! Bridge from decoded Sixel frames into the existing Kitty graphics
//! forwarding pipeline.
//!
//! The vendored VT parser ignores Sixel DCS sequences entirely, so Herdr
//! decodes each frame into RGBA and relays it to the *same* terminal core as
//! a synthetic Kitty image: a transmit (`a=t`) followed by a placement
//! (`a=p`) at the current cursor, which is exactly where the frame started.
//! The parser then owns image storage, scrolling, placement lifecycle and
//! visibility exactly like any child-issued Kitty image, so
//! `collect_visible_placements` / `encode_graphics_output` need no changes.

use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::sixel::decode::{decode_sixel, DecodedImage};

/// Synthetic image ID space, far above the small IDs children typically use,
/// so a child Kitty image can never collide with a relayed Sixel image.
const SIXEL_IMAGE_ID_BASE: u32 = 1_000_000;
static NEXT_SIXEL_IMAGE_ID: AtomicU32 = AtomicU32::new(SIXEL_IMAGE_ID_BASE);

fn next_sixel_image_id() -> u32 {
    NEXT_SIXEL_IMAGE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Encode a decoded Sixel image as a pair of Kitty graphics commands placed
/// at the current cursor position. `cols`/`rows` are the grid-cell size of
/// the placement.
fn encode_kitty_commands(image: &DecodedImage, image_id: u32, cols: u32, rows: u32) -> Vec<u8> {
    let mut command = Vec::new();
    let control = format!(
        "a=t,t=d,f=32,s={},v={},i={image_id},q=2",
        image.width, image.height
    );
    crate::kitty_graphics::write_kitty_data(&mut command, &control, &image.rgba)
        .expect("writing to Vec cannot fail");
    let _ = write!(
        command,
        "\x1b_Ga=p,i={image_id},p={image_id},c={cols},r={rows},z=0,C=1,q=2;\x1b\\"
    );
    command
}

/// Decode one complete Sixel frame and relay it into the terminal core as a
/// Kitty image. Returns the synthetic image ID on success (so the pane can
/// track it for clear-sync deletion), or `None` when the frame was dropped.
///
/// The placement lands at the current cursor (the frame's start position);
/// the caller injects the synthetic cursor advance afterwards so trailing
/// text starts below the image.
pub(crate) fn relay_sixel_frame(
    terminal: &mut crate::ghostty::Terminal,
    payload: &[u8],
    pixel_height: u32,
    cell_width_px: u32,
    cell_height_px: u32,
    background: Option<[u8; 4]>,
) -> Option<u32> {
    let Ok(image) = decode_sixel(payload, pixel_height, background) else {
        tracing::debug!(
            pixel_height,
            payload_len = payload.len(),
            "sixel: frame failed to decode"
        );
        return None;
    };
    let cols = grid_cells(image.width, cell_width_px);
    let rows = grid_cells(image.height, cell_height_px);
    if cols == 0 || rows == 0 {
        tracing::debug!(
            image_width = image.width,
            image_height = image.height,
            "sixel: zero grid"
        );
        return None;
    }
    let image_id = next_sixel_image_id();
    let command = encode_kitty_commands(&image, image_id, cols, rows);
    if command.len() > crate::protocol::MAX_GRAPHICS_FRAME_SIZE {
        tracing::warn!(
            bytes = command.len(),
            budget = crate::protocol::MAX_GRAPHICS_FRAME_SIZE,
            "sixel: relayed image exceeds the graphics frame budget; skipping"
        );
        return None;
    }
    terminal.write(&command);
    tracing::debug!(
        image_id,
        cols,
        rows,
        image_width = image.width,
        image_height = image.height,
        "sixel: relayed frame as Kitty placement"
    );
    Some(image_id)
}

/// Number of host cells that span `pixels` at the given cell size.
fn grid_cells(pixels: u32, cell_pixels: u32) -> u32 {
    let cell = cell_pixels.max(1) as u64;
    (pixels as u64).div_ceil(cell).min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_cells_round_up_and_never_zero() {
        assert_eq!(grid_cells(0, 16), 0);
        assert_eq!(grid_cells(1, 16), 1);
        assert_eq!(grid_cells(16, 16), 1);
        assert_eq!(grid_cells(17, 16), 2);
        assert_eq!(grid_cells(100, 0), 100);
    }

    #[test]
    fn commands_transmit_rgba_then_place_at_cursor() {
        let image = DecodedImage {
            width: 3,
            height: 6,
            rgba: [255, 0, 0, 255].repeat(3 * 6),
        };
        let command = encode_kitty_commands(&image, 1_000_000, 1, 1);
        let text = String::from_utf8(command).unwrap();
        assert!(text.contains("a=t,t=d,f=32,s=3,v=6,i=1000000,q=2"));
        assert!(text.contains("a=p,i=1000000,p=1000000,c=1,r=1,z=0,C=1,q=2"));
        // RGBA 3x6 = 72 bytes => 96 base64 chars, all in the first chunk.
        assert!(text.ends_with(";\x1b\\"));
    }

    #[test]
    fn relay_places_image_into_the_terminal_core() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 100).unwrap();
        terminal.enable_kitty_graphics().unwrap();
        let placed = relay_sixel_frame(&mut terminal, b"#1;100;0;0!20~-\x40", 12, 10, 16, None);
        // Parallel tests consume synthetic IDs from the shared counter, so
        // only the id space base is stable.
        assert!(
            placed.is_some_and(|id| id >= SIXEL_IMAGE_ID_BASE),
            "frame should relay"
        );
        let placements = terminal.kitty_image_placements().unwrap();
        assert_eq!(placements.len(), 1, "parser should own one placement");
        assert_eq!(placements[0].image_width, 20);
        assert_eq!(placements[0].image_height, 12);
    }
}
