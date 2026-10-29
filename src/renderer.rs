//! Port of `src/renderer.zig`.
//!
//! STATUS: partial — `Renderer` (generic.zig over OpenGL), `Device`,
//! `Thread`, `Overlay` and the font grid types are stand-ins in
//! [`standin`] until those files are ported (TODO(port-dedupe)).
//!
//! Renderer implementation and utilities. The renderer is responsible for
//! taking the internal screen state and turning into some output format,
//! usually for a screen.
//!
//! The renderer is closely tied to the windowing system which usually
//! has to prepare the window for the given renderer using system-specific
//! APIs. The renderers in this package assume that the renderer is already
//! setup (OpenGL has a context, Vulkan has a surface, etc.)

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "renderer/backend.rs"]
pub mod backend;
#[path = "renderer/cell.rs"]
pub mod cell;
#[path = "renderer/cursor.rs"]
pub mod cursor;
#[path = "renderer/display_link.rs"]
pub mod display_link;
#[path = "renderer/dmabuf.rs"]
pub mod dmabuf;
#[path = "renderer/generic.rs"]
pub mod generic;
#[path = "renderer/image.rs"]
pub mod image;
#[path = "renderer/link.rs"]
pub mod link;
#[path = "renderer/message.rs"]
pub mod message;
#[path = "renderer/metal.rs"]
pub mod metal;
#[path = "renderer/opengl.rs"]
pub mod opengl;
#[path = "renderer/options.rs"]
pub mod options;
#[path = "renderer/overlay.rs"]
pub mod overlay;
#[path = "renderer/row.rs"]
pub mod row;
#[path = "renderer/shadertoy.rs"]
pub mod shadertoy;
#[path = "renderer/size.rs"]
pub mod size;
#[path = "renderer/state.rs"]
pub mod state;
#[path = "renderer/thread.rs"]
pub mod thread;
// @generated-mods-end

pub use self::backend::Backend;
pub use self::cursor::style as cursor_style;
pub use self::cursor::Style as CursorStyle;
pub use self::message::Message;
pub use self::options::Options;
pub use self::size::{CellSize, Coordinate, GridSize, Padding, ScreenSize, Size};
pub use self::state::State;
pub use crate::lib;

/// The implementation to use for the renderer. This is comptime chosen
/// so that every build has exactly one renderer implementation.
///
/// TODO(port-dedupe): replace with the generic renderer instantiated with
/// the OpenGL graphics API (`GenericRenderer(GraphicsAPI)`).
pub use self::standin::Renderer;

/// The app-scoped render device from which surface renderers are created.
///
/// TODO(port-dedupe): replace with `GraphicsAPI::Device`.
pub use self::standin::Device;

/// TODO(port-dedupe): replace with `thread::Thread` (renderer/Thread.zig).
pub use self::standin::Thread;

crate::lib_enum! {
    /// The health status of a renderer. These must be shared across all
    /// renderers even if some states aren't reachable so that our API users
    /// can use the same enum for all renderers.
    pub enum Health: c {
        Healthy,
        Unhealthy,
    }
}

/// Stand-ins for types whose Zig files are not ported yet.
pub mod standin {
    use std::fmt;

    use super::Message;
    use crate::config::Config;
    use crate::datastruct::BlockingQueue;

    /// Error from initializing a derived config.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum InitError {
        OutOfMemory,
    }

    impl fmt::Display for InitError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("OutOfMemory")
        }
    }

    impl std::error::Error for InitError {}

    /// TODO(port-dedupe): replace with the generic renderer
    /// (`renderer/generic.zig` instantiated with OpenGL).
    #[derive(Debug, Default)]
    pub struct Renderer {}

    /// TODO(port-dedupe): replace with `Renderer::DerivedConfig`
    /// (`renderer/generic.zig`).
    #[derive(Debug, Clone, Default)]
    pub struct RendererDerivedConfig {}

    impl RendererDerivedConfig {
        pub fn init(_config: &Config) -> Result<RendererDerivedConfig, InitError> {
            Ok(RendererDerivedConfig {})
        }

        pub fn deinit(&mut self) {}
    }

    /// TODO(port-dedupe): replace with `GraphicsAPI::Device`.
    #[derive(Debug, Default)]
    pub struct Device {}

    /// TODO(port-dedupe): replace with `crate::renderer::thread::Thread`.
    #[derive(Debug, Default)]
    pub struct Thread {}

    /// The type used for sending messages to the renderer thread.
    ///
    /// TODO(port-dedupe): replace with `crate::renderer::thread::Mailbox`.
    pub type ThreadMailbox = BlockingQueue<Message, 64>;

    /// TODO(port-dedupe): replace with `crate::renderer::thread::DerivedConfig`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct ThreadDerivedConfig {
        pub scrollback_compression: bool,
    }

    impl ThreadDerivedConfig {
        pub fn init(config: &Config) -> ThreadDerivedConfig {
            ThreadDerivedConfig {
                scrollback_compression: config.scrollback_compression,
            }
        }
    }

    /// TODO(port-dedupe): replace with `crate::font::SharedGrid`.
    #[derive(Debug, Default)]
    pub struct SharedGrid {}

    /// TODO(port-dedupe): replace with `crate::font::SharedGridSet`.
    #[derive(Debug, Default)]
    pub struct SharedGridSet {}

    /// TODO(port-dedupe): replace with `crate::font::shared_grid_set::Key`.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct SharedGridSetKey {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghostty_h_health() {
        crate::lib::check_ghostty_h_enum::<Health>(
            "GHOSTTY_RENDERER_HEALTH_",
            include_str!("../include/ghostty.h"),
        )
        .unwrap();
    }
}
