//! Sixel graphics support.
//!
//! Children in a pane may emit DECSIXEL raster graphics (`ESC P q … ESC \`).
//! Herdr's vendored VT core does not parse Sixel (upstream Ghostty
//! intentionally declined it), so Herdr owns the handling:
//!
//! - `extractor` recognises complete Sixel frames in the child byte stream
//!   without consuming bytes, derives each image's pixel height from its band
//!   count, captures the frame payload, and lets the write path insert a
//!   synthetic cursor advance so text following an image starts below it.
//! - `decode` is a small zero-dependency SIXEL decoder producing RGBA pixels.
//! - `forward` relays a decoded frame into the terminal core as a synthetic
//!   Kitty image (transmit + placement), so the existing Kitty forwarding
//!   pipeline owns scrolling, lifecycle and clipping unchanged.
//!
//! The global mode below gates whether any Sixel work is active. `auto`
//! currently behaves like `on` (relay); a pure pass-through fast path for
//! hosts that support Sixel natively is deferred (see LOG in the RFC).

use std::sync::atomic::{AtomicU8, Ordering};

pub(crate) mod decode;
pub(crate) mod extractor;
pub(crate) mod forward;

pub(crate) use extractor::SixelExtractor;

const MODE_AUTO: u8 = 0;
const MODE_ON: u8 = 1;
const MODE_OFF: u8 = 2;

/// Configured Sixel mode: `Some(true)` forces on, `Some(false)` forces off,
/// `None` defers to the outer terminal's reported capability.
static SIXEL_MODE: AtomicU8 = AtomicU8::new(MODE_AUTO);

pub(crate) fn set_mode(mode: Option<bool>) {
    SIXEL_MODE.store(
        match mode {
            None => MODE_AUTO,
            Some(true) => MODE_ON,
            Some(false) => MODE_OFF,
        },
        Ordering::Release,
    );
}

pub(crate) fn mode() -> Option<bool> {
    match SIXEL_MODE.load(Ordering::Acquire) {
        MODE_ON => Some(true),
        MODE_OFF => Some(false),
        _ => None,
    }
}
