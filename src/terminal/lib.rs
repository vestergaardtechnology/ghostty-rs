//! Port of `src/terminal/lib.zig`.
//!
//! STATUS: partial — the forwarded re-exports of `crate::lib` items (alloc,
//! TinyIo, Buffer, Enum, parseInt, TaggedUnion, Struct, String,
//! checkGhosttyHEnum, structSizedFieldFits) are pending the `src/lib` port.
//!
//! Helpers for the terminal package's C ABI (`libghostty-vt`).
//!
//! In Zig, `lib.Enum(lib.target, &.{...})` creates an enum whose backing
//! type is `c_int` when building the C ABI and the smallest unsigned integer
//! otherwise. The Rust port is always the Ghostty application build
//! (`build_options::C_ABI == false`), so such enums are written as plain Rust
//! enums with explicit `#[repr(u8)]` discriminants equal to the key index.

use super::build_options;

/// The ABI flavor of a library build (`lib.Target`).
///
/// TODO(port-dedupe): replace with crate::lib::target::Target
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// C ABI compatible types (`c_int` enums, extern structs).
    C,
    /// Native types with the smallest possible representation.
    Zig,
}

/// The target for the terminal lib in particular.
pub const TARGET: Target = if build_options::C_ABI {
    Target::C
} else {
    Target::Zig
};

/// A calling convention (`std.lang.CallingConvention`), restricted to what
/// the terminal package uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallingConvention {
    /// The C calling convention (`extern "C"` in Rust).
    C,
}

/// The calling convention to use for C APIs.
///
/// This is always .c for now. I want to make this "Zig" when we're not
/// building the C ABI but there are bigger issues we need to resolve to
/// make that possible (change it and see for yourself).
///
/// In Rust, functions exported with this convention are declared
/// `extern "C" fn`.
pub const CALLING_CONV: CallingConvention = CallingConvention::C;

// Forwarded decls from lib that are used.
//
// TODO(port-dedupe): once `src/lib` is ported, re-export:
//   pub use crate::lib::allocator as alloc;
//   pub use crate::lib::tinyio::TinyIo;
//   pub use crate::lib::types::{Buffer, String};
//   pub use crate::lib::r#enum::{Enum, check_ghostty_h_enum};
//   pub use crate::lib::parse_int::parse_int;
//   pub use crate::lib::union::TaggedUnion;
//   pub use crate::lib::r#struct::{Struct, sized_field_fits as struct_sized_field_fits};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_is_zig_for_app_build() {
        assert_eq!(TARGET, Target::Zig);
        assert_eq!(CALLING_CONV, CallingConvention::C);
    }
}
