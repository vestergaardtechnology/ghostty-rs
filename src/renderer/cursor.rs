//! Port of `src/renderer/cursor.zig`.
//!
//! STATUS: partial — logic and tests ported, but against a minimal
//! stand-in for `terminal.RenderState` (not yet ported); the tests build
//! the render state that `RenderState.update` would produce instead of
//! driving a real `Terminal` (TODO(port-dedupe))

use crate::terminal::cursor as terminal_cursor;

use self::terminal_standin as terminal;

/// Available cursor styles for drawing that renderers must support.
/// This is a superset of terminal cursor styles since the renderer supports
/// some additional cursor states such as the hollow block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Style {
    // Typical cursor input styles
    Block,
    BlockHollow,
    Bar,
    Underline,

    // Special cursor styles
    Lock,
}

impl Style {
    /// Create a cursor style from the terminal style request.
    pub fn from_terminal(term: terminal_cursor::Style) -> Style {
        match term {
            terminal_cursor::Style::Bar => Style::Bar,
            terminal_cursor::Style::Block => Style::Block,
            terminal_cursor::Style::BlockHollow => Style::BlockHollow,
            terminal_cursor::Style::Underline => Style::Underline,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StyleOptions {
    pub preedit: bool,
    pub focused: bool,
    pub blink_visible: bool,
}

/// Returns the cursor style to use for the current render state or null
/// if a cursor should not be rendered at all.
pub fn style(state: &terminal::RenderState, opts: StyleOptions) -> Option<Style> {
    // Note the order of conditionals below is important. It represents
    // a priority system of how we determine what state overrides cursor
    // visibility and style.

    // The cursor must be visible in the viewport to be rendered.
    state.cursor.viewport?;

    // If we are in preedit, then we always show the block cursor. We do
    // this even if the cursor is explicitly not visible because it shows
    // an important editing state to the user.
    if opts.preedit {
        return Some(Style::Block);
    }

    // If we're at a password input its always a lock.
    if state.cursor.password_input {
        return Some(Style::Lock);
    }

    // If the cursor is explicitly not visible by terminal mode, we don't render.
    if !state.cursor.visible {
        return None;
    }

    // If we're not focused, our cursor is always visible so that
    // we can show the hollow box.
    if !opts.focused {
        return Some(Style::BlockHollow);
    }

    // If the cursor is blinking and our blink state is not visible,
    // then we don't show the cursor.
    if state.cursor.blinking && !opts.blink_visible {
        return None;
    }

    // Otherwise, we use whatever style the terminal wants.
    Some(Style::from_terminal(state.cursor.visual_style))
}

/// Minimal stand-in for the cursor part of `terminal.RenderState`.
///
/// TODO(port-dedupe): replace with crate::terminal::render::RenderState
/// once it is ported.
pub mod terminal_standin {
    use crate::terminal::cursor;
    use crate::terminal::size::CellCountInt;

    /// Zig: `RenderState` (only `cursor`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct RenderState {
        pub cursor: Cursor,
    }

    /// Zig: `RenderState.Cursor` (the fields the renderer cursor logic
    /// reads).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Cursor {
        /// The x/y position of the cursor within the viewport. This
        /// may be null if the cursor is not visible within the viewport.
        pub viewport: Option<CursorViewport>,

        /// The visual style of the cursor itself, such as a block or
        /// bar.
        pub visual_style: cursor::Style,

        /// True if the cursor is detected to be at a password input field.
        pub password_input: bool,

        /// Cursor visibility state determined by the terminal mode.
        pub visible: bool,

        /// Cursor blink state determined by the terminal mode.
        pub blinking: bool,
    }

    impl Default for Cursor {
        /// The values of `RenderState.empty`.
        fn default() -> Self {
            Cursor {
                viewport: None,
                visual_style: cursor::Style::Block,
                password_input: false,
                visible: true,
                blinking: false,
            }
        }
    }

    /// Zig: `RenderState.Cursor.Viewport`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct CursorViewport {
        pub x: CellCountInt,
        pub y: CellCountInt,
        pub wide_tail: bool,
    }
}

