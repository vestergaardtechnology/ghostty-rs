//! Port of `src/input/paste.zig`.
//!
//! STATUS: ported
//!
//! Port notes / deviations:
//! - Zig's `encode` takes either a `[]u8` (mutable, never fails) or a
//!   `[]const u8` (may return `error.MutableRequired`) via `anytype`. Rust
//!   splits this into [`encode`] (`&mut [u8]`) and [`encode_const`]
//!   (`&[u8]`).
//! - [`encode_writer`] takes any `std::io::Write`. Zig copies the data
//!   directly into the writer's buffer (`writableSliceGreedy`) and encodes
//!   it in place; Rust has no generic access to a writer's buffer, so the
//!   data is copied and encoded chunk by chunk through a stack buffer and
//!   then written. The bytes written are identical.

use std::fmt;
use std::io::Write;

use crate::terminal::modes::Mode;
use crate::terminal::Terminal;

/// The bracketed paste (mode 2004) frame written around the data.
pub const BRACKETED_PREFIX: &[u8] = b"\x1b[200~";
pub const BRACKETED_SUFFIX: &[u8] = b"\x1b[201~";

/// The maximum number of bytes `encode` adds around the data, so callers
/// can size a buffer for the full encoded result.
pub const MAX_FRAME_SIZE: usize = BRACKETED_PREFIX.len() + BRACKETED_SUFFIX.len();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// True if bracketed paste mode is on.
    pub bracketed: bool,
}

impl Options {
    /// Return the encoding options based on the current terminal state.
    pub fn from_terminal(t: &Terminal) -> Options {
        Options {
            bracketed: t.modes.get(Mode::BracketedPaste),
        }
    }
}

/// These are the set of byte values that are always replaced by
/// a space (per xterm's behavior) for any text insertion method e.g.
/// a paste, drag and drop, etc. These are copied directly from xterm's
/// source.
const STRIP: &[u8] = &[
    0x00, // NUL
    0x08, // BS
    0x05, // ENQ
    0x04, // EOT
    0x1B, // ESC
    0x7F, // DEL
    // These can be overridden by the running terminal program
    // via tcsetattr, so they aren't totally safe to hardcode like
    // this. In practice, I haven't seen modern programs change these
    // and its a much bigger architectural change to pass these through
    // so for now they're hardcoded.
    0x03, // VINTR (Ctrl+C)
    0x1C, // VQUIT (Ctrl+\)
    0x15, // VKILL (Ctrl+U)
    0x1A, // VSUSP (Ctrl+Z)
    0x11, // VSTART (Ctrl+Q)
    0x13, // VSTOP (Ctrl+S)
    0x17, // VWERASE (Ctrl+W)
    0x16, // VLNEXT (Ctrl+V)
    0x12, // VREPRINT (Ctrl+R)
    0x0F, // VDISCARD (Ctrl+O)
];

#[inline]
fn is_strip(b: u8) -> bool {
    STRIP.contains(&b)
}

/// Encode the given data for pasting. The resulting value can be written
/// to the pty to perform a paste of the input data.
///
/// This is the mutable form of Zig's `encode`: the data is modified in
/// place and this can't fail. See [`encode_const`] for the const form.
///
/// The data is returned as a set of slices to limit allocations. The caller
/// can combine the slices into a single buffer if desired.
///
/// WARNING: The input data is not checked for safety. See the `is_safe`
/// function to check if the data is safe to paste.
pub fn encode(data: &mut [u8], opts: Options) -> [&[u8]; 3] {
    // If we have any of the strip values, then we need to replace them
    // with spaces. This is what xterm does and it does it regardless
    // of bracketed paste mode. This is a security measure to prevent pastes
    // from containing bytes that could be used to inject commands.
    for b in data.iter_mut() {
        if is_strip(*b) {
            *b = b' ';
        }
    }

    // Bracketed paste mode (mode 2004) wraps pasted data in
    // fenceposts so that the terminal can ignore things like newlines.
    if opts.bracketed {
        return [BRACKETED_PREFIX, data, BRACKETED_SUFFIX];
    }

    // Non-bracketed. We have to replace newline with `\r`. This matches
    // the behavior of xterm and other terminals. For `\r\n` this will
    // result in `\r\r` which does match xterm.
    for b in data.iter_mut() {
        if *b == b'\n' {
            *b = b'\r';
        }
    }

    [b"", data, b""]
}

/// The const form of Zig's `encode`: returns `Error::MutableRequired` if
/// encoding requires a mutable copy of the data. The caller can make a
/// mutable copy after seeing the error and use [`encode`].
pub fn encode_const(data: &[u8], opts: Options) -> Result<[&[u8]; 3], Error> {
    if data.iter().any(|&b| is_strip(b)) {
        return Err(Error::MutableRequired);
    }

    if opts.bracketed {
        return Ok([BRACKETED_PREFIX, data, BRACKETED_SUFFIX]);
    }

    if memchr::memchr(b'\n', data).is_some() {
        return Err(Error::MutableRequired);
    }

    Ok([b"", data, b""])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Returned if encoding requires a mutable copy of the data. This
    /// can only be returned if the input data type is const.
    MutableRequired,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MutableRequired")
    }
}

