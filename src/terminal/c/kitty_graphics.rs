//! Port of `src/terminal/c/kitty_graphics.zig`.
//!
//! STATUS: partial — only the `GhosttyKittyGraphics` handle type is
//! ported, as needed by `c/terminal` (`GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS`).
//! TODO(c-api batch 3): the image/placement handles, enums, iterators,
//! render info, the `kitty_graphics_*` functions and the tests.

/// C: GhosttyKittyGraphics
///
/// (Zig: `*anyopaque` in builds without Kitty graphics; this port always
/// builds Kitty graphics.)
pub type KittyGraphics = *mut crate::terminal::kitty::graphics_storage::ImageStorage;
