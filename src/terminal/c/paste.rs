//! Port of `src/terminal/c/paste.zig`.
//!
//! STATUS: partial — `terminal_paste` (`ghostty_terminal_paste`), its
//! `Sink`/`readTrampoline` helpers and its four tests need the full
//! `c/terminal` wrapper (effects, write_pty/clipboard_read callbacks,
//! `set`, `vt_write`, the stream handler's `paste`), which is
//! TODO(c-api batch 2). Everything else, including the `GhosttyPaste`
//! request type, is ported.

use super::io;
use super::result::Result;
use super::types::{slice_or_empty, CApiEnum};
use crate::input::paste;
use crate::lib::r#enum::LibEnum;
use crate::lib::String as LibString;
use crate::terminal::clipboard;

crate::lib_enum! {
    /// Why a paste happened. The flat C form of the Zig tagged union
    /// (terminal.paste.Source); the location rides alongside in
    /// GhosttyPaste and only applies to a clipboard paste.
    ///
    /// C: GhosttyPasteSource
    pub enum Source: c {
        Clipboard,
        Text,
    }
}

impl CApiEnum for Source {
    fn zig_fields() -> Vec<(String, i64)> {
        Source::FIELDS
            .iter()
            .map(|(name, v)| (super::types::zig_name(name), v.backing() as i64))
            .collect()
    }
}

/// A paste of clipboard contents into the terminal. Sized struct.
///
/// C: GhosttyPaste
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub size: usize,
    pub location: clipboard::Location,
    pub source: Source,
    pub mimes: *const LibString,
    pub mimes_len: usize,
    pub reader: io::MimeReader,
    pub allow_unsafe: bool,
}

impl Request {
    /// A request with the `size` field set (Zig: the field default).
    pub fn new(
        location: clipboard::Location,
        source: Source,
        mimes: *const LibString,
        mimes_len: usize,
        reader: io::MimeReader,
        allow_unsafe: bool,
    ) -> Request {
        Request {
            size: std::mem::size_of::<Request>(),
            location,
            source,
            mimes,
            mimes_len,
            reader,
            allow_unsafe,
        }
    }
}

// TODO(c-api batch 2): `terminal_paste` (C: ghostty_terminal_paste) with
// its `Sink` and `readTrampoline` helpers, once `c/terminal` has the
// effects and stream handler.

/// C: ghostty_paste_is_safe
///
/// # Safety
/// A non-null `data` must be valid for `len` reads.
#[export_name = "ghostty_paste_is_safe"]
pub unsafe extern "C" fn is_safe(data: *const u8, len: usize) -> bool {
    paste::is_safe(slice_or_empty(data, len))
}

