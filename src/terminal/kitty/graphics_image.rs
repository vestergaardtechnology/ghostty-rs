//! Port of `src/terminal/kitty/graphics_image.zig`.
//!
//! STATUS: partial — only `LoadingImage.Limits` (as [`Limits`]) is ported,
//! because `Screen` needs it for its options. `LoadingImage`, `Image`,
//! `Rect`, decoding, and all tests are pending.

/// The limits of the Kitty Graphics protocol we should allow
/// (Zig: `LoadingImage.Limits`).
///
/// This can be used to restrict the type of images and other
/// parameters for resource or security reasons. Note that depending
/// on how libghostty is compiled, some of these may be fully unsupported
/// and ignored (e.g. "file" on wasm32-freestanding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub file: bool,
    pub temporary_file: TemporaryFile,
    pub shared_memory: bool,
}

/// (Zig: the anonymous `Limits.temporary_file` union.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemporaryFile {
    Enabled {
        /// The directory to expect temporary files in.
        directory: Vec<u8>,
    },
    Disabled,
}

impl Limits {
    /// Enables all filesystem-related image transmission mediums. `path`
    /// is the temporary directory to expect files in when files are
    /// transmitted using said medium.
    pub fn all_with_temp_dir(path: &[u8]) -> Limits {
        Limits {
            file: true,
            temporary_file: TemporaryFile::Enabled {
                directory: path.to_vec(),
            },
            shared_memory: true,
        }
    }

    pub const DIRECT: Limits = Limits {
        file: false,
        temporary_file: TemporaryFile::Disabled,
        shared_memory: false,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Limits::DIRECT
    }
}