#[cfg(test)]
mod tests {
    use super::terminal_standin::{Cursor, CursorViewport, RenderState};
    use super::*;

    // TODO(port-dedupe): these tests build the render state of a fresh
    // 10x10 `Terminal` (cursor at the origin of the viewport, the default
    // `cursor_visible` mode on) with the given settings, as
    // `RenderState.update` would; switch to driving a real `Terminal`
    // once it is ported.

    /// A render state for a terminal with `cursor_style` and the given
    /// `cursor_visible`/`cursor_blinking` modes.
    fn state(
        cursor_style: terminal_cursor::Style,
        cursor_visible: bool,
        cursor_blinking: bool,
    ) -> RenderState {
        RenderState {
            cursor: Cursor {
                viewport: Some(CursorViewport::default()),
                visual_style: cursor_style,
                password_input: false,
                visible: cursor_visible,
                blinking: cursor_blinking,
            },
        }
    }

    fn opts(preedit: bool, focused: bool, blink_visible: bool) -> StyleOptions {
        StyleOptions {
            preedit,
            focused,
            blink_visible,
        }
    }

    #[test]
    fn cursor_default_uses_configured_style() {
        // cursor_style = .bar, cursor_blinking = true
        let state = state(terminal_cursor::Style::Bar, true, true);

        assert!(style(&state, opts(false, true, true)) == Some(Style::Bar));
        assert!(style(&state, opts(false, false, true)) == Some(Style::BlockHollow));
        assert!(style(&state, opts(false, false, false)) == Some(Style::BlockHollow));
        assert!(style(&state, opts(false, true, false)).is_none());
    }

    #[test]
    fn cursor_blinking_disabled() {
        // cursor_style = .bar, cursor_blinking = false
        let state = state(terminal_cursor::Style::Bar, true, false);

        let o = |focused, blink_visible| StyleOptions {
            focused,
            blink_visible,
            ..Default::default()
        };
        assert!(style(&state, o(true, true)) == Some(Style::Bar));
        assert!(style(&state, o(true, false)) == Some(Style::Bar));
        assert!(style(&state, o(false, true)) == Some(Style::BlockHollow));
        assert!(style(&state, o(false, false)) == Some(Style::BlockHollow));
    }

    #[test]
    fn cursor_explicitly_not_visible() {
        // cursor_style = .bar, cursor_visible = false, cursor_blinking = false
        let state = state(terminal_cursor::Style::Bar, false, false);

        let o = |focused, blink_visible| StyleOptions {
            focused,
            blink_visible,
            ..Default::default()
        };
        assert!(style(&state, o(true, true)).is_none());
        assert!(style(&state, o(true, false)).is_none());
        assert!(style(&state, o(false, true)).is_none());
        assert!(style(&state, o(false, false)).is_none());
    }

    #[test]
    fn cursor_always_block_with_preedit() {
        // A default terminal: block cursor, visible, not blinking.
        let mut state = state(terminal_cursor::Style::Block, true, false);

        // In any bool state
        assert!(style(&state, opts(true, false, false)) == Some(Style::Block));
        assert!(style(&state, opts(true, true, false)) == Some(Style::Block));
        assert!(style(&state, opts(true, true, true)) == Some(Style::Block));
        assert!(style(&state, opts(true, false, true)) == Some(Style::Block));

        // If we're scrolled though, then we don't show the cursor.
        // (100 index() calls then scrollViewport(.top): the cursor is no
        // longer in the viewport.)
        state.cursor.viewport = None;

        // In any bool state
        assert!(style(&state, opts(true, false, false)).is_none());
        assert!(style(&state, opts(true, true, false)).is_none());
        assert!(style(&state, opts(true, true, true)).is_none());
        assert!(style(&state, opts(true, false, true)).is_none());
    }
}
