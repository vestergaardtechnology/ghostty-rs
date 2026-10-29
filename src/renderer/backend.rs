//! Port of `src/renderer/backend.zig`.
//!
//! STATUS: ported

use std::fmt;

/// Possible implementations, used for build options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    Opengl,
    Metal,
}

impl Backend {
    /// The default backend for the current compilation target (Zig:
    /// `default(target, wasm_target)`; the Zig function ignores the wasm
    /// target, so the Rust port has no parameter for it).
    pub const fn default() -> Backend {
        if cfg!(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "visionos"
        )) {
            return Backend::Metal;
        }
        Backend::Opengl
    }

    /// The Zig tag name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Backend::Opengl => "opengl",
            Backend::Metal => "metal",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