/// C: ghostty_paste_encode
///
/// # Safety
/// A non-null `data` must be valid for `data_len` reads and writes (unsafe
/// bytes are replaced in place); a non-null `out_` must be valid for
/// `out_len` writable bytes.
#[export_name = "ghostty_paste_encode"]
pub unsafe extern "C" fn encode(
    data: *mut u8,
    data_len: usize,
    bracketed: bool,
    out_: *mut u8,
    out_len: usize,
    out_written: &mut usize,
) -> Result {
    let slice: &mut [u8] = if data.is_null() || data_len == 0 {
        &mut []
    } else {
        std::slice::from_raw_parts_mut(data, data_len)
    };
    let result = paste::encode(slice, paste::Options { bracketed });

    let total = result[0].len() + result[1].len() + result[2].len();
    *out_written = total;

    let out: &mut [u8] = if out_.is_null() {
        &mut []
    } else {
        std::slice::from_raw_parts_mut(out_, out_len)
    };
    if out.len() < total {
        return Result::OutOfSpace;
    }

    let mut offset: usize = 0;
    for segment in result {
        out[offset..offset + segment.len()].copy_from_slice(segment);
        offset += segment.len();
    }

    Result::Success
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_int;

    fn run_encode(input: &[u8], bracketed: bool, buf: Option<&mut [u8]>) -> (Result, usize) {
        let mut data = input.to_vec();
        let mut written: usize = 0;
        let result = unsafe {
            match buf {
                Some(buf) => encode(
                    data.as_mut_ptr(),
                    data.len(),
                    bracketed,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut written,
                ),
                None => encode(data.as_mut_ptr(), data.len(), bracketed, std::ptr::null_mut(), 0, &mut written),
            }
        };
        (result, written)
    }

    #[test]
    fn encode_bracketed() {
        let mut buf = [0u8; 64];
        let (result, written) = run_encode(b"hello", true, Some(&mut buf));
        assert_eq!(Result::Success, result);
        assert_eq!(b"\x1b[200~hello\x1b[201~", &buf[..written]);
    }

    #[test]
    fn encode_unbracketed_no_newlines() {
        let mut buf = [0u8; 64];
        let (result, written) = run_encode(b"hello", false, Some(&mut buf));
        assert_eq!(Result::Success, result);
        assert_eq!(b"hello", &buf[..written]);
    }

    #[test]
    fn encode_unbracketed_newlines() {
        let mut buf = [0u8; 64];
        let (result, written) = run_encode(b"hello\nworld", false, Some(&mut buf));
        assert_eq!(Result::Success, result);
        assert_eq!(b"hello\rworld", &buf[..written]);
    }

    #[test]
    fn encode_strip_unsafe_bytes() {
        let mut buf = [0u8; 64];
        let (result, written) = run_encode(b"hel\x1blo\x00world", true, Some(&mut buf));
        assert_eq!(Result::Success, result);
        assert_eq!(b"\x1b[200~hel lo world\x1b[201~", &buf[..written]);
    }

    #[test]
    fn encode_with_insufficient_buffer() {
        let mut buf = [0u8; 1];
        let (result, written) = run_encode(b"hello", true, Some(&mut buf));
        assert_eq!(Result::OutOfSpace, result);
        assert_eq!(17, written);
    }

    #[test]
    fn encode_with_null_buffer() {
        let (result, written) = run_encode(b"hello", true, None);
        assert_eq!(Result::OutOfSpace, result);
        assert_eq!(17, written);
    }

    #[test]
    fn is_safe_with_safe_data() {
        let safe = b"hello world";
        assert!(unsafe { is_safe(safe.as_ptr(), safe.len()) });
    }

    #[test]
    fn is_safe_with_newline() {
        let unsafe_ = b"hello\nworld";
        assert!(!unsafe { is_safe(unsafe_.as_ptr(), unsafe_.len()) });
    }

    #[test]
    fn is_safe_with_carriage_return() {
        let unsafe_ = b"hello\rworld";
        assert!(!unsafe { is_safe(unsafe_.as_ptr(), unsafe_.len()) });
    }

    #[test]
    fn is_safe_with_bracketed_paste_end() {
        let unsafe_ = b"hello\x1b[201~world";
        assert!(!unsafe { is_safe(unsafe_.as_ptr(), unsafe_.len()) });
    }

    #[test]
    fn is_safe_with_empty_data() {
        let empty = b"";
        assert!(unsafe { is_safe(empty.as_ptr(), 0) });
    }

    #[test]
    fn is_safe_with_null_empty_data() {
        assert!(unsafe { is_safe(std::ptr::null(), 0) });
    }

    #[test]
    #[ignore = "TODO(c-api batch 2): terminal_paste needs the full c/terminal wrapper"]
    fn terminal_paste_null_handling() {}

    #[test]
    #[ignore = "TODO(c-api batch 2): terminal_paste needs the full c/terminal wrapper"]
    fn terminal_paste_without_write_pty_is_invalid() {}

    #[test]
    #[ignore = "TODO(c-api batch 2): terminal_paste needs the full c/terminal wrapper"]
    fn terminal_paste_text_and_unsafe() {}

    #[test]
    #[ignore = "TODO(c-api batch 2): terminal_paste needs the full c/terminal wrapper"]
    fn terminal_paste_refused_write_ignored_by_the_reader() {}

    #[test]
    #[ignore = "TODO(c-api batch 2): terminal_paste needs the full c/terminal wrapper"]
    fn terminal_paste_event() {}

    #[test]
    fn request_layout() {
        let req = Request::new(
            clipboard::Location::STANDARD,
            Source::Clipboard,
            std::ptr::null(),
            0,
            io::MimeReader::default(),
            false,
        );
        assert_eq!(std::mem::size_of::<Request>(), req.size);
        assert_eq!(0, Source::Clipboard as c_int);
        assert_eq!(1, Source::Text as c_int);
    }
}
