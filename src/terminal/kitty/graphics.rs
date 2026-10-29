//! Port of `src/terminal/kitty/graphics.zig`.
//!
//! STATUS: partial — the re-exports from `graphics_render`, `graphics_exec`
//! and the remaining `graphics_image` types (RenderPlacement, execute, Image,
//! LoadingImage) are pending those ports.
//!
//! Kitty graphics protocol support.
//!
//! Documentation:
//! https://sw.kovidgoyal.net/kitty/graphics-protocol
//!
//! Performance:
//! The performance of this particular subsystem of Ghostty is not great.
//! We can avoid a lot more allocations, we can replace some C code (which
//! implicitly allocates) with native Zig, we can improve the data structures
//! to avoid repeated lookups, etc. I tried to avoid pessimization but my
//! aim to ship a v1 of this implementation came at some cost. I learned a lot
//! though and I think we can go back through and fix this up.

use super::graphics_command as command;

pub use super::graphics_animation as animation;
pub use super::graphics_pixel as pixel;
pub use super::graphics_unicode as unicode;

pub use self::animation::Animation;
pub use self::command::Command;
pub use self::command::Parser as CommandParser;
pub use self::command::Response;

pub use super::graphics_image as image;
pub use super::graphics_storage as storage;
pub use self::image::Limits as LoadingImageLimits;
pub use self::storage::{next_generation, ImageStorage};

// TODO(port): re-export once these modules are ported:
//   use super::graphics_render as render;
//   use super::graphics_exec as exec;
//   pub use self::image::{Image, LoadingImage};
//   pub use self::render::Placement as RenderPlacement;
//   pub use self::exec::execute;