impl std::error::Error for Error {}

/// Encode the given data for pasting directly into a writer. This is
/// the same transformation as `encode` (unsafe bytes replaced, bracketed
/// frame or newline conversion per `opts`) but the data is never
/// modified and no allocation is made. This is the form to use when the
/// data is const and the result is being assembled into a single buffer
/// anyway.
///
/// The data is copied and encoded in chunks, so any writer works; a
/// writer with less total capacity than needed reports an error as usual.
///
/// WARNING: The input data is not checked for safety. See `is_safe`
/// and `is_safe_with` to check if the data is safe to paste.
pub fn encode_writer<W: Write + ?Sized>(writer: &mut W, data: &[u8], opts: Options) -> std::io::Result<()> {
    if opts.bracketed {
        writer.write_all(BRACKETED_PREFIX)?;
    }

    // The byte transformations are position-independent, so the data
    // can be copied and encoded chunk by chunk. The frame returned by
    // encode is ignored since it's written around the whole data here.
    let mut buf = [0u8; 4096];
    for chunk in data.chunks(buf.len()) {
        let dest = &mut buf[..chunk.len()];
        dest.copy_from_slice(chunk);
        let _ = encode(dest, opts);
        writer.write_all(dest)?;
    }

    if opts.bracketed {
        writer.write_all(BRACKETED_SUFFIX)?;
    }
    Ok(())
}

/// Returns true if the data looks safe to paste. Data is considered
/// unsafe if it contains any of the following:
///
/// - `\n` or `\r`: Line terminators can be used to inject commands.
///   A carriage return is what the Enter key sends, and an unbracketed
///   paste converts newlines to carriage returns anyway, so the two are
///   equivalent on the input stream. Other terminals that warn on
///   multi-line pastes (iTerm2, Windows Terminal, kitty) treat both
///   bytes as line terminators as well.
/// - `\x1b[201~`: This is the end of a bracketed paste. This can be used
///   to exit a bracketed paste and inject commands.
///
/// We consider any scenario unsafe regardless of current terminal state.
/// For example, even if bracketed paste mode is not active, we still
/// consider `\x1b[201~` unsafe. The existence of these types of bytes
/// should raise suspicion that the producer of the paste data is
/// acting strangely.
pub fn is_safe(data: &[u8]) -> bool {
    memchr::memchr2(b'\r', b'\n', data).is_none() && memchr::memmem::find(data, b"\x1b[201~").is_none()
}

