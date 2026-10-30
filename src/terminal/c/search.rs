//! Port of `src/terminal/c/search.zig`.
//!
//! STATUS: partial — only the `GhosttySearch` handle (`SearchWrapper`) is
//! ported, as needed by `c/terminal` (which detaches live searches when a
//! terminal is freed). TODO(c-api batch 3): the search API functions
//! (`search_*`), `Status`, `Option`, `Data`, `Scroll` and the tests.

use super::terminal::Terminal;
use super::types::HandleAllocator;
use crate::terminal::search::terminal::{SelectScroll, TerminalSearch};
use crate::terminal::terminal::Terminal as ZigTerminal;

/// C: GhosttySearch
pub type Search = *mut SearchWrapper;

pub struct SearchWrapper {
    pub alloc: HandleAllocator,

    /// The terminal this search is bound to. This is borrowed, so the
    /// search never frees it. If the terminal is freed first, it sets
    /// this to null to detach the search: calls that need the terminal
    /// then fail cleanly and free releases only search-owned memory.
    pub terminal: Terminal,

    /// The search state. Null when no needle is set: the search
    /// starts idle and returns to idle when the needle is cleared.
    pub search: Option<TerminalSearch>,

    /// The scroll policy applied by the select options. Persistent
    /// across selects, set via the select_scroll option.
    pub select_scroll: SelectScroll,
}

impl SearchWrapper {
    /// The Zig terminal this search is bound to, or null if the
    /// terminal was freed before the search.
    pub fn zig_terminal(&self) -> Option<&mut ZigTerminal> {
        // SAFETY: a non-null terminal is live (the terminal detaches its
        // searches before it is freed).
        unsafe { super::terminal::zig_terminal(self.terminal) }
    }
}
