//! Port of `src/os/wasm/target.zig`.
//!
//! STATUS: ported (wasm32-only; not compiled on Linux)

/// The wasm target platform. This is used to toggle certain features
/// on and off since the standard triple target is often not specific
/// enough (i.e. we can't tell wasm32-freestanding is for browser or not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Browser,
}

/// Our specific target platform.
///
/// TODO(port-dedupe): replace with the build option once build_config is
/// ported (Zig: `options.wasm_target`, default `.browser`).
pub const TARGET: Option<Target> = if cfg!(target_arch = "wasm32") {
    Some(Target::Browser)
} else {
    None
};