/// Returns true if the data looks safe to paste given how it will be
/// encoded. This is the terminal-state-aware counterpart of `is_safe`:
///
/// - Bracketed (mode 2004 on): the program receives the data as one
///   framed unit, so line terminators are fine. The data is unsafe only if it
///   contains the end of the frame (`\x1b[201~`), which would let the
///   rest of the data escape the frame and inject commands.
/// - Unbracketed: the same rule as `is_safe`.
///
/// Callers wanting the conservative rule regardless of terminal state
/// should use `is_safe` instead.
pub fn is_safe_with(data: &[u8], opts: Options) -> bool {
    if opts.bracketed {
        return memchr::memmem::find(data, BRACKETED_SUFFIX).is_none();
    }
    is_safe(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRACKETED: Options = Options { bracketed: true };
    const UNBRACKETED: Options = Options { bracketed: false };

    #[test]
    fn is_safe_test() {
        assert!(is_safe(b"hello"));
        assert!(is_safe(b"hello\tworld"));
        assert!(!is_safe(b"hello\n"));
        assert!(!is_safe(b"hello\nworld"));
        assert!(!is_safe(b"he\x1b[201~llo"));

        // A carriage return is the Enter key on the input stream, so it
        // can execute a command exactly like a newline.
        assert!(!is_safe(b"hello\r"));
        assert!(!is_safe(b"hello\rworld"));
        assert!(!is_safe(b"hello\r\nworld"));
        assert!(!is_safe(b"\r"));
    }

    #[test]
    fn is_safe_with_test() {
        // Bracketed: line terminators are fine, the frame terminator is not.
        assert!(is_safe_with(b"hello", BRACKETED));
        assert!(is_safe_with(b"hello\nworld", BRACKETED));
        assert!(is_safe_with(b"hello\rworld", BRACKETED));
        assert!(is_safe_with(b"hello\r\nworld", BRACKETED));
        assert!(!is_safe_with(b"he\x1b[201~llo", BRACKETED));
        assert!(!is_safe_with(b"hello\n\x1b[201~", BRACKETED));

        // Unbracketed: the conservative rule.
        assert!(is_safe_with(b"hello", UNBRACKETED));
        assert!(!is_safe_with(b"hello\nworld", UNBRACKETED));
        assert!(!is_safe_with(b"hello\rworld", UNBRACKETED));
        assert!(!is_safe_with(b"hello\r", UNBRACKETED));
        assert!(!is_safe_with(b"he\x1b[201~llo", UNBRACKETED));
    }

    fn fixed_writer(buf: &mut [u8], f: impl FnOnce(&mut &mut [u8]) -> std::io::Result<()>) -> std::io::Result<usize> {
        let total = buf.len();
        let mut writer: &mut [u8] = buf;
        f(&mut writer)?;
        Ok(total - writer.len())
    }

    #[test]
    fn encode_writer_bracketed() {
        let mut buf = [0u8; 64];
        let n = fixed_writer(&mut buf, |w| encode_writer(w, b"hel\x1blo\nworld", BRACKETED)).unwrap();
        assert_eq!(b"\x1b[200~hel lo\nworld\x1b[201~", &buf[..n]);
    }

    #[test]
    fn encode_writer_unbracketed() {
        let mut buf = [0u8; 64];
        let n = fixed_writer(&mut buf, |w| encode_writer(w, b"hel\x00lo\r\nworld", UNBRACKETED)).unwrap();
        assert_eq!(b"hel lo\r\rworld", &buf[..n]);
    }

    #[test]
    fn encode_writer_empty() {
        let mut buf = [0u8; 64];
        let n = fixed_writer(&mut buf, |w| encode_writer(w, b"", BRACKETED)).unwrap();
        assert_eq!(b"\x1b[200~\x1b[201~", &buf[..n]);
        let n = fixed_writer(&mut buf, |w| encode_writer(w, b"", UNBRACKETED)).unwrap();
        assert_eq!(b"", &buf[..n]);
    }

    #[test]
    fn encode_writer_chunks_through_a_small_writer_buffer() {
        // A writer with a 4-byte staging buffer that drains into a list,
        // so the data is copied and encoded in several chunks.
        let mut sink = std::io::BufWriter::with_capacity(4, Vec::new());
        let data = b"line one\nline\x1btwo\nline three\n";
        encode_writer(&mut sink, data, BRACKETED).unwrap();
        sink.flush().unwrap();
        let list = sink.into_inner().unwrap();
        assert_eq!(&b"\x1b[200~line one\nline two\nline three\n\x1b[201~"[..], &list[..]);
    }

    #[test]
    fn encode_writer_too_small() {
        let mut buf = [0u8; 4];
        assert!(fixed_writer(&mut buf, |w| encode_writer(w, b"hello", BRACKETED)).is_err());
    }

    #[test]
    fn max_frame_size() {
        let result = encode_const(b"", BRACKETED).unwrap();
        assert_eq!(MAX_FRAME_SIZE, result[0].len() + result[2].len());
    }

    #[test]
    fn encode_bracketed() {
        let result = encode_const(b"hello", BRACKETED).unwrap();
        assert_eq!(b"\x1b[200~", result[0]);
        assert_eq!(b"hello", result[1]);
        assert_eq!(b"\x1b[201~", result[2]);
    }

    #[test]
    fn encode_unbracketed_no_newlines() {
        let result = encode_const(b"hello", UNBRACKETED).unwrap();
        assert_eq!(b"", result[0]);
        assert_eq!(b"hello", result[1]);
        assert_eq!(b"", result[2]);
    }

    #[test]
    fn encode_unbracketed_newlines_const() {
        assert_eq!(Err(Error::MutableRequired), encode_const(b"hello\nworld", UNBRACKETED));
    }

    #[test]
    fn encode_unbracketed_newlines() {
        let mut data = b"hello\nworld".to_vec();
        let result = encode(&mut data, UNBRACKETED);
        assert_eq!(b"", result[0]);
        assert_eq!(b"hello\rworld", result[1]);
        assert_eq!(b"", result[2]);
    }

    #[test]
    fn encode_unbracketed_windows_stye_newline() {
        let mut data = b"hello\r\nworld".to_vec();
        let result = encode(&mut data, UNBRACKETED);
        assert_eq!(b"", result[0]);
        assert_eq!(b"hello\r\rworld", result[1]);
        assert_eq!(b"", result[2]);
    }

    #[test]
    fn encode_strip_unsafe_bytes_const() {
        assert_eq!(Err(Error::MutableRequired), encode_const(b"hello\x00world", BRACKETED));
    }

    #[test]
    fn encode_strip_unsafe_bytes_mutable_bracketed() {
        let mut data = b"hel\x1blo\x00world".to_vec();
        let result = encode(&mut data, BRACKETED);
        assert_eq!(b"\x1b[200~", result[0]);
        assert_eq!(b"hel lo world", result[1]);
        assert_eq!(b"\x1b[201~", result[2]);
    }

    #[test]
    fn encode_strip_unsafe_bytes_mutable_unbracketed() {
        let mut data = b"hel\x03lo".to_vec();
        let result = encode(&mut data, UNBRACKETED);
        assert_eq!(b"", result[0]);
        assert_eq!(b"hel lo", result[1]);
        assert_eq!(b"", result[2]);
    }

    #[test]
    fn encode_strip_multiple_unsafe_bytes() {
        let mut data = b"\x00\x08\x7f".to_vec();
        let result = encode(&mut data, BRACKETED);
        assert_eq!(b"   ", result[1]);
    }
}
