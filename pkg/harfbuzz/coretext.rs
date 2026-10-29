//! Port of `pkg/harfbuzz/coretext.zig`.
//!
//! STATUS: ported (macOS only; not compiled or tested on Linux)

#![cfg(target_os = "macos")]

use std::os::raw::c_void;

use super::c;
use super::errors::Error;
use super::font::Font;

// TODO(port-dedupe): replace with crate::pkg::macos::text::Font
/// A `CTFontRef` (Zig: `*macos.text.Font`).
#[repr(C)]
pub struct CTFont {
    _data: [u8; 0],
    _marker: std::marker::PhantomData<(*mut u8, std::marker::PhantomPinned)>,
}

// Use custom extern functions so that the proper CoreText structs are used
// without a ptrcast.
#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    fn hb_coretext_font_create(ct_face: *mut CTFont) -> *mut c::hb_font_t;
}

/// Creates an hb_font_t font object from the specified CTFontRef.
///
/// # Safety
/// `face` must be a valid `CTFontRef`.
pub unsafe fn create_font(face: *mut CTFont) -> Result<Font, Error> {
    let handle = hb_coretext_font_create(face);
    if handle.is_null() {
        return Err(Error::HarfbuzzFailed);
    }
    Ok(Font { handle })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal CoreFoundation/CoreText FFI for the test (the Zig test uses
    // the `macos` package).
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            alloc: *const c_void,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external: u8,
        ) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "CoreText", kind = "framework")]
    unsafe extern "C" {
        fn CTFontDescriptorCreateWithNameAndSize(name: *const c_void, size: f64) -> *const c_void;
        fn CTFontCreateWithFontDescriptor(
            desc: *const c_void,
            size: f64,
            matrix: *const c_void,
        ) -> *mut CTFont;
    }

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x08000100;

    #[test]
    fn coretext_font() {
        // SAFETY: CoreFoundation/CoreText calls with valid arguments; every
        // created object is released.
        unsafe {
            let name_bytes = b"Monaco";
            let name = CFStringCreateWithBytes(
                std::ptr::null(),
                name_bytes.as_ptr(),
                name_bytes.len() as isize,
                K_CF_STRING_ENCODING_UTF8,
                0,
            );
            assert!(!name.is_null());
            let desc = CTFontDescriptorCreateWithNameAndSize(name, 12.0);
            assert!(!desc.is_null());
            let font = CTFontCreateWithFontDescriptor(desc, 12.0, std::ptr::null());
            assert!(!font.is_null());

            let mut hb_font = create_font(font).unwrap();
            hb_font.destroy();

            CFRelease(font as *const c_void);
            CFRelease(desc);
            CFRelease(name);
        }
    }
}
