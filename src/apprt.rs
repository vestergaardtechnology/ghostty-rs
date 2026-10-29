//! Port of `src/apprt.zig`.
//!
//! STATUS: partial — `App`/`Surface` are stand-ins until the GTK apprt
//! (`apprt/gtk.zig`) is ported (TODO(port-dedupe)).
//!
//! "apprt" is the "application runtime" package. This abstracts the
//! application runtime and lifecycle management such as creating windows,
//! getting user input (mouse/keyboard), etc.
//!
//! This enables compile-time interfaces to be built to swap out the underlying
//! application runtime. For example: pure macOS Cocoa, GTK+, browser, etc.
//!
//! The goal is to have different implementations share as much of the core
//! logic as possible, and to only reach out to platform-specific implementation
//! code when absolutely necessary.
//!
//! Rust notes: Zig's `apprt.runtime` (the comptime-selected implementation
//! module) is [`rt`] because `runtime` is the module port of
//! `apprt/runtime.zig` (whose `Runtime` enum is re-exported here).

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "apprt/action.rs"]
pub mod action;
#[path = "apprt/browser.rs"]
pub mod browser;
#[path = "apprt/embedded.rs"]
pub mod embedded;
#[path = "apprt/gtk.rs"]
pub mod gtk;
#[path = "apprt/ipc.rs"]
pub mod ipc;
#[path = "apprt/none.rs"]
pub mod none;
#[path = "apprt/runtime.rs"]
pub mod runtime;
#[path = "apprt/structs.rs"]
pub mod structs;
#[path = "apprt/surface.rs"]
pub mod surface;
// @generated-mods-end

pub use self::action::{Action, Target};
pub use self::runtime::Runtime;

pub use self::structs::{
    Clipboard, ClipboardContent, ClipboardReadResult, ClipboardRequest, ClipboardRequestType,
    ColorScheme, ContentScale, CursorPos, IMEPos, Selection, SurfaceSize,
};

/// The implementation to use for the app runtime. This is comptime chosen
/// so that every build has exactly one application runtime implementation.
/// Note: it is very rare to use Runtime directly; most usage will use
/// Window or something.
///
/// Zig: `apprt.runtime`. The executable on Linux/FreeBSD uses GTK; the
/// library (macOS) uses the embedded runtime.
#[cfg(not(target_os = "macos"))]
pub use self::gtk as rt;
#[cfg(target_os = "macos")]
pub use self::embedded as rt;

// TODO(port-dedupe): replace with `rt::App` / `rt::Surface` once the GTK
// (and embedded) apprts are ported.
pub use self::standin::{App, Surface};

/// Stand-ins for the runtime's `App` and `Surface` types.
pub mod standin {
    /// TODO(port-dedupe): replace with `crate::apprt::rt::App`.
    #[derive(Debug, Default)]
    pub struct App {}

    impl App {
        /// Wake up the app event loop so it processes its mailbox.
        pub fn wakeup(&self) {}
    }

    /// TODO(port-dedupe): replace with `crate::apprt::rt::Surface`.
    #[derive(Debug, Default)]
    pub struct Surface {}
}
