//! Port of `src/font/library.zig`.
//!
//! STATUS: ported
//!
//! A library represents the shared state that the underlying font
//! library implementation(s) require per-process.

use std::sync::{Arc, Mutex};

use super::backend::Backend;

/// Library implementation for the compile options.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_arch = "wasm32")))]
pub type Library = FreetypeLibrary;

/// Library implementation for the compile options.
///
/// Some backends such as CT and Canvas don't have a "library".
#[cfg(any(target_os = "macos", target_os = "ios", target_arch = "wasm32"))]
pub type Library = NoopLibrary;

const _: () = {
    // Keep the `Library` alias in sync with the configured backend.
    let has_ft = crate::font::OPTIONS.backend.has_freetype();
    let is_ft = cfg!(not(any(target_os = "macos", target_os = "ios", target_arch = "wasm32")));
    assert!(has_ft == is_ft);
    let _ = Backend::Freetype;
};

/// Minimal stand-in for the FreeType binding of `pkg/freetype`.
// TODO(port-dedupe): replace with crate::pkg::freetype (Library, Error)
// once pkg/freetype is ported.
pub mod freetype {
    use std::ffi::c_int;
    use std::ffi::c_void;

    #[link(name = "freetype")]
    extern "C" {
        fn FT_Init_FreeType(alibrary: *mut *mut c_void) -> c_int;
        fn FT_Done_FreeType(library: *mut c_void) -> c_int;
    }

    /// A FreeType error code.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Error(pub i32);

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "FreeType error {}", self.0)
        }
    }

    impl std::error::Error for Error {}

    /// A handle to a FreeType library instance (`FT_Library`).
    #[derive(Debug, Clone, Copy)]
    pub struct Library {
        pub handle: *mut c_void,
    }

    // SAFETY: FT_Library handles may be moved between threads; callers
    // serialize face creation/destruction with `FreetypeLibrary::mutex`.
    unsafe impl Send for Library {}
    unsafe impl Sync for Library {}

    impl Library {
        pub fn init() -> Result<Library, Error> {
            let mut handle: *mut c_void = std::ptr::null_mut();
            // SAFETY: FT_Init_FreeType writes a valid handle on success.
            let res = unsafe { FT_Init_FreeType(&mut handle) };
            if res != 0 {
                return Err(Error(res));
            }
            Ok(Library { handle })
        }

        pub fn deinit(&self) {
            // SAFETY: the handle was created by FT_Init_FreeType and is
            // only released once by `FreetypeLibrary::deinit`.
            unsafe {
                FT_Done_FreeType(self.handle);
            }
        }
    }
}

/// Library backed by FreeType.
///
/// This is cheap to clone: clones share the same FreeType library and
/// mutex (the Zig struct is copied by value and holds pointers).
#[derive(Debug, Clone)]
pub struct FreetypeLibrary {
    pub lib: freetype::Library,

    /// Mutex to be held any time the library is
    /// being used to create or destroy a face.
    pub mutex: Arc<Mutex<()>>,
}

pub type FreetypeInitError = freetype::Error;

impl FreetypeLibrary {
    pub fn init() -> Result<FreetypeLibrary, FreetypeInitError> {
        let lib = freetype::Library::init()?;
        let mutex = Arc::new(Mutex::new(()));
        Ok(FreetypeLibrary { lib, mutex })
    }

    pub fn deinit(&mut self) {
        self.lib.deinit();
    }
}

/// Library for backends without any library state.
#[derive(Debug, Clone, Default)]
pub struct NoopLibrary {}

/// `NoopLibrary` initialization can't fail (Zig `error{}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoopInitError {}

impl NoopLibrary {
    pub fn init() -> Result<NoopLibrary, NoopInitError> {
        Ok(NoopLibrary {})
    }

    pub fn deinit(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_deinit() {
        let mut lib = FreetypeLibrary::init().unwrap();
        let clone = lib.clone();
        {
            let _guard = clone.mutex.lock().unwrap();
        }
        lib.deinit();

        let mut noop = NoopLibrary::init().unwrap();
        noop.deinit();
    }
}
