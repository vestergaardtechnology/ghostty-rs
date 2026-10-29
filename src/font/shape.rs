//! Port of `src/font/shape.zig`.
//!
//! STATUS: ported
//!
//! Port notes:
//! * The shaper implementations live in `font::shaper::*` (the Zig files
//!   are under `font/shaper/`); they are re-exported here like in Zig.
//! * `RunOptions.cells` is a slice of [`RenderCell`] (the Zig type is a
//!   `std.MultiArrayList(terminal.RenderState.Cell).Slice`).

use crate::config::FontShapingBreak;
use crate::font::shared_grid::SharedGrid;
use crate::terminal::page;
use crate::terminal::style::Style;

pub use super::shaper::cache::Cache;
pub use super::shaper::coretext;
pub use super::shaper::feature::{Feature, FeatureList, DEFAULT_FEATURES};
pub use super::shaper::harfbuzz;
pub use super::shaper::noop;
pub use super::shaper::run::{RunIterator, RunIteratorHook, TextRun};
pub use super::shaper::web_canvas;

/// Zig `default_features`.
pub use super::shaper::feature::DEFAULT_FEATURES as default_features;

/// Shaper implementation for our compile options.
///
/// `.freetype`, `.freetype_windows`, `.fontconfig_freetype`,
/// `.coretext_freetype` and `.coretext_harfbuzz` use the HarfBuzz shaper.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_arch = "wasm32")))]
pub type Shaper = harfbuzz::Shaper;

/// Shaper implementation for our compile options.
///
/// Note that coretext_freetype cannot use the coretext
/// shaper because the coretext shaper requests CoreText
/// font faces.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub type Shaper = coretext::Shaper;

/// Shaper implementation for our compile options.
#[cfg(target_arch = "wasm32")]
pub type Shaper = web_canvas::Shaper;

/// A cell is a single glyph within a terminal that should be rendered
/// for a shaping call. Not all terminal cells may be present; only
/// cells that have a glyph that needs to be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    /// The X position of this shaper cell relative to the offset of the
    /// run. Because runs are always within a single row, it is expected
    /// that the caller can reconstruct the full position of the cell by
    /// using the known Y position of the cell and adding the X position
    /// to the run offset.
    pub x: u16,

    /// An additional offset to apply to the rendering.
    pub x_offset: i16,
    pub y_offset: i16,

    /// The glyph index for this cell. The font index to use alongside
    /// this cell is available in the text run. This glyph index is only
    /// valid for a given GroupCache and FontIndex that was used to create
    /// the runs.
    pub glyph_index: u32,
}

/// Options for shapers.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// Font features to use when shaping.
    ///
    /// Note: eventually, this will move to font.Face probably as we may
    /// want to support per-face feature configuration. For now, we only
    /// support applying features globally.
    pub features: &'a [&'a str],
}

/// One cell of a render state row (Zig `terminal.RenderState.Cell`).
///
/// TODO(port-dedupe): replace with crate::terminal::render::RenderState's
/// `Cell` once `terminal/render.zig` is ported.
#[derive(Debug, Clone, Default)]
pub struct RenderCell {
    /// Always set, this is the raw copied cell data from page.Cell.
    /// The managed memory (hyperlinks, graphames, etc.) is NOT safe
    /// to access from here. It is duplicated into the other fields if
    /// it exists.
    pub raw: page::Cell,

    /// Grapheme data for the cell. This is undefined unless the
    /// raw cell's content_tag is `codepoint_grapheme`.
    pub grapheme: Vec<u32>,

    /// The style data for the cell. This is undefined unless
    /// the style_id is non-default on raw.
    pub style: Style,
}

/// Options for runIterator.
#[derive(Clone, Copy)]
pub struct RunOptions<'a> {
    /// The font state for the terminal screen. This is mutable because
    /// cached values may be updated during shaping.
    pub grid: &'a SharedGrid,

    /// The cells for the row to shape.
    pub cells: &'a [RenderCell],

    /// The x boundaries of the selection in this row.
    pub selection: Option<[u16; 2]>,

    /// The cursor position within this row. This is used to break shaping
    /// on cursor boundaries. This can be disabled by setting this to
    /// null.
    pub cursor_x: Option<usize>,
}

impl<'a> RunOptions<'a> {
    /// Zig `.{ .grid = grid }` with the other fields at their defaults.
    pub fn new(grid: &'a SharedGrid) -> RunOptions<'a> {
        RunOptions {
            grid,
            cells: &[],
            selection: None,
            cursor_x: None,
        }
    }

    /// Apply the font break configuration to the run.
    pub fn apply_break_config(&mut self, config: FontShapingBreak) {
        if !config.cursor {
            self.cursor_x = None;
        }
    }
}
