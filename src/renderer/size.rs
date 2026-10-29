//! Port of `src/renderer/size.zig`.
//!
//! STATUS: partial — only `PaddingBalance` (needed by the config) is ported.

/// Controls how extra whitespace around the terminal grid is distributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaddingBalance {
    /// No balancing; padding is applied as specified explicitly.
    False,
    /// Balances padding but caps the top padding so the first row doesn't
    /// drift too far from the top of the window. Excess vertical space is
    /// shifted to the bottom.
    True,
    /// Distributes leftover space equally on all sides so the grid is
    /// centered within the screen.
    Equal,
}

impl PaddingBalance {
    /// The Zig tag name (`@tagName`).
    pub const fn as_str(self) -> &'static str {
        match self {
            PaddingBalance::False => "false",
            PaddingBalance::True => "true",
            PaddingBalance::Equal => "equal",
        }
    }
}
