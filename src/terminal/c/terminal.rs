//! Port of `src/terminal/c/terminal.zig`.
//!
//! STATUS: partial — only the `GhosttyTerminal` handle (`TerminalWrapper`
//! with its `terminal` field), `new` and `free` are ported, as needed by
//! the batch-1 C API files (`key_encode`/`mouse_encode`
//! `setopt_from_terminal`). TODO(c-api batch 2): everything else (the
//! persistent VT stream, effects/callbacks, options, get/set, clipboard
//! and device types, `vt_write`, scrolling, grid refs, ...).

use super::result::Result;
use super::types::HandleAllocator;
use crate::lib::allocator::Allocator as CAllocator;
use crate::terminal::osc::semantic_prompt as osc_sp;
use crate::terminal::size::CellCountInt;
use crate::terminal::terminal::{self as zig_terminal, Terminal as ZigTerminal};

/// Wrapper around ZigTerminal that tracks additional state for C API usage.
///
/// TODO(c-api batch 2): the stream, effects, tmp dir path, terminfo name,
/// tracked grid refs and searches.
pub struct TerminalWrapper {
    pub terminal: Box<ZigTerminal>,
    alloc: HandleAllocator,
}

/// C: GhosttyTerminal
pub type Terminal = *mut TerminalWrapper;

/// `error{InvalidValue, OutOfMemory}`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewError {
    InvalidValue,
    OutOfMemory,
}

/// C: ghostty_terminal_new
#[export_name = "ghostty_terminal_new"]
pub extern "C" fn new(
    alloc_: Option<&CAllocator>,
    result: &mut Terminal,
    cols: CellCountInt,
    rows: CellCountInt,
) -> Result {
    match new_(alloc_, cols, rows) {
        Ok(t) => {
            *result = t;
            Result::Success
        }
        Err(err) => {
            *result = std::ptr::null_mut();
            match err {
                NewError::InvalidValue => Result::InvalidValue,
                NewError::OutOfMemory => Result::OutOfMemory,
            }
        }
    }
}

fn new_(alloc_: Option<&CAllocator>, cols: CellCountInt, rows: CellCountInt) -> std::result::Result<Terminal, NewError> {
    if cols == 0 || rows == 0 {
        return Err(NewError::InvalidValue);
    }

    let alloc = HandleAllocator::new(alloc_);

    // Setup our terminal
    let mut t = ZigTerminal::init(zig_terminal::Options {
        cols,
        rows,
        ..Default::default()
    })
    .map_err(|e| match e {
        zig_terminal::InitError::InvalidValue => NewError::InvalidValue,
        zig_terminal::InitError::OutOfMemory => NewError::OutOfMemory,
    })?;

    // libghostty-vt embedders don't necessarily install Ghostty's shell
    // integration, so don't assume OSC 133 prompts can be redrawn on resize.
    // Shells can still opt in with OSC 133;A;redraw=1.
    t.flags.shell_redraws_prompt = osc_sp::Redraw::False;

    let ptr = alloc.create(TerminalWrapper {
        terminal: Box::new(t),
        alloc,
    });
    if ptr.is_null() {
        return Err(NewError::OutOfMemory);
    }
    Ok(ptr)
}

/// C: ghostty_terminal_free
///
/// # Safety
/// `terminal_` must be NULL or a terminal from [`new`] not used afterwards.
#[export_name = "ghostty_terminal_free"]
pub unsafe extern "C" fn free(terminal_: Terminal) {
    let Some(wrapper) = terminal_.as_ref() else {
        return;
    };
    let alloc = wrapper.alloc;
    alloc.destroy(terminal_);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lib::allocator::C_ALLOCATOR;

    #[test]
    fn new_free() {
        let mut t: Terminal = std::ptr::null_mut();
        assert_eq!(Result::Success, new(Some(&C_ALLOCATOR), &mut t, 80, 24));
        assert!(!t.is_null());
        unsafe { free(t) };
    }

    #[test]
    fn new_invalid_value() {
        let mut t: Terminal = std::ptr::null_mut();
        assert_eq!(Result::InvalidValue, new(Some(&C_ALLOCATOR), &mut t, 0, 24));
        assert!(t.is_null());
        unsafe { free(t) };
    }
}
