//! Port of `src/terminal/search.zig`.
//!
//! STATUS: ported
//!
//! Search functionality for the terminal.
//!
//! Notes on the port:
//! - `options` is [`crate::terminal::build_options`].
//! - The Zig file re-exports each searcher under a short name
//!   (`search.Screen`, `search.Viewport`, ...). Rust re-exports them under
//!   their full names ([`ScreenSearch`], [`ViewportSearch`], ...) plus the
//!   short aliases so both spellings resolve.
//! - `Thread` is only available for the Ghostty application artifact in
//!   Zig (libghostty lacks the xev dependency). This crate always builds the
//!   application, so the module is always available.

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "search/active.rs"]
pub mod active;
#[path = "search/pagelist.rs"]
pub mod pagelist;
#[path = "search/screen.rs"]
pub mod screen;
#[path = "search/sliding_window.rs"]
pub mod sliding_window;
#[path = "search/terminal.rs"]
pub mod terminal;
#[path = "search/thread.rs"]
pub mod thread;
#[path = "search/viewport.rs"]
pub mod viewport;
// @generated-mods-end

pub use crate::terminal::build_options as options;

pub use self::active::ActiveSearch;
pub use self::pagelist::PageListSearch;
pub use self::screen::ScreenSearch;
pub use self::terminal::TerminalSearch;
pub use self::thread::Thread;
pub use self::viewport::ViewportSearch;

/// Zig: `search.Active`.
pub type Active = ActiveSearch;
/// Zig: `search.PageList`.
pub type PageList = PageListSearch;
/// Zig: `search.Screen`.
pub type Screen = ScreenSearch;
/// Zig: `search.Terminal`.
pub type Terminal = TerminalSearch;
/// Zig: `search.Viewport`.
pub type Viewport = ViewportSearch;

/// Test helpers shared by the search submodules.
#[cfg(test)]
pub(crate) mod testing {
    use crate::terminal::csi::EraseDisplay;
    use crate::terminal::point::{Coordinate, Point};
    use crate::terminal::terminal::{Options, Terminal};

    /// Create a terminal with the given size and scrollback limit.
    pub fn terminal(cols: u16, rows: u16, max_scrollback_bytes: Option<usize>) -> Terminal {
        Terminal::init(Options {
            cols,
            rows,
            max_scrollback_bytes,
            ..Default::default()
        })
        .unwrap()
    }

    /// Create a terminal with the default scrollback limit.
    pub fn terminal_default(cols: u16, rows: u16) -> Terminal {
        Terminal::init(Options {
            cols,
            rows,
            ..Default::default()
        })
        .unwrap()
    }

    pub fn active(x: u16, y: u32) -> Option<Point> {
        Some(Point::Active(Coordinate { x, y }))
    }

    pub fn screen(x: u16, y: u32) -> Option<Point> {
        Some(Point::Screen(Coordinate { x, y }))
    }

    /// A minimal VT stream for tests: Zig `t.vtStream().nextSlice(..)`.
    ///
    /// TODO(port-dedupe): replace with `crate::terminal::Terminal::vt_stream`
    /// once `stream_terminal` is ported. This only understands what the
    /// search tests feed: printable ASCII, CR, LF, `CSI Ps J` and
    /// `CSI Ps ; Ps H`.
    pub fn next_slice(t: &mut Terminal, input: &[u8]) {
        let mut i = 0;
        while i < input.len() {
            let c = input[i];
            i += 1;
            match c {
                b'\r' => t.carriage_return(),
                b'\n' => t.linefeed().unwrap(),
                0x1B => {
                    assert_eq!(input[i], b'[', "unsupported escape sequence");
                    i += 1;
                    let mut params: Vec<usize> = Vec::new();
                    let mut cur: Option<usize> = None;
                    let final_byte = loop {
                        let b = input[i];
                        i += 1;
                        match b {
                            b'0'..=b'9' => cur = Some(cur.unwrap_or(0) * 10 + (b - b'0') as usize),
                            b';' => params.push(cur.take().unwrap_or(0)),
                            0x40..=0x7E => {
                                if let Some(v) = cur.take() {
                                    params.push(v);
                                }
                                break b;
                            }
                            _ => panic!("unsupported CSI byte {b:#x}"),
                        }
                    };
                    match final_byte {
                        b'J' => {
                            let mode = params.first().copied().unwrap_or(0);
                            let mode = EraseDisplay::from_int(mode as u8).unwrap();
                            t.erase_display(mode, false);
                        }
                        b'H' => {
                            let row = params.first().copied().filter(|v| *v != 0).unwrap_or(1);
                            let col = params.get(1).copied().filter(|v| *v != 0).unwrap_or(1);
                            t.set_cursor_pos(row, col);
                        }
                        _ => panic!("unsupported CSI final {}", final_byte as char),
                    }
                }
                0x20..=0x7E => t.print(c as u32).unwrap(),
                _ => panic!("unsupported control byte {c:#x}"),
            }
        }
    }
}
