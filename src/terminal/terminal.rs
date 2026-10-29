//! Port of `src/terminal/Terminal.zig`.
//!
//! STATUS: partial — implementation ported; tests in progress.
//!
//! The primary terminal emulation structure. This represents a single
//! "terminal" containing a grid of characters and exposes various operations
//! on that grid. This also maintains the scrollback buffer.
//!
//! Notes on the port:
//! - `io`/`alloc` parameters and the `io()`/`gpa()` accessors are dropped
//!   (global allocator, no `std.Io`). `deinit` is `Drop`.
//! - Screens are addressed through [`ScreenPtr`] (Zig `*Screen`), see
//!   `screen_set.rs`.
//! - `vtStream`/`vtHandler` are provided by the `stream_terminal` port (they
//!   construct its `Stream`/`Handler` types); `kittyGraphics` is provided by
//!   the `kitty/graphics_exec` port (it wraps `kitty.graphics.execute`).
//! - The batched print fast path (`printSlice`) uses scalar loops where the
//!   Zig version hand-vectorizes with `@Vector` (identical results).
//! - Kitty image placements: the `graphics_storage` port does not store
//!   placements yet, so the scroll-margin adjustment paths use local
//!   stand-ins (see `KittyScrollMargins`) and never run.

use std::ffi::CStr;
use std::fmt;
use std::io::Write as _;

use crate::terminal::ansi;
use crate::terminal::apc::glyph;
use crate::terminal::build_options;
use crate::terminal::charsets;
use crate::terminal::color;
use crate::terminal::csi;
use crate::terminal::cursor::Style as CursorStyle;
use crate::terminal::kitty;
use crate::terminal::kitty::graphics_image::Limits as LoadingImageLimits;
use crate::terminal::kitty::graphics_storage::ImageStorage;
use crate::terminal::modes::{Mode, ModePacked, ModeState};
use crate::terminal::mouse;
use crate::terminal::osc;
use crate::terminal::osc::semantic_prompt as osc_sp;
use crate::terminal::page::{
    self as pagepkg, cell_fields, field_mask, Cell, Mask, Page, Row, SemanticContent, SemanticPrompt as RowSemanticPrompt,
    Wide,
};
use crate::terminal::page_list::{CellSubset, CompressMode, Direction, IncreaseCapacityError, IncrementalCompressionResult, Node, NodePtr, Pin, PinPtr};
use crate::terminal::point::{self, Coordinate, Point};
use crate::terminal::screen::{
    self, CursorCopy, DumpStringError, InputClear, Resize as ScreenResize, SavedCursor, Screen, SemanticClick,
    SemanticContentUpdate,
};
use crate::terminal::screen_set::{Key as ScreenKey, ScreenPtr, ScreenSet};
use crate::terminal::sgr;
use crate::terminal::size::{AllocError, CellCountInt};
use crate::terminal::style::{self, Style};
use crate::terminal::tabstops::{Tabstops, TabstopsError};
use crate::terminal::xt_checksum;
use crate::tripwire;
use crate::unicode;
use crate::unicode::uucode::grapheme::BreakState;
use crate::unicode::uucode::types::GraphemeBreakNoControl;

const LOG_TARGET: &str = "terminal";

/// Default tabstop interval
const TABSTOP_INTERVAL: usize = 8;

// ---------------------------------------------------------------------------
// Raw pointer helpers (Rust-only)
// ---------------------------------------------------------------------------

/// Dereference a tracked pin pointer.
#[inline(always)]
fn tp<'a>(p: PinPtr) -> &'a mut Pin {
    // SAFETY: tracked pins are live while tracked by their PageList.
    unsafe { &mut *p.as_ptr() }
}

/// Dereference a node pointer.
#[inline(always)]
fn nd<'a>(node: NodePtr) -> &'a mut Node {
    // SAFETY: node pointers used here come from live pins of the active
    // screen's PageList.
    unsafe { &mut *node.as_ptr() }
}

/// Mutable access to a row through a raw pointer.
#[inline(always)]
fn rw<'a>(row: *mut Row) -> &'a mut Row {
    // SAFETY: row pointers come from live page memory.
    unsafe { &mut *row }
}

/// Mutable access to a cell through a raw pointer.
#[inline(always)]
fn cl<'a>(cell: *mut Cell) -> &'a mut Cell {
    // SAFETY: cell pointers come from live page memory.
    unsafe { &mut *cell }
}

/// A mutable slice of `len` cells starting at `ptr` (Zig: `ptr[0..len]`).
#[inline(always)]
fn cells_mut<'a>(ptr: *mut Cell, len: usize) -> &'a mut [Cell] {
    // SAFETY: callers only build slices within a single row of a live page.
    unsafe { std::slice::from_raw_parts_mut(ptr, len) }
}

/// The page the cursor of the given screen is on.
#[inline(always)]
fn cursor_page<'a>(s: ScreenPtr) -> &'a mut Page {
    nd(tp(s.cursor.page_pin).node).page()
}

/// `screen.clearCells(cursor page, cursor row, cells)`.
#[inline(always)]
fn clear_cursor_row_cells(mut s: ScreenPtr, cells: &mut [Cell]) {
    let page = cursor_page(s);
    let row = rw(s.cursor.page_row);
    s.clear_cells(page, row, cells);
}

/// `screen.clearUnprotectedCells(cursor page, cursor row, cells)`.
#[inline(always)]
fn clear_cursor_row_unprotected_cells(mut s: ScreenPtr, cells: &mut [Cell]) {
    let page = cursor_page(s);
    let row = rw(s.cursor.page_row);
    s.clear_unprotected_cells(page, row, cells);
}

// TODO(port-dedupe): replace with
// crate::terminal::kitty::graphics_storage::ImageStorage::placements.count()
// once placements are ported. The storage never holds placements yet.
#[inline(always)]
fn kitty_placement_count(_storage: &ImageStorage) -> usize {
    0
}

// TODO(port-dedupe): replace with
// crate::terminal::kitty::graphics_storage::ScrollOp once ported.
/// (Zig: `kitty.graphics.ImageStorage.ScrollOp`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KittyScrollOp {
    WindowShift,
    InPlace,
}

// TODO(port-dedupe): replace with
// crate::terminal::kitty::graphics_storage::ScrollMargins once ported.
/// Handle when Kitty graphics is disabled (or placements are not yet
/// ported): a scroll-margins state whose `end` does nothing.
struct KittyScrollMargins;

impl KittyScrollMargins {
    #[inline]
    fn end(&mut self) {}
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Set via the XTSHIFTESCAPE sequence. If true (XTSHIFTESCAPE = 1)
/// then we want to capture the shift key for the mouse protocol
/// if the configuration allows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum MouseShiftCapture {
    #[default]
    Null = 0,
    False = 1,
    True = 2,
}

/// These are just a packed set of flags we may set on the terminal.
/// (Zig: the anonymous `flags` packed struct.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags {
    // This supports a Kitty extension where programs using semantic
    // prompts (OSC133) can annotate their new prompts with `redraw=0` to
    // disable clearing the prompt on resize.
    pub shell_redraws_prompt: osc_sp::Redraw,

    // This is set via ESC[4;2m. Any other modify key mode just sets
    // this to false and we act in mode 1 by default.
    pub modify_other_keys_2: bool,

    /// The mouse event mode and format. These are set to the last
    /// set mode in modes. You can't get the right event/format to use
    /// based on modes alone because modes don't show you what order
    /// this was called so we have to track it separately.
    pub mouse_event: mouse::Event,
    pub mouse_format: mouse::Format,

    /// Set via the XTSHIFTESCAPE sequence. If true (XTSHIFTESCAPE = 1)
    /// then we want to capture the shift key for the mouse protocol
    /// if the configuration allows it.
    pub mouse_shift_capture: MouseShiftCapture,

    /// The checksum variant DECRQCRA computes, set via XTCHECKSUM.
    pub xt_checksum: xt_checksum::Flags,

    /// True if the window is focused.
    pub focused: bool,

    /// True if the terminal view may be visible. Unknown visibility is
    /// represented as visible so callers behave conservatively.
    pub visible: bool,

    /// Whether a resize may pull rows out of scrollback back into the
    /// active area. This should be false if the pty keeps its own screen
    /// buffer without scrollback (e.g. Windows ConPTY) so that we stay in
    /// sync with it. See PageList.Resize for details. This is configuration
    /// rather than terminal state so it is preserved across a full reset.
    pub resize_pull_scrollback: bool,

    /// True if the terminal is in a password entry mode. This is set
    /// to true based on termios state. This is set
    /// to true based on termios state.
    pub password_input: bool,

    /// True if the terminal should perform selection scrolling.
    pub selection_scroll: bool,

    /// Dirty flag used only by the search thread. The renderer is expected
    /// to set this to true if the viewport was dirty as it was rendering.
    /// This is used by the search thread to more efficiently re-search the
    /// viewport and active area.
    ///
    /// Since the renderer is going to inspect the viewport/active area ANYWAYS,
    /// this lets our search thread do less work and hold the lock less time,
    /// resulting in more throughput for everything.
    pub search_viewport_dirty: bool,

    /// Dirty flags for the renderer.
    pub dirty: Dirty,
}

impl Default for Flags {
    fn default() -> Self {
        Flags {
            shell_redraws_prompt: osc_sp::Redraw::True,
            modify_other_keys_2: false,
            mouse_event: mouse::Event::None,
            mouse_format: mouse::Format::X10,
            mouse_shift_capture: MouseShiftCapture::Null,
            xt_checksum: xt_checksum::Flags::default(),
            focused: true,
            visible: true,
            resize_pull_scrollback: true,
            password_input: false,
            selection_scroll: false,
            search_viewport_dirty: false,
            dirty: Dirty::default(),
        }
    }
}

/// The various color configurations a terminal maintains and that can
/// be set dynamically via OSC, with defaults usually coming from a
/// configuration.
#[derive(Debug, Clone)]
pub struct Colors {
    pub background: color::DynamicRgb,
    pub foreground: color::DynamicRgb,
    pub cursor: color::DynamicRgb,
    pub palette: color::DynamicPalette,
}

impl Colors {
    pub const DEFAULT: Colors = Colors {
        background: color::DynamicRgb::UNSET,
        foreground: color::DynamicRgb::UNSET,
        cursor: color::DynamicRgb::UNSET,
        palette: color::DynamicPalette::DEFAULT,
    };
}

impl Default for Colors {
    fn default() -> Self {
        Colors::DEFAULT
    }
}

/// This is a set of dirty flags the renderer can use to determine
/// what parts of the screen need to be redrawn. It is up to the renderer
/// to clear these flags.
///
/// This only contains dirty flags for terminal state, not for the screen
/// state. The screen state has its own dirty flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Dirty {
    /// Set when the color palette is modified in any way.
    pub palette: bool,

    /// Set when the reverse colors mode is modified.
    pub reverse_colors: bool,

    /// Screen clear of some kind. This can be due to a screen change,
    /// erase display, etc.
    pub clear: bool,

    /// Set when the pre-edit is modified.
    pub preedit: bool,

    /// Set when Glyph Protocol registrations may have changed. Registered
    /// glyphs can affect already-visible PUA cells, so this requires a full
    /// render-state rebuild.
    pub glyph_glossary: bool,
}

/// Scrolling region is the area of the screen designated where scrolling
/// occurs. When scrolling the screen, only this viewport is scrolled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollingRegion {
    // Top and bottom of the scroll region (0-indexed)
    // Precondition: top < bottom
    pub top: CellCountInt,
    pub bottom: CellCountInt,

    // Left/right scroll regions.
    // Precondition: right > left
    // Precondition: right <= cols - 1
    pub left: CellCountInt,
    pub right: CellCountInt,
}

/// Terminal-level cursor state shared by all screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Whether the current cursor appearance follows the configured defaults.
    pub is_default: bool,

    /// Configured style restored by DECSCUSR default, RIS and DECSTR.
    pub default_style: CursorStyle,

    /// Configured blink restored by DECSCUSR default, RIS and DECSTR.
    /// Null selects the terminal emulator default, which is blinking.
    pub default_blink: Option<bool>,
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor {
            is_default: true,
            default_style: CursorStyle::Block,
            default_blink: Some(false),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// The size of the terminal grid in cells. Both must be at least 1,
    /// otherwise `init` returns `error.InvalidValue`.
    pub cols: CellCountInt,
    pub rows: CellCountInt,

    /// The maximum size of scrollback in bytes. Null is unlimited and zero
    /// disables scrollback.
    pub max_scrollback_bytes: Option<usize>,

    /// The maximum number of physical scrollback rows, excluding the active
    /// area. Null is unlimited. The effective limit permits at least one
    /// standard page and only complete historical pages are pruned.
    pub max_scrollback_lines: Option<usize>,

    pub colors: Colors,

    /// The default mode state. When the terminal gets a reset, it
    /// will revert back to this state.
    pub default_modes: ModePacked,

    /// Cursor state restored by DECSCUSR default, RIS and DECSTR.
    pub default_cursor_style: CursorStyle,
    pub default_cursor_blink: Option<bool>,

    /// The checksum variant DECRQCRA computes after RIS or DECSTR.
    pub default_xt_checksum: xt_checksum::Flags,

    /// The total storage limit for Kitty images in bytes. Has no effect
    /// if kitty images are disabled at build-time.
    pub kitty_image_storage_limit: usize,

    /// The limits for what medium types are allowed for Kitty image loading.
    /// Has no effect if kitty images are disabled otherwise. For example,
    // if no `sys.decode_png` hook is specified, png formats are disabled
    // no matter what.
    pub kitty_image_loading_limits: LoadingImageLimits,
}

impl Options {
    /// Options for the given grid size with every other field defaulted.
    pub fn new(cols: CellCountInt, rows: CellCountInt) -> Options {
        Options {
            cols,
            rows,
            ..Default::default()
        }
    }
}

impl Default for Options {
    /// Note: `cols` and `rows` have no default in Zig; they are zero here
    /// and must be set (`init` rejects zero dimensions).
    fn default() -> Self {
        Options {
            cols: 0,
            rows: 0,
            max_scrollback_bytes: Some(10_000),
            max_scrollback_lines: None,
            colors: Colors::DEFAULT,
            default_modes: ModePacked::default(),
            default_cursor_style: CursorStyle::Block,
            default_cursor_blink: Some(false),
            default_xt_checksum: xt_checksum::Flags::default(),
            kitty_image_storage_limit: match build_options::ARTIFACT {
                build_options::Artifact::Ghostty => 320 * 1000 * 1000, // 320MB

                // libghostty we start with a much lower limit since this is an
                // embedded library and we want to be more conservative with memory
                // usage by default.
                build_options::Artifact::Lib => 10 * 1000 * 1000, // 10MB
            },
            kitty_image_loading_limits: LoadingImageLimits::DIRECT,
        }
    }
}

/// Errors from [`Terminal::init`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    OutOfMemory,
    InvalidValue,
}

impl From<AllocError> for InitError {
    fn from(_: AllocError) -> Self {
        InitError::OutOfMemory
    }
}

impl From<TabstopsError> for InitError {
    fn from(_: TabstopsError) -> Self {
        InitError::OutOfMemory
    }
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for InitError {}

/// Errors from [`Terminal::print_string`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintStringError {
    InvalidUtf8,
    OutOfMemory,
    OutOfSpace,
}

impl From<IncreaseCapacityError> for PrintStringError {
    fn from(e: IncreaseCapacityError) -> Self {
        match e {
            IncreaseCapacityError::OutOfMemory => PrintStringError::OutOfMemory,
            IncreaseCapacityError::OutOfSpace => PrintStringError::OutOfSpace,
        }
    }
}

impl From<AllocError> for PrintStringError {
    fn from(_: AllocError) -> Self {
        PrintStringError::OutOfMemory
    }
}

impl fmt::Display for PrintStringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for PrintStringError {}

/// The semantic prompt type. This is used when tracking a line type and
/// requires integration with the shell. By default, we mark a line as "none"
/// meaning we don't know what type it is.
///
/// See: https://gitlab.freedesktop.org/Per_Bothner/specifications/blob/master/proposals/semantic-prompts.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticPrompt {
    Prompt,
    PromptContinuation,
    Input,
    Command,
}

/// The tag of [`ScrollViewport`] (Zig: `ScrollViewport.Tag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum ScrollViewportTag {
    Top = 0,
    Bottom = 1,
    Delta = 2,
    Row = 3,
    DeltaPrompt = 4,
}

/// Options for scrolling the viewport of the terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollViewport {
    /// Scroll to the top of the scrollback
    Top,

    /// Scroll to the bottom, i.e. the top of the active area
    Bottom,

    /// Scroll by some delta amount, up is negative.
    Delta(isize),

    /// Scroll to the given absolute row offset from the top of the
    /// scrollable area. A value of zero is the top row. The requested
    /// row becomes the first visible row of the viewport, clamped so
    /// the viewport never scrolls beyond the top of the active area.
    /// This is the same row space as PageList.Scrollbar offset.
    Row(usize),

    /// Scroll by a number of prompts relative to the viewport top, up is negative.
    /// Zero or no matching prompt is a no-op. Moving forward through a final
    /// prompt continuation can snap to the active area.
    DeltaPrompt(isize),
}

/// C value union of [`ScrollViewport`] (Zig: `ScrollViewport.CValue`).
/// Padding: largest variant is isize (8 bytes on 64-bit).
/// Use [2]u64 (16 bytes) for future expansion.
#[repr(C)]
#[derive(Clone, Copy)]
pub union ScrollViewportCValue {
    pub delta: isize,
    pub row: usize,
    pub delta_prompt: isize,
    pub _padding: [u64; 2],
}

/// C representation of [`ScrollViewport`] (Zig: `ScrollViewport.C`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScrollViewportC {
    pub tag: ScrollViewportTag,
    pub value: ScrollViewportCValue,
}

impl ScrollViewport {
    pub fn tag(self) -> ScrollViewportTag {
        match self {
            ScrollViewport::Top => ScrollViewportTag::Top,
            ScrollViewport::Bottom => ScrollViewportTag::Bottom,
            ScrollViewport::Delta(_) => ScrollViewportTag::Delta,
            ScrollViewport::Row(_) => ScrollViewportTag::Row,
            ScrollViewport::DeltaPrompt(_) => ScrollViewportTag::DeltaPrompt,
        }
    }

    /// Convert to the C representation.
    pub fn cval(self) -> ScrollViewportC {
        let value = match self {
            ScrollViewport::Top | ScrollViewport::Bottom => ScrollViewportCValue { _padding: [0; 2] },
            ScrollViewport::Delta(v) => ScrollViewportCValue { delta: v },
            ScrollViewport::Row(v) => ScrollViewportCValue { row: v },
            ScrollViewport::DeltaPrompt(v) => ScrollViewportCValue { delta_prompt: v },
        };
        ScrollViewportC { tag: self.tag(), value }
    }
}

/// The amount of compression work performed by `compress` before returning.
///
/// The declaration order is part of the libghostty-vt C ABI. Removed values
/// must leave a `null` hole so later values retain their integer values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum CompressionMode {
    Incremental = 0,
    Full = 1,
}

/// The scheduling result of a `compress` call.
///
/// The declaration order is part of the libghostty-vt C ABI. Removed values
/// must leave a `null` hole so later values retain their integer values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum CompressionResult {
    Unsupported = 0,
    Pending = 1,
    Complete = 2,
}

/// The width class of a printSlice batch. Each batch contains only
/// codepoints of a single width class because they fill cells
/// differently: wide codepoints occupy a (wide, spacer_tail) cell
/// pair while narrow codepoints occupy a single cell.
///
/// (Zig: `PrintSliceWidth`; the port uses a `const WIDE: bool` generic.)
#[inline(always)]
const fn print_slice_cells_per_cp(wide: bool) -> usize {
    if wide {
        2
    } else {
        1
    }
}

/// Whether a codepoint above 0xFF is eligible for the batched print
/// fast path with the given width class.
#[inline(always)]
fn print_slice_eligible<const WIDE: bool>(cp: u32) -> bool {
    debug_assert!(cp > 0xFF);
    if build_options::KITTY_GRAPHICS && cp == kitty::graphics::unicode::PLACEHOLDER {
        return false;
    }

    unicode::TABLE.get(cp).width == if WIDE { 2 } else { 1 }
}

/// The bit position of the `content` field within the packed
/// Cell. A codepoint occupies the low bits of `content`, so
/// shifting a codepoint left by this amount places it exactly
/// where `.content = .{ .codepoint = .{ .data = cp } }` would.
const CP_SHIFT: u32 = cell_fields::CONTENT.offset;

/// Our fast path can only handle "simple" cells. A simple cell is
/// a codepoint cell (no grapheme data or bg-color tag), narrow, and
/// not a hyperlink. The mask covers every field that must match
/// the expected value (see printSliceCheckExpected) exactly.
const SIMPLE_MASK_BITS: u64 = field_mask(&[
    cell_fields::CONTENT_TAG,
    cell_fields::STYLE_ID,
    cell_fields::WIDE,
    cell_fields::HYPERLINK,
]);
type SimpleMask = Mask<Cell, SIMPLE_MASK_BITS, 4>;

/// Store narrow cells using a template with zeroed codepoint bits.
/// If a charset table is provided, all input codepoints must fit in a byte.
#[inline(always)]
fn print_slice_store_run(
    cells: *mut Cell,
    cps: &[u32],
    from: usize,
    to: usize,
    template_bits: u64,
    charset_table: Option<&[u16]>,
) {
    // Since codepoints are OR'd into the content field rather than
    // assigned, any nonzero content bits in the template would
    // corrupt the stored codepoints.
    debug_assert!(template_bits & cell_fields::CONTENT.mask() == 0);

    if let Some(table) = charset_table {
        for idx in from..to {
            // SAFETY: idx < cell_count, within the cursor row.
            unsafe {
                *cells.add(idx) = Cell::from_bits(template_bits | ((table[cps[idx] as usize] as u64) << CP_SHIFT));
            }
        }
        return;
    }

    for idx in from..to {
        // SAFETY: idx < cell_count, within the cursor row.
        unsafe {
            *cells.add(idx) = Cell::from_bits(template_bits | ((cps[idx] as u64) << CP_SHIFT));
        }
    }
}

/// The expected value of a simple cell (per SimpleMask in
/// printSliceFill) that already has the given style (so no
/// ref-counting is needed).
#[inline(always)]
fn print_slice_check_expected(style_id: style::Id) -> u64 {
    let mut e = Cell::from_bits(0);
    e.set_style_id(style_id);
    e.to_bits()
}

/// The modes for DECCOLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DeccolmMode {
    Cols80 = 0,
    Cols132 = 1,
}

/// Pixel geometry of a single cell (Zig: `Resize.cell_size_px`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSizePx {
    pub width: u32,
    pub height: u32,
}

/// A terminal resize expressed in cells with optional per-cell pixel
/// geometry. It is highly recommended that callers supply cell geometry
/// but a terminal can technically function without it (but some reports
/// like certain mouse reporting modes and Kitty image protocol will
/// not be functional).
///
/// Cell pixel dimensions must already be scaled for the current display DPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resize {
    pub cols: CellCountInt,
    pub rows: CellCountInt,
    pub cell_size_px: Option<CellSizePx>,
}

impl Resize {
    pub fn new(cols: CellCountInt, rows: CellCountInt) -> Resize {
        Resize {
            cols,
            rows,
            cell_size_px: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeError {
    /// Resize requires allocation
    OutOfMemory,

    /// Input value was invalid, such as a 0-sized dimension.
    InvalidValue,
}

impl From<AllocError> for ResizeError {
    fn from(_: AllocError) -> Self {
        ResizeError::OutOfMemory
    }
}

impl From<TabstopsError> for ResizeError {
    fn from(_: TabstopsError) -> Self {
        ResizeError::OutOfMemory
    }
}

impl fmt::Display for ResizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ResizeError {}

/// Tripwire failure points for `resize` (Zig: `resize_tw`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResizeFailPoint {
    Tabstops,
    PrimaryScreen,
    AlternateScreen,
    AlternateScreenInit,
}

pub static RESIZE_TW: tripwire::Module<ResizeFailPoint, ResizeError> = tripwire::Module::new();

/// Modal screen changes. These map to the literal terminal
/// modes to enable or disable alternate screen modes. They each
/// have subtle behaviors so we define them as an enum here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SwitchScreenMode {
    /// Legacy alternate screen mode. This goes to the alternate
    /// screen or primary screen and only copies the cursor. The
    /// screen is not erased.
    Mode47,

    /// Alternate screen mode where the alternate screen is cleared
    /// on exit. The primary screen is never cleared. The cursor is
    /// copied.
    Mode1047,

    /// Save primary screen cursor, switch to alternate screen,
    /// and clear the alternate screen on entry. On exit,
    /// do not clear the screen, and restore the cursor on the
    /// primary screen.
    Mode1049,
}

impl SwitchScreenMode {
    /// The Zig tag name.
    pub fn as_str(self) -> &'static str {
        match self {
            SwitchScreenMode::Mode47 => "47",
            SwitchScreenMode::Mode1047 => "1047",
            SwitchScreenMode::Mode1049 => "1049",
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

/// See the module documentation.
pub struct Terminal {
    /// The set of screens behind this terminal (e.g. primary vs alternate).
    pub screens: ScreenSet,

    /// Whether we're currently writing to the status line (DECSASD and DECSSDT).
    /// We don't support a status line currently so we just black hole this
    /// data so that it doesn't mess up our main display.
    pub status_display: ansi::StatusDisplay,

    /// Where the tabstops are.
    pub tabstops: Tabstops,

    /// The size of the terminal.
    pub rows: CellCountInt,
    pub cols: CellCountInt,

    /// The size of the screen in pixels. This is used for pty events and images
    pub width_px: u32,
    pub height_px: u32,

    /// The current scrolling region.
    pub scrolling_region: ScrollingRegion,

    /// The last reported pwd, if any. Stored with a trailing NUL sentinel.
    pub pwd: Vec<u8>,

    /// The title of the terminal as set by escape sequences (e.g. OSC 0/2).
    /// Stored with a trailing NUL sentinel.
    pub title: Vec<u8>,

    /// The color state for this terminal.
    pub colors: Colors,

    /// The previous printed character. This is used for the repeat previous
    /// char CSI (ESC [ <n> b).
    pub previous_char: Option<u32>,

    /// The modes that this terminal currently has active.
    pub modes: ModeState,

    /// Terminal-level cursor state.
    pub cursor: Cursor,

    /// The checksum variant DECRQCRA computes after RIS or DECSTR. The current
    /// variant is in `flags.xt_checksum`.
    pub default_xt_checksum: xt_checksum::Flags,

    /// The most recently set mouse shape for the terminal.
    pub mouse_shape: mouse::Shape,

    /// Per-session Glyph Protocol registrations.
    pub glyph_glossary: glyph::Glossary,

    /// Kitty drag and drop protocol (OSC 72) state. Allocated when a client
    /// registers to accept drops (t=a) and freed when it unregisters (t=A),
    /// so a terminal that never runs a drag and drop aware program pays
    /// nothing for it. Non-null means a client currently accepts drops.
    pub kitty_dnd: Option<Box<kitty::dnd::State>>,

    /// These are just a packed set of flags we may set on the terminal.
    pub flags: Flags,
}

impl fmt::Debug for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Terminal")
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("screens", &self.screens)
            .field("scrolling_region", &self.scrolling_region)
            .field("flags", &self.flags)
            .finish_non_exhaustive()
    }
}

impl Terminal {
    /// Initialize a new terminal with the given options.
    ///
    /// The returned terminal owns its memory.
    ///
    /// Returns `error.InvalidValue` if `cols` or `rows` is zero. This check
    /// happens before anything is allocated. If the size comes from an
    /// untrusted source, you can rely on this check instead of validating
    /// it yourself.
    pub fn init(opts: Options) -> Result<Terminal, InitError> {
        let cols = opts.cols;
        let rows = opts.rows;

        // The screens and scrolling region need at least one row and column.
        // Check this before allocating so there is nothing to clean up.
        if cols == 0 || rows == 0 {
            return Err(InitError::InvalidValue);
        }

        let screen_set = ScreenSet::init(screen::Options {
            cols,
            rows,
            max_scrollback_bytes: opts.max_scrollback_bytes,
            max_scrollback_lines: opts.max_scrollback_lines,
            kitty_image_storage_limit: opts.kitty_image_storage_limit,
            kitty_image_loading_limits: opts.kitty_image_loading_limits.clone(),
        })?;

        let mut result = Terminal {
            cols,
            rows,
            screens: screen_set,
            status_display: ansi::StatusDisplay::Main,
            tabstops: Tabstops::init(cols as usize, TABSTOP_INTERVAL)?,
            width_px: 0,
            height_px: 0,
            scrolling_region: ScrollingRegion {
                top: 0,
                bottom: rows - 1,
                left: 0,
                right: cols - 1,
            },
            pwd: Vec::new(),
            title: Vec::new(),
            colors: opts.colors,
            previous_char: None,
            modes: ModeState {
                values: opts.default_modes,
                default: opts.default_modes,
                ..Default::default()
            },
            cursor: Cursor {
                default_style: opts.default_cursor_style,
                default_blink: opts.default_cursor_blink,
                ..Default::default()
            },
            default_xt_checksum: opts.default_xt_checksum,
            mouse_shape: mouse::Shape::Text,
            glyph_glossary: glyph::Glossary::empty(),
            kitty_dnd: None,
            flags: Flags::default(),
        };
        result.flags.xt_checksum = opts.default_xt_checksum;
        result.set_cursor_style(ansi::CursorStyle::Default);
        Ok(result)
    }

    /// Returns the current color for an xterm OSC color target.
    ///
    /// Unsupported dynamic and special colors return null. The cursor color
    /// follows xterm-style reporting and falls back to the foreground color when
    /// no explicit cursor color is set.
    pub fn color_for_xterm(&self, target: osc::color::Target) -> Option<color::Rgb> {
        use crate::terminal::color::Dynamic;
        match target {
            osc::color::Target::Palette(i) => Some(self.colors.palette.current[i as usize]),
            osc::color::Target::Dynamic(dynamic) => match dynamic {
                Dynamic::Foreground => self.colors.foreground.get(),
                Dynamic::Background => self.colors.background.get(),
                Dynamic::Cursor => self.colors.cursor.get().or(self.colors.foreground.get()),
                Dynamic::PointerForeground
                | Dynamic::PointerBackground
                | Dynamic::TektronixForeground
                | Dynamic::TektronixBackground
                | Dynamic::HighlightBackground
                | Dynamic::TektronixCursor
                | Dynamic::HighlightForeground => None,
            },
            osc::color::Target::Special(_) => None,
        }
    }

    /// Returns the current color for a Kitty color protocol key.
    ///
    /// Only palette, foreground, background, and cursor colors are backed by
    /// Terminal state. Unsupported keys, or supported dynamic colors without a
    /// value, return null.
    pub fn color_for_kitty(&self, key: kitty::color::Kind) -> Option<color::Rgb> {
        use crate::terminal::kitty::color::{Kind, Special};
        match key {
            Kind::Palette(palette) => Some(self.colors.palette.current[palette as usize]),
            Kind::Special(special) => match special {
                Special::Foreground => self.colors.foreground.get(),
                Special::Background => self.colors.background.get(),
                Special::Cursor => self.colors.cursor.get(),
                _ => None,
            },
        }
    }

    /// Set the checksum variant restored by RIS and DECSTR. Like
    /// `ModeState.setDefault`, this also changes the current variant.
    pub fn set_default_xt_checksum(&mut self, flags: xt_checksum::Flags) {
        self.default_xt_checksum = flags;
        self.flags.xt_checksum = flags;
    }

    /// Compute the DECRQCRA checksum of a rectangle of the active area,
    /// using the variant selected by XTCHECKSUM.
    pub fn rect_xt_checksum(&self, req: xt_checksum::Request) -> u16 {
        let origin: Option<ScrollingRegion> = if self.modes.get(Mode::Origin) {
            Some(self.scrolling_region)
        } else {
            None
        };
        let screen: &Screen = &self.screens.active;
        xt_checksum::compute(screen, req.selection(&screen.pages, origin), self.flags.xt_checksum)
    }

    /// Change the cursor's current shape and blink behavior.
    ///
    /// The terminal parser uses this for DECSCUSR (`CSI Ps SP q`), but the behavior
    /// is general: `.default` selects the configured defaults, while any other
    /// value selects a concrete appearance until it is changed again or reset.
    pub fn set_cursor_style(&mut self, value: ansi::CursorStyle) {
        use crate::terminal::ansi::CursorStyle as C;

        // Remember whether future configuration changes should update the visible
        // cursor. An explicit appearance must remain in effect until the program
        // selects the default again.
        self.cursor.is_default = value == C::Default;

        // Convert the request into the concrete values used by the renderer and
        // terminal mode state. A null default blink means the emulator default.
        self.modes.set(
            Mode::CursorBlinking,
            match value {
                C::Default => self.cursor.default_blink.unwrap_or(true),
                C::SteadyBlock | C::SteadyBar | C::SteadyUnderline => false,
                C::BlinkingBlock | C::BlinkingBar | C::BlinkingUnderline => true,
            },
        );
        self.screens.active.cursor.cursor_style = match value {
            C::Default => self.cursor.default_style,
            C::BlinkingBlock | C::SteadyBlock => CursorStyle::Block,
            C::BlinkingBar | C::SteadyBar => CursorStyle::Bar,
            C::BlinkingUnderline | C::SteadyUnderline => CursorStyle::Underline,
        };
    }

    /// Change the default cursor shape.
    ///
    /// If the cursor currently follows its defaults, the visible shape changes
    /// immediately. Otherwise the new shape is saved for the next reset or default
    /// selection, such as DECSCUSR `CSI 0 SP q`.
    pub fn set_default_cursor_style(&mut self, configured_style: CursorStyle) {
        // Always retain the new default, even while an explicit appearance is
        // active, so a later reset or default request can restore it.
        self.cursor.default_style = configured_style;

        // Do not overwrite an appearance explicitly selected by the program.
        if self.cursor.is_default {
            self.set_cursor_style(ansi::CursorStyle::Default);
        }
    }

    /// Change the default cursor blink behavior.
    ///
    /// Null selects the terminal emulator default (blinking). Like the default
    /// shape, this is applied immediately only when the cursor currently follows
    /// its defaults; otherwise it is saved for the next reset or default selection.
    pub fn set_default_cursor_blink(&mut self, blink: Option<bool>) {
        // Keep the configured value separate from the currently resolved mode so
        // null can continue to mean "use the emulator default."
        self.cursor.default_blink = blink;

        // Do not overwrite blink behavior explicitly selected by the program.
        if self.cursor.is_default {
            self.set_cursor_style(ansi::CursorStyle::Default);
        }
    }

    /// Change the primary screen's maximum scrollback allocation in bytes.
    ///
    /// Null removes the byte limit and zero disables scrollback. Disabling
    /// scrollback also immediately erases retained history and changes future
    /// scrolling to use the no-scrollback path. The alternate screen is
    /// intentionally unaffected because it never retains scrollback.
    pub fn set_scrollback_max_bytes(&mut self, max: Option<usize>) {
        let mut primary = self.screens.get(ScreenKey::Primary).unwrap();
        primary.pages.set_max_bytes(max);
        primary.no_scrollback = max == Some(0);

        // Lowering the limit can drop old history, along with any
        // placements attached to it.
        if build_options::KITTY_GRAPHICS {
            primary.kitty_images.mark_dirty();
        }

        if primary.no_scrollback {
            primary.erase_history(None);
        }
    }

    /// Change the primary screen's maximum number of physical scrollback lines.
    ///
    /// Null removes the line limit. The alternate screen is intentionally
    /// unaffected because it never retains scrollback.
    pub fn set_scrollback_max_lines(&mut self, max: Option<usize>) {
        let mut primary = self.screens.get(ScreenKey::Primary).unwrap();
        primary.pages.set_max_lines(max);

        // Lowering the limit can drop old history, along with any
        // placements attached to it.
        if build_options::KITTY_GRAPHICS {
            primary.kitty_images.mark_dirty();
        }
    }

    /// Print UTF-8 encoded string to the terminal.
    pub fn print_string<S: AsRef<[u8]> + ?Sized>(&mut self, str: &S) -> Result<(), PrintStringError> {
        let view = std::str::from_utf8(str.as_ref()).map_err(|_| PrintStringError::InvalidUtf8)?;
        for cp in view.chars() {
            match cp {
                '\n' => {
                    self.carriage_return();
                    self.linefeed()?;
                }

                _ => self.print(cp as u32)?,
            }
        }
        Ok(())
    }

    /// Print the previous printed character a repeated amount of times.
    pub fn print_repeat(&mut self, count_req: usize) -> Result<(), IncreaseCapacityError> {
        let Some(c) = self.previous_char else {
            return Ok(());
        };
        let mut remaining = count_req.max(1);

        // Print the repeated codepoint in slices so that eligible runs
        // take the batched printSlice fast path. printSlice is semantically
        // identical to calling print per codepoint: ineligible characters
        // or terminal states (insert mode, grapheme clustering, hyperlinks,
        // etc.) fall back to the per-codepoint print() path internally.
        let mut buf = [0u32; 4096];
        let len = remaining.min(buf.len());
        for cp in &mut buf[..len] {
            *cp = c;
        }
        while remaining > 0 {
            let n = remaining.min(buf.len());
            self.print_slice(&buf[..n])?;
            remaining -= n;
        }
        Ok(())
    }

    /// Print multiple codepoints to the terminal at once. This is
    /// semantically identical to calling `print` for each codepoint in
    /// order, but is much faster because it can batch cell writes and
    /// hoist per-codepoint checks out of the hot loop.
    ///
    /// The codepoints must all be printable: it is illegal for any
    /// codepoint in this slice to be a C0 control character. Therefore,
    /// this should only be called as a result of a proper VT parser
    /// (like our own).
    ///
    /// This is optimized for the common case: ASCII, soft-wrap, etc.
    /// Sequences of codepoints that require special handling (e.g. wide characters,
    /// grapheme clustering) are handled correctly but fall back to the
    /// slower per-codepoint path. They're less common and this is optimized
    /// for the aforementioned cases.
    pub fn print_slice(&mut self, cps: &[u32]) -> Result<(), IncreaseCapacityError> {
        // Check if we can do the fast path up front. If we can't
        // we need to go back to scalar `print`.
        let fast = 'fast: {
            // Only the main display is supported.
            if self.status_display != ansi::StatusDisplay::Main {
                break 'fast false;
            }

            // Modes that require per-codepoint handling in print().
            // Wraparound is required (its the default) so that our
            // row-fill logic below can assume soft-wrap semantics. Insert
            // mode shifts cells per print.
            if self.modes.get(Mode::Insert) {
                break 'fast false;
            }
            if !self.modes.get(Mode::Wraparound) {
                break 'fast false;
            }

            // Single shifts require per-codepoint charset handling.
            let screen = self.screens.active;
            if screen.charset.single_shift.is_some() {
                break 'fast false;
            }

            // Hyperlinks require per-cell map bookkeeping.
            if screen.cursor.hyperlink_id != 0 {
                break 'fast false;
            }

            true
        };
        if !fast {
            for &cp in cps {
                self.print(cp)?;
            }
            return Ok(());
        }

        let grapheme_cluster = self.modes.get(Mode::GraphemeCluster);

        // When grapheme clustering is enabled and a left margin is set,
        // print() consults the cell left of the margin after wrapping,
        // which we can't reason about here. Restrict the fast path to
        // the [0x10, 0xFF] range in that case (those never cluster).
        let charset = self.screens.active.charset;
        let allow_unicode = match charset.charsets.get(charset.gl) {
            charsets::Charset::Utf8 | charsets::Charset::Ascii => {
                !grapheme_cluster || self.scrolling_region.left == 0
            }
            // print() handles Unicode width and clustering before charset mapping.
            _ => false,
        };

        let mut i: usize = 0;
        while i < cps.len() {
            // Try the fast-path print first. This will return the number of
            // codepoints it consumed.
            let consumed = self.print_slice_fast(&cps[i..], grapheme_cluster, allow_unicode)?;
            if consumed > 0 {
                i += consumed;
                continue;
            }

            // Consuming zero bytes means that the fast path can't handle
            // the next codepoint or the terminal is in a state we can't
            // fast-path. Fall back to the slow cp-by-cp print then try
            // fast paths again.
            self.print(cps[i])?;
            i += 1;
        }
        Ok(())
    }

    /// Attempt to print a prefix of `cps` using a batched fast path that
    /// writes cells directly. Returns the number of codepoints consumed.
    /// A return value of zero means the caller must print the first
    /// codepoint via the normal `print` path.
    ///
    /// The fast path handles runs of narrow (width 1) and wide (width 2)
    /// codepoints being written to simple cells. Everything else (zero
    /// width codepoints, grapheme cluster continuations, complex cells,
    /// etc.) is rejected so `print` can handle it with full generality.
    fn print_slice_fast(
        &mut self,
        cps: &[u32],
        grapheme_cluster: bool,
        allow_unicode: bool,
    ) -> Result<usize, IncreaseCapacityError> {
        let screen = self.screens.active;

        // Codepoints in [0x10, 0xFF] are always narrow (width 1, matching
        // the c <= 0xFF fast path in print) and can never interact with
        // grapheme clustering (which requires a codepoint > 0xFF).
        //
        // Codepoints above 0xFF are batchable if their width is 1 or 2
        // (excluding zero-width characters such as combining marks, ZWJ,
        // and variation selectors) and, when grapheme clustering (mode
        // 2027) is enabled, if they are a grapheme break from the
        // previously printed codepoint (so print would never attach them
        // to the previous cell).

        // Codepoints in [0x10, 0xFF] are always narrow: print()
        // hardcodes width 1 for c <= 0xFF (no width table lookup).
        // They also can never interact with grapheme clustering,
        // which print() only performs for c > 0xFF, so they're
        // immediately eligible for the narrow fill with no further
        // checks.
        let cp0 = cps[0];
        if cp0 <= 0xFF {
            // C0 control characters (0x00-0x0F) aren't printable. The
            // stream never sends these (they're routed to execute), but
            // printSlice is a public API so defer to print() for safety.
            if cp0 < 0x10 {
                return Ok(0);
            }
            return self.print_slice_fill::<false>(cps, grapheme_cluster, allow_unicode);
        }

        if !allow_unicode {
            return Ok(0);
        }
        if build_options::KITTY_GRAPHICS {
            // The Kitty graphics placeholder requires row bookkeeping.
            if cp0 == kitty::graphics::unicode::PLACEHOLDER {
                return Ok(0);
            }
        }

        // The first codepoint requires care when grapheme clustering is
        // enabled: print() may attach it to the previous *cell* instead
        // of writing a new one. Take the first codepoint only when we can
        // determine — computing exactly what print() would — that it
        // starts a new cluster. At column zero with no pending wrap,
        // print() skips clustering entirely. Otherwise resolve the
        // previous cell the way print() does and check for a break.
        //
        // Note the pending-wrap rejection: print() may attach to the
        // pending cell *instead of wrapping*, which we can't model here.
        if grapheme_cluster && screen.cursor.x != 0 {
            'gate: {
                if screen.cursor.pending_wrap {
                    return Ok(0);
                }

                // Resolve the content cell to our left exactly like print():
                // if the immediate left cell is a wide spacer tail, the
                // content lives one further left. (A spacer tail can never
                // be at column zero — its wide half would have to be in the
                // previous row — so the second cursorCellLeft is in bounds.)
                let immediate = screen.cursor_cell_left(1);
                let prev = match cl(immediate).wide() {
                    Wide::SpacerTail => cl(screen.cursor_cell_left(2)),
                    _ => cl(immediate),
                };

                // An empty previous cell is necessarily a grapheme break.
                if prev.codepoint() == 0 {
                    break 'gate;
                }

                // Grapheme data on the previous cell requires the full
                // cluster state machine replay; only print() can do that.
                if prev.has_grapheme() {
                    return Ok(0);
                }

                // A simple single-codepoint previous cell: print() would run
                // exactly this break check from the default state.
                let mut state = BreakState::default();
                if !unicode::grapheme_break(prev.content_codepoint(), cp0, &mut state) {
                    return Ok(0);
                }
            }
        } else if grapheme_cluster && screen.cursor.pending_wrap {
            return Ok(0);
        }

        // The width lookup is a runtime value while printSliceFill is
        // specialized at comptime by width class, so this switch selects
        // between the two instantiations rather than passing the width
        // through as an argument.
        match unicode::TABLE.get(cp0).width {
            1 => self.print_slice_fill::<false>(cps, grapheme_cluster, allow_unicode),
            2 => self.print_slice_fill::<true>(cps, grapheme_cluster, allow_unicode),
            _ => Ok(0),
        }
    }

    /// The row-filling portion of the printSlice fast path, specialized by
    /// width class. The first codepoint must already be validated by the
    /// caller (printSliceFast).
    fn print_slice_fill<const WIDE: bool>(
        &mut self,
        cps: &[u32],
        grapheme_cluster: bool,
        allow_unicode: bool,
    ) -> Result<usize, IncreaseCapacityError> {
        let r = self.print_slice_fill_::<WIDE>(cps, grapheme_cluster, allow_unicode);

        // After doing any printing, wrapping, scrolling, etc. we want to
        // ensure that our screen remains in a consistent state.
        self.screens.active.assert_integrity();
        r
    }

    fn print_slice_fill_<const WIDE: bool>(
        &mut self,
        cps: &[u32],
        grapheme_cluster: bool,
        allow_unicode: bool,
    ) -> Result<usize, IncreaseCapacityError> {
        let mut screen = self.screens.active;
        let charset_table: Option<&'static [u16]> = match screen.charset.charsets.get(screen.charset.gl) {
            charsets::Charset::Utf8 | charsets::Charset::Ascii => None,
            set => Some(charsets::table(set)),
        };
        debug_assert!(charset_table.is_none() || !allow_unicode);

        // Determine the run of codepoints in the same width class that we
        // can batch. For codepoints after the first, the previous codepoint
        // in the run is always written as a fresh, single-codepoint cell,
        // so the grapheme break check against it is exact.
        let run_len: usize = 'run: {
            let mut idx: usize = 1;
            while idx < cps.len() {
                let cp = cps[idx];
                if !WIDE && (0x10..=0xFF).contains(&cp) {
                    idx += 1;
                    continue;
                }
                if cp > 0xFF && allow_unicode && print_slice_eligible::<WIDE>(cp) {
                    if !grapheme_cluster {
                        idx += 1;
                        continue;
                    }
                    let mut state = BreakState::default();
                    if unicode::grapheme_break(cps[idx - 1], cp, &mut state) {
                        idx += 1;
                        continue;
                    }
                }
                break 'run idx;
            }
            cps.len()
        };
        debug_assert!(run_len > 0);

        // The number of cells each codepoint occupies.
        let cells_per_cp: usize = print_slice_cells_per_cp(WIDE);
        let group_len = SimpleMask::GROUP_LEN;

        let mut printed: usize = 0;
        'outer: while printed < run_len {
            // If we're soft-wrapping, handle that first so that our cursor
            // is in the row/column that will receive the next codepoint.
            if screen.cursor.pending_wrap {
                self.print_wrap()?;
            }

            // Our right margin depends on where our cursor is now,
            // matching the logic in print().
            let right_limit: usize = if screen.cursor.x > self.scrolling_region.right {
                self.cols as usize
            } else {
                self.scrolling_region.right as usize + 1
            };

            // A degenerate 1-wide region can't hold a wide char; print()
            // has special handling so fall back to it.
            if WIDE && right_limit - self.scrolling_region.left as usize <= 1 {
                break;
            }

            let avail: usize = right_limit - screen.cursor.x as usize;
            debug_assert!(avail > 0);

            // The cursor caches live row and cell pointers into this mapping, so
            // its page cannot be compressed while this print path is active.
            let page = nd(tp(screen.cursor.page_pin).node).page_assume_resident();
            let cells: *mut Cell = screen.cursor.page_cell;
            let row = rw(screen.cursor.page_row);
            let style_id = screen.cursor.style_id;
            let template: Cell = {
                let mut t = Cell::init(0);
                t.set_style_id(style_id);
                t.set_wide(Wide::Narrow);
                t.set_protected(screen.cursor.protected);
                t.set_semantic_content(screen.cursor.semantic_content);
                t
            };
            let template_bits: u64 = template.to_bits();
            let check_expected: u64 = print_slice_check_expected(style_id);

            if WIDE && avail == 1 {
                // Only one cell left in the row: print() writes a
                // spacer head (or a blank narrow cell if we're inside
                // a right margin) and wraps. We require a simple cell,
                // otherwise fall back to print() for the cleanup.
                if !SimpleMask::eql_scalar(*cl(cells), check_expected) {
                    break;
                }

                let mut spacer = template;
                if right_limit == self.cols as usize {
                    row.set_wrap(true);
                    spacer.set_wide(Wide::SpacerHead);
                }
                row.set_dirty(true);
                if style_id != style::DEFAULT_ID {
                    row.set_styled(true);
                }
                *cl(cells) = spacer;
                self.print_wrap()?;
                continue 'outer;
            }

            // Number of codepoints and cells we're writing to this row.
            let count = (avail / cells_per_cp).min(run_len - printed);
            debug_assert!(count > 0);
            let cell_count = count * cells_per_cp;

            // Wide cells always come in (wide, spacer_tail) pairs.
            let spacer_bits: u64 = {
                let mut spacer = template;
                spacer.set_wide(Wide::SpacerTail);
                spacer.to_bits()
            };
            let wide_bits: u64 = {
                let mut w = template;
                w.set_wide(Wide::Wide);
                w.to_bits()
            };

            // A read-only view of the cells being written (for the masked
            // group compares). Recreated for each use so it never overlaps
            // a write.
            let view = || -> &[Cell] {
                // SAFETY: cell_count cells starting at the cursor fit in the row.
                unsafe { std::slice::from_raw_parts(cells, cell_count) }
            };
            let run_cps = &cps[printed..];

            let mut k: usize = 0; // cells written
            'fill: while k < cell_count {
                // Find the run of simple cells so the store loop below is
                // branch-free (and vectorizable). This is an early-exit
                // search loop, and reused rows typically match the whole
                // way through, so scan several cells at a time.
                let mut simple = k;
                'simple: {
                    while simple + group_len <= cell_count {
                        let p = SimpleMask::eql_prefix(view(), simple, check_expected);
                        simple += p;
                        if p != group_len {
                            break 'simple;
                        }
                    }
                    while simple < cell_count {
                        if !SimpleMask::eql_scalar(*cl(cells.wrapping_add(simple)), check_expected) {
                            break;
                        }
                        simple += 1;
                    }
                }

                if WIDE {
                    // We can only write whole (wide, spacer) pairs.
                    let pair_end = k + (simple - k) / 2 * 2;
                    let mut idx = k;
                    while idx < pair_end {
                        // SAFETY: idx + 1 < cell_count.
                        unsafe {
                            *cells.add(idx) = Cell::from_bits(wide_bits | ((run_cps[idx / 2] as u64) << CP_SHIFT));
                            *cells.add(idx + 1) = Cell::from_bits(spacer_bits);
                        }
                        idx += 2;
                    }

                    // If the simple run ended mid-pair we stop at the pair
                    // boundary and handle the offending cell below.
                    k = pair_end;
                } else {
                    print_slice_store_run(cells, run_cps, k, simple, template_bits, charset_table);
                    k = simple;
                }
                if k >= cell_count {
                    break;
                }

                // Bulk path for runs of cells that differ from the
                // expected simple cell only by their style: this is the
                // common case when styled text overwrites previously
                // styled (or default-styled) rows, e.g. TUI redraws.
                // These runs are handled wholesale: one scan to find the
                // run of identical old styles, two ref-count updates,
                // and a branch-free fill.
                if !WIDE {
                    'bulk: {
                        let first = SimpleMask::pattern(*cl(cells.wrapping_add(k)));

                        // The old cell must be a plain narrow codepoint cell
                        // with no hyperlink whose only difference is the
                        // style id (see printSliceCheckExpected: every other
                        // masked field must be zero).
                        let style_shift = cell_fields::STYLE_ID.offset;
                        let old_style: style::Id = (first >> style_shift) as style::Id;
                        if first != print_slice_check_expected(old_style) {
                            break 'bulk;
                        }
                        debug_assert!(old_style != style_id); // it failed the simple check

                        // Find the run of cells with identical masked bits.
                        let mut m = k + 1;
                        'scan: {
                            while m + group_len <= cell_count {
                                let p = SimpleMask::eql_prefix(view(), m, first);
                                m += p;
                                if p != group_len {
                                    break 'scan;
                                }
                            }
                            while m < cell_count {
                                if !SimpleMask::eql_scalar(*cl(cells.wrapping_add(m)), first) {
                                    break;
                                }
                                m += 1;
                            }
                        }

                        // Fix up the style ref counts for the whole run at
                        // once. Each of the old cells held a reference to
                        // old_style so the release is safe by construction.
                        let n = m - k;
                        if old_style != style::DEFAULT_ID {
                            // SAFETY: the style is in this page's set.
                            unsafe { page.styles.release_multiple(page.memory, old_style, n as _) };
                        }
                        if style_id != style::DEFAULT_ID {
                            // SAFETY: the style is in this page's set.
                            unsafe { page.styles.use_multiple(page.memory, style_id, n as _) };
                        }

                        print_slice_store_run(cells, run_cps, k, m, template_bits, charset_table);
                        k = m;
                        continue 'fill;
                    }
                }

                // General path for cells that failed the masked check:
                // style-only mismatches are handled inline; anything that
                // needs cleanup (wide chars and their spacers, grapheme
                // data, hyperlinks) falls back to print().
                let general_count: usize = cells_per_cp;
                for offset in 0..general_count {
                    let cell = cl(cells.wrapping_add(k + offset));
                    if cell.wide() != Wide::Narrow || cell.has_grapheme() || cell.hyperlink() {
                        break 'fill;
                    }
                }
                for offset in 0..general_count {
                    let cell = cl(cells.wrapping_add(k + offset));
                    if cell.style_id() != style_id {
                        if cell.style_id() != style::DEFAULT_ID {
                            // SAFETY: the style is in this page's set.
                            unsafe { page.styles.release(page.memory, cell.style_id()) };
                        }
                        if style_id != style::DEFAULT_ID {
                            // SAFETY: the style is in this page's set.
                            unsafe { page.styles.use_(page.memory, style_id) };
                        }
                    }
                }
                if WIDE {
                    *cl(cells.wrapping_add(k)) = Cell::from_bits(wide_bits | ((run_cps[k / 2] as u64) << CP_SHIFT));
                    *cl(cells.wrapping_add(k + 1)) = Cell::from_bits(spacer_bits);
                } else {
                    let cp = match charset_table {
                        Some(table) => table[run_cps[k] as usize] as u32,
                        None => run_cps[k],
                    };
                    *cl(cells.wrapping_add(k)) = Cell::from_bits(template_bits | ((cp as u64) << CP_SHIFT));
                }
                k += cells_per_cp;
            }

            if k > 0 {
                debug_assert!(k % cells_per_cp == 0);
                row.set_dirty(true);
                if style_id != style::DEFAULT_ID {
                    row.set_styled(true);
                }
                self.previous_char = Some(cps[printed + k / cells_per_cp - 1]);
                printed += k / cells_per_cp;

                // Advance the cursor. If we filled through the right limit
                // then the cursor stays on the last cell with the pending
                // wrap flag set, matching print().
                if screen.cursor.x as usize + k >= right_limit {
                    debug_assert!(screen.cursor.x as usize + k == right_limit);
                    screen.cursor_right((k - 1) as CellCountInt);
                    screen.cursor.pending_wrap = true;
                } else {
                    screen.cursor_right(k as CellCountInt);
                }
            }

            // We hit a cell that requires the slow path. The cursor is
            // exactly at that cell so return and let the caller print the
            // next codepoint via print().
            if k < cell_count {
                break;
            }
        }

        Ok(printed)
    }

    pub fn print(&mut self, c: u32) -> Result<(), IncreaseCapacityError> {
        // log.debug("print={x} y={} x={}", .{ c, self.screens.active.cursor.y, self.screens.active.cursor.x });

        // If we're not on the main display, do nothing for now
        if self.status_display != ansi::StatusDisplay::Main {
            return Ok(());
        }

        let r = self.print_(c);

        // After doing any printing, wrapping, scrolling, etc. we want to ensure
        // that our screen remains in a consistent state.
        self.screens.active.assert_integrity();
        r
    }

    fn print_(&mut self, c: u32) -> Result<(), IncreaseCapacityError> {
        // The charset this character is printed with. A single shift applies to
        // exactly one character, so it is used up here, whether the character
        // takes a cell or attaches to the previous one: xterm clears it for a
        // combining character and so do we.
        let charset: charsets::Charset = {
            let state = &mut self.screens.active.charset;
            let key = state.single_shift.unwrap_or(state.gl);
            state.single_shift = None;
            state.charsets.get(key)
        };

        // Our right margin depends where our cursor is now.
        let right_limit: CellCountInt = if self.screens.active.cursor.x > self.scrolling_region.right {
            self.cols
        } else {
            self.scrolling_region.right + 1
        };

        // Perform grapheme clustering if grapheme support is enabled (mode 2027).
        // This is MUCH slower than the normal path so the conditional below is
        // purposely ordered in least-likely to most-likely so we can drop out
        // as quickly as possible.
        if c > 255 && self.modes.get(Mode::GraphemeCluster) && self.screens.active.cursor.x > 0 {
            'grapheme: {
                let mut s = self.screens.active;

                // We need the previous cell to determine if we're at a grapheme
                // break or not. If we are NOT, then we are still combining the
                // same grapheme, and will be appending to prev.cell. Otherwise, we are
                // in a new cell.
                let (mut prev_cell, prev_left): (*mut Cell, CellCountInt) = {
                    let left: CellCountInt = 'left: {
                        // If we have wraparound, then we use the prev col unless
                        // there's a pending wrap, in which case we use the current.
                        if self.modes.get(Mode::Wraparound) {
                            break 'left (!s.cursor.pending_wrap) as CellCountInt;
                        }

                        // If we do not have wraparound, the logic is trickier. If
                        // we're not on the last column, then we just use the previous
                        // column. Otherwise, we need to check if there is text to
                        // figure out if we're attaching to the prev or current.
                        if s.cursor.x != right_limit - 1 {
                            break 'left 1;
                        }
                        (cl(s.cursor.page_cell).codepoint() == 0) as CellCountInt
                    };

                    // If the previous cell is a wide spacer tail, then we actually
                    // want to use the cell before that because that has the actual
                    // content.
                    let immediate = s.cursor_cell_left(left);
                    match cl(immediate).wide() {
                        Wide::SpacerTail => (s.cursor_cell_left(left + 1), left + 1),
                        _ => (immediate, left),
                    }
                };

                // If our cell has no content, then this is a new cell and
                // necessarily a grapheme break.
                if cl(prev_cell).codepoint() == 0 {
                    break 'grapheme;
                }

                let mut previous_codepoint: u32 = cl(prev_cell).content_codepoint();
                let grapheme_break = {
                    let mut state = BreakState::default();
                    if cl(prev_cell).has_grapheme() {
                        let cps = cursor_page(s).lookup_grapheme(cl(prev_cell)).unwrap();
                        for &cp2 in cps {
                            // With mode 2027 disabled, zero-width codepoints are
                            // attached without applying grapheme boundary rules. If
                            // the mode is enabled later, an existing cell can
                            // therefore contain one or more breaks. Feed those breaks
                            // into the state machine so it can reset its context and
                            // determine the boundary for the new codepoint.
                            let _ = unicode::grapheme_break(previous_codepoint, cp2, &mut state);
                            previous_codepoint = cp2;
                        }
                    }

                    unicode::grapheme_break(previous_codepoint, c, &mut state)
                };

                // If we can NOT break, this means that "c" is part of a grapheme
                // with the previous char.
                if !grapheme_break {
                    match unicode::grapheme_width_effect(previous_codepoint, c) {
                        unicode::GraphemeWidthEffect::Ignore => return Ok(()),
                        unicode::GraphemeWidthEffect::Wide => {
                            if cl(prev_cell).wide() != Wide::Wide {
                                // Move our cursor back to the previous. We'll move
                                // the cursor within this block to the proper location.
                                s.cursor_left(prev_left);

                                // If we don't have space for the wide char, we need to
                                // insert spacers and wrap.
                                if s.cursor.x == right_limit - 1 {
                                    if !self.modes.get(Mode::Wraparound) {
                                        return Ok(());
                                    }

                                    // This path can write a spacer_head before printWrap
                                    // which can trigger integrity violations so mark
                                    // the wrap first to keep the intermediary state valid
                                    // if we're wrapping.
                                    let row_wrap = right_limit == self.cols;
                                    if row_wrap {
                                        rw(s.cursor.page_row).set_wrap(true);
                                    }

                                    let prev_cp = cl(prev_cell).content_codepoint();
                                    // Copy the grapheme codepoints since overwriting the cell frees them.
                                    let mut prev_grapheme_buf = [0u32; pagepkg::GRAPHEME_MAX_LEN];
                                    let prev_grapheme_len: usize = if !cl(prev_cell).has_grapheme() {
                                        0
                                    } else {
                                        let cps = tp(s.cursor.page_pin).grapheme(cl(prev_cell)).unwrap();
                                        prev_grapheme_buf[..cps.len()].copy_from_slice(cps);
                                        cps.len()
                                    };

                                    self.write_cell(0, if row_wrap { Wide::SpacerHead } else { Wide::Narrow });
                                    self.print_wrap()?;
                                    self.write_cell(prev_cp, Wide::Wide);
                                    if prev_grapheme_len > 0 {
                                        let cell = s.cursor.page_cell;
                                        s.set_graphemes(cell, &prev_grapheme_buf[..prev_grapheme_len])?;
                                    }

                                    prev_cell = s.cursor.page_cell;
                                } else {
                                    cl(prev_cell).set_wide(Wide::Wide);
                                }

                                // Write our spacer, since prev.cell is now wide
                                s.cursor_right(1);

                                // Writing the spacer can grow the page to make room for
                                // the cursor hyperlink. Growing replaces the page, which
                                // invalidates `prev.cell`. Record the page identity first
                                // so the common case where nothing grows stays free.
                                //
                                // A pointer comparison alone isn't enough: pages are
                                // pooled, so a replacement can reuse the same address.
                                // The serial makes the pair a unique identity.
                                let spacer_node = tp(s.cursor.page_pin).node;
                                let spacer_serial = nd(spacer_node).serial;

                                self.write_cell(0, Wide::SpacerTail);

                                if tp(s.cursor.page_pin).node != spacer_node
                                    || nd(tp(s.cursor.page_pin).node).serial != spacer_serial
                                {
                                    // The cursor is on the spacer tail we just wrote, so
                                    // the wide cell we append to is the one to its left.
                                    prev_cell = s.cursor_cell_left(1);
                                }

                                // Move the cursor again so we're beyond our spacer
                                if s.cursor.x == right_limit - 1 {
                                    s.cursor.pending_wrap = true;
                                } else {
                                    s.cursor_right(1);
                                }
                            }
                        }

                        unicode::GraphemeWidthEffect::Narrow => {
                            // Prev cell is no longer wide
                            if cl(prev_cell).wide() == Wide::Wide {
                                cl(prev_cell).set_wide(Wide::Narrow);

                                // Remove the wide spacer tail. The previous cell may be
                                // under the cursor, so locate the tail from the wide base
                                // rather than by subtracting from the cursor distance.
                                let prev_x = s.cursor.x - prev_left;
                                if prev_x < self.cols - 1 {
                                    cl(prev_cell.wrapping_add(1)).set_wide(Wide::Narrow);
                                }

                                // Place the cursor one cell after the now-narrow base,
                                // clamped to the right edge. Usually this moves the cursor
                                // back from after the old tail, but saved cursor state or
                                // changed margins can leave it directly on the base.
                                s.cursor.pending_wrap = false;
                                s.cursor_horizontal_absolute((prev_x + 1).min(right_limit - 1));
                            }
                        }

                        unicode::GraphemeWidthEffect::NoChange => {}
                    }

                    log::debug!(
                        target: LOG_TARGET,
                        "c={:X} grapheme attach to left={} primary_cp={:X}",
                        c,
                        prev_left,
                        cl(prev_cell).codepoint()
                    );
                    s.cursor_mark_dirty();
                    s.append_grapheme(prev_cell, c)?;
                    return Ok(());
                }
            }
        }

        // Determine the width of this character so we can handle
        // non-single-width characters properly. We have a fast-path for
        // byte-sized characters since they're so common. We can ignore
        // control characters because they're always filtered prior.
        let width: usize = if c <= 0xFF {
            1
        } else {
            unicode::TABLE.get(c).width as usize
        };

        // Note: it is possible to have a width of "3" and a width of "-1" from
        // uucode.x's wcwidth. We should look into those cases and handle them
        // appropriately.
        debug_assert!(width <= 2);
        // log.debug("c={x} width={}", .{ c, width });

        let mut s = self.screens.active;

        // Attach zero-width characters to our cell as grapheme data.
        if width == 0 {
            // If we have grapheme clustering enabled, we don't blindly attach
            // any zero width character to our cells and we instead just ignore
            // it.
            if self.modes.get(Mode::GraphemeCluster) {
                return Ok(());
            }

            // If we have wraparound enabled and a pending wrap, the character
            // we're attaching to is still under the cursor. Otherwise, it's the
            // cell to the left.
            let left: CellCountInt = if self.modes.get(Mode::Wraparound) && s.cursor.pending_wrap {
                0
            } else {
                1
            };

            // If we're at cell zero and not pending a wrap, then this is malformed
            // data and we don't print anything or even store this. Zero-width
            // characters are ALWAYS attached to some other non-zero-width
            // character at the time of writing.
            if s.cursor.x == 0 && left == 1 {
                log::warn!(target: LOG_TARGET, "zero-width character with no prior character, ignoring");
                return Ok(());
            }

            // Find our previous cell
            let prev = {
                let immediate = s.cursor_cell_left(left);
                if cl(immediate).wide() != Wide::SpacerTail {
                    immediate
                } else {
                    s.cursor_cell_left(left + 1)
                }
            };

            // If our previous cell has no text, just ignore the zero-width character
            if !cl(prev).has_text() {
                log::warn!(target: LOG_TARGET, "zero-width character with no prior character, ignoring");
                return Ok(());
            }

            // If this is a emoji variation selector, prev must be an emoji
            if c == 0xFE0F || c == 0xFE0E {
                let prev_props = unicode::TABLE.get(cl(prev).content_codepoint());
                let emoji = prev_props.grapheme_break == GraphemeBreakNoControl::ExtendedPictographic;
                if !emoji {
                    return Ok(());
                }
            }

            s.append_grapheme(prev, c)?;
            return Ok(());
        }

        // We have a printable character, save it
        self.previous_char = Some(c);

        if s.cursor.pending_wrap {
            if self.modes.get(Mode::Wraparound) {
                self.print_wrap()?;
            } else {
                // Match xterm's consumption of a pending wrap.
                s.cursor.pending_wrap = false;
            }
        }

        // If we have insert mode enabled then we need to handle that. We
        // only do insert mode if we're not at the end of the line.
        if self.modes.get(Mode::Insert) && (s.cursor.x as usize) + width < self.cols as usize {
            self.insert_blanks(width);
        }

        match width {
            // Single cell is very easy: just write in the cell
            1 => {
                s.cursor_mark_dirty();
                self.print_cell(c, Wide::Narrow, charset);
            }

            // Wide character requires a spacer. We print this by
            // using two cells: the first is flagged "wide" and has the
            // wide char. The second is guaranteed to be a spacer if
            // we're not at the end of the line.
            2 => {
                if (right_limit - self.scrolling_region.left) > 1 {
                    // If we don't have space for the wide char, we need
                    // to insert spacers and wrap. Then we just print the wide
                    // char as normal.
                    if s.cursor.x == right_limit - 1 {
                        // If we don't have wraparound enabled then we don't print
                        // this character at all and don't move the cursor. This is
                        // how xterm behaves.
                        if !self.modes.get(Mode::Wraparound) {
                            return Ok(());
                        }

                        // We only create a spacer head if we're at the real edge
                        // of the screen. Otherwise, we clear the space with a narrow.
                        // This allows soft wrapping to work correctly.
                        if right_limit == self.cols {
                            // Special-case: we need to set wrap to true even
                            // though we call printWrap below because if there is
                            // a page resize during printCell then it'll fail
                            // integrity checks.
                            rw(s.cursor.page_row).set_wrap(true);
                            self.write_cell(0, Wide::SpacerHead);
                        } else {
                            self.write_cell(0, Wide::Narrow);
                        }
                        self.print_wrap()?;
                    }

                    s.cursor_mark_dirty();
                    self.print_cell(c, Wide::Wide, charset);
                    s.cursor_right(1);
                    self.write_cell(0, Wide::SpacerTail);
                } else {
                    // This is pretty broken, terminals should never be only 1-wide.
                    // We should prevent this downstream.
                    s.cursor_mark_dirty();
                    self.write_cell(0, Wide::Narrow);
                }
            }

            _ => unreachable!(),
        }

        // If we're at the column limit, then we need to wrap the next time.
        // In this case, we don't move the cursor.
        if s.cursor.x == right_limit - 1 {
            s.cursor.pending_wrap = true;
            return Ok(());
        }

        // Move the cursor
        s.cursor_right(1);
        Ok(())
    }

    /// Prints the character unmapped_c into the cell under the cursor, mapped
    /// through set, the charset print chose for it. Spacers and cells that
    /// already hold a mapped character are written with writeCell instead.
    #[inline(always)]
    fn print_cell(&mut self, unmapped_c: u32, wide: Wide, set: charsets::Charset) {
        let c: u32 = 'c: {
            // TODO: non-utf8 handling, gr

            // UTF-8 or ASCII is used as-is
            if set == charsets::Charset::Utf8 || set == charsets::Charset::Ascii {
                break 'c unmapped_c;
            }

            // The tables only cover 0x00-0xFF. Like xterm, which never remaps a
            // character above 255 in UTF-8 mode, we print anything above as is,
            // which also keeps it at the width it was measured with.
            if unmapped_c > u8::MAX as u32 {
                break 'c unmapped_c;
            }

            // Get our lookup table and map it
            let table = charsets::table(set);
            table[unmapped_c as usize] as u32
        };

        self.write_cell(c, wide);
    }

    /// Writes c into the cell under the cursor as it is, without a charset.
    #[inline(always)]
    fn write_cell(&mut self, c: u32, wide: Wide) {
        self.write_cell_(c, wide);
        self.screens.active.assert_integrity();
    }

    fn write_cell_(&mut self, c: u32, wide: Wide) {
        let mut s = self.screens.active;

        // TODO: spacers should use a bgcolor only cell

        let cell = cl(s.cursor.page_cell);

        // If the wide property of this cell is the same, then we don't
        // need to do the special handling here because the structure will
        // be the same. If it is NOT the same, then we may need to clear some
        // cells.
        if cell.wide() != wide {
            match cell.wide() {
                // Previous cell was narrow. Do nothing.
                Wide::Narrow => {}

                // Previous cell was wide. We need to clear the tail and head.
                Wide::Wide => {
                    if s.cursor.x < self.cols - 1 {
                        let spacer_cell = s.cursor_cell_right(1);
                        clear_cursor_row_cells(s, cells_mut(spacer_cell, 1));

                        // If we're near the left edge, a wide char may have
                        // wrapped from the previous row, leaving a spacer_head
                        // at the end of that row. Clear it so the previous row
                        // doesn't keep a stale spacer_head.
                        if s.cursor.y > 0 && s.cursor.x <= 1 {
                            let head_cell = cl(s.cursor_cell_end_of_prev());
                            if head_cell.wide() == Wide::SpacerHead {
                                head_cell.set_wide(Wide::Narrow);
                            }
                        }
                    }
                }

                Wide::SpacerTail => {
                    debug_assert!(s.cursor.x > 0);

                    // So integrity checks pass. We fix this up later so we don't
                    // need to do this without safety checks.
                    if build_options::SLOW_RUNTIME_SAFETY {
                        cell.set_wide(Wide::Narrow);
                    }

                    let wide_cell = s.cursor_cell_left(1);
                    clear_cursor_row_cells(s, cells_mut(wide_cell, 1));
                    // If we're near the left edge, a wide char may have
                    // wrapped from the previous row, leaving a spacer_head
                    // at the end of that row. Clear it so the previous row
                    // doesn't keep a stale spacer_head.
                    if s.cursor.y > 0 && s.cursor.x <= 1 {
                        let head_cell = cl(s.cursor_cell_end_of_prev());
                        if head_cell.wide() == Wide::SpacerHead {
                            head_cell.set_wide(Wide::Narrow);
                        }
                    }
                }

                // TODO: this case was not handled in the old terminal implementation
                // but it feels like we should do something. investigate other
                // terminals (xterm mainly) and see what's up.
                Wide::SpacerHead => {}
            }
        }

        // If the prior value had graphemes, clear those
        if cell.has_grapheme() {
            let page = cursor_page(s);
            page.clear_grapheme(cell);
            page.update_row_grapheme_flag(rw(s.cursor.page_row));
        }

        // We don't need to update the style refs unless the
        // cell's new style will be different after writing.
        let style_changed = cell.style_id() != s.cursor.style_id;
        if style_changed {
            let page = cursor_page(s);

            // Release the old style.
            if cell.style_id() != style::DEFAULT_ID {
                debug_assert!(rw(s.cursor.page_row).styled());
                // SAFETY: the style is in this page's set.
                unsafe { page.styles.release(page.memory, cell.style_id()) };
            }
        }

        // Keep track if we had a hyperlink so we can unset it.
        let had_hyperlink = cell.hyperlink();

        // Write
        *cell = {
            let mut new = Cell::init(c);
            new.set_style_id(s.cursor.style_id);
            new.set_wide(wide);
            new.set_protected(s.cursor.protected);
            new.set_semantic_content(s.cursor.semantic_content);
            new
        };

        if style_changed {
            let page = cursor_page(s);

            // Use the new style.
            if cell.style_id() != style::DEFAULT_ID {
                // SAFETY: the style is in this page's set.
                unsafe { page.styles.use_(page.memory, cell.style_id()) };
                rw(s.cursor.page_row).set_styled(true);
            }
        }

        // If this is a Kitty unicode placeholder then we need to mark the
        // row so that the renderer can lookup rows with these much faster.
        if build_options::KITTY_GRAPHICS && c == kitty::graphics::unicode::PLACEHOLDER {
            rw(s.cursor.page_row).set_kitty_virtual_placeholder(true);
        }

        // We check for an active hyperlink first because setHyperlink
        // handles clearing the old hyperlink and an optimization if we're
        // overwriting the same hyperlink.
        if s.cursor.hyperlink_id > 0 {
            if let Err(err) = s.cursor_set_hyperlink() {
                log::warn!(
                    target: LOG_TARGET,
                    "error reallocating for more hyperlink space, ignoring hyperlink err={err:?}"
                );

                // A partially successful grow can replace the page even when the
                // call fails, so `cell` may be stale here. The cursor pointers are
                // always reloaded, so read the cell through the cursor.
                debug_assert!(!cl(s.cursor.page_cell).hyperlink());
            }
        } else if had_hyperlink {
            // If the previous cell had a hyperlink then we need to clear it.
            let page = cursor_page(s);
            page.clear_hyperlink(cell);
            page.update_row_hyperlink_flag(rw(s.cursor.page_row));
        }
    }

    fn print_wrap(&mut self) -> Result<(), AllocError> {
        // We only mark that we soft-wrapped if we're at the edge of our
        // full screen. We don't mark the row as wrapped if we're in the
        // middle due to a right margin.
        let mut s = self.screens.active;
        let mark_wrap = s.cursor.x == self.cols - 1;
        if mark_wrap {
            rw(s.cursor.page_row).set_wrap(true);
        }

        // Get the old semantic prompt so we can extend it to the next
        // line. We need to do this before we index() because we may
        // modify memory.
        let old_semantic = s.cursor.semantic_content;
        let old_semantic_clear = s.cursor.semantic_content_clear_eol;

        // Move to the next line
        self.index()?;
        s.cursor_horizontal_absolute(self.scrolling_region.left);

        // Our pointer should never move
        debug_assert!(s == self.screens.active);

        // We always reset our semantic prompt state
        s.cursor.semantic_content = old_semantic;
        s.cursor.semantic_content_clear_eol = old_semantic_clear;
        match old_semantic {
            SemanticContent::Output | SemanticContent::Input => {}
            SemanticContent::Prompt => rw(s.cursor.page_row).set_semantic_prompt(RowSemanticPrompt::PromptContinuation),
        }

        if mark_wrap {
            let row = rw(s.cursor.page_row);
            // Always mark the row as a continuation
            row.set_wrap_continuation(true);
        }

        // Assure that our screen is consistent
        s.assert_integrity();
        Ok(())
    }

    /// Set the charset into the given slot.
    pub fn configure_charset(&mut self, slot: charsets::Slots, set: charsets::Charset) {
        self.screens.active.charset.charsets.set(slot, set);
    }

    /// Invoke the charset in slot into the active slot. If single is true,
    /// then this will only be invoked for a single character.
    pub fn invoke_charset(&mut self, active: charsets::ActiveSlot, slot: charsets::Slots, single: bool) {
        if single {
            debug_assert!(active == charsets::ActiveSlot::GL);
            self.screens.active.charset.single_shift = Some(slot);
            return;
        }

        match active {
            charsets::ActiveSlot::GL => self.screens.active.charset.gl = slot,
            charsets::ActiveSlot::GR => self.screens.active.charset.gr = slot,
        }
    }

    /// Carriage return moves the cursor to the first column.
    pub fn carriage_return(&mut self) {
        // Always reset pending wrap state
        self.screens.active.cursor.pending_wrap = false;

        // In origin mode we always move to the left margin
        let x = if self.modes.get(Mode::Origin) {
            self.scrolling_region.left
        } else if self.screens.active.cursor.x >= self.scrolling_region.left {
            self.scrolling_region.left
        } else {
            0
        };
        self.screens.active.cursor_horizontal_absolute(x);
    }

    /// Linefeed moves the cursor to the next line.
    pub fn linefeed(&mut self) -> Result<(), AllocError> {
        self.index()?;
        if self.modes.get(Mode::Linefeed) {
            self.carriage_return();
        }
        Ok(())
    }

    /// Backspace moves the cursor back a column (but not less than 0).
    pub fn backspace(&mut self) {
        self.cursor_left(1);
    }

    /// Move the cursor up amount lines. If amount is greater than the maximum
    /// move distance then it is internally adjusted to the maximum. If amount is
    /// 0, adjust it to 1.
    pub fn cursor_up(&mut self, count_req: usize) {
        let mut s = self.screens.active;

        // Always resets pending wrap
        s.cursor.pending_wrap = false;

        // The maximum amount the cursor can move up depends on scrolling regions
        let max = if s.cursor.y >= self.scrolling_region.top {
            s.cursor.y - self.scrolling_region.top
        } else {
            s.cursor.y
        };
        let count = (max as usize).min(count_req.max(1));

        // We can safely intCast below because of the min/max clamping we did above.
        s.cursor_up(count as CellCountInt);
    }

    /// Move the cursor down amount lines. If amount is greater than the maximum
    /// move distance then it is internally adjusted to the maximum. This sequence
    /// will not scroll the screen or scroll region. If amount is 0, adjust it to 1.
    pub fn cursor_down(&mut self, count_req: usize) {
        let mut s = self.screens.active;

        // Always resets pending wrap
        s.cursor.pending_wrap = false;

        // The max the cursor can move to depends where the cursor currently is
        let max = if s.cursor.y <= self.scrolling_region.bottom {
            self.scrolling_region.bottom - s.cursor.y
        } else {
            self.rows - s.cursor.y - 1
        };
        let count = (max as usize).min(count_req.max(1));
        s.cursor_down(count as CellCountInt);
    }

    /// Move the cursor right amount columns. If amount is greater than the
    /// maximum move distance then it is internally adjusted to the maximum.
    /// This sequence will not scroll the screen or scroll region. If amount is
    /// 0, adjust it to 1.
    pub fn cursor_right(&mut self, count_req: usize) {
        let mut s = self.screens.active;

        // Always resets pending wrap
        s.cursor.pending_wrap = false;

        // The max the cursor can move to depends where the cursor currently is
        let max = if s.cursor.x <= self.scrolling_region.right {
            self.scrolling_region.right - s.cursor.x
        } else {
            self.cols - s.cursor.x - 1
        };
        let count = (max as usize).min(count_req.max(1));
        s.cursor_right(count as CellCountInt);
    }

    /// Move the cursor to the left amount cells. If amount is 0, adjust it to 1.
    pub fn cursor_left(&mut self, count_req: usize) {
        // Wrapping behavior depends on various terminal modes
        #[derive(PartialEq, Eq)]
        enum WrapMode {
            None,
            Reverse,
            ReverseExtended,
        }
        let wrap_mode = if !self.modes.get(Mode::Wraparound) {
            WrapMode::None
        } else if self.modes.get(Mode::ReverseWrapExtended) {
            WrapMode::ReverseExtended
        } else if self.modes.get(Mode::ReverseWrap) {
            WrapMode::Reverse
        } else {
            WrapMode::None
        };

        let mut s = self.screens.active;
        let mut count = count_req.max(1);

        // If we are in no wrap mode, then we move the cursor left and exit
        // since this is the fastest and most typical path. Like cursorRight,
        // we stop at the left margin unless the cursor is already left of it.
        if wrap_mode == WrapMode::None {
            let x = s.cursor.x;
            let left_margin = if x < self.scrolling_region.left {
                0
            } else {
                self.scrolling_region.left
            };
            s.cursor_left(count.min((x - left_margin) as usize) as CellCountInt);
            s.cursor.pending_wrap = false;
            return;
        }

        // If we have a pending wrap state and we are in either reverse wrap
        // modes then we decrement the amount we move by one to match xterm.
        if s.cursor.pending_wrap {
            count -= 1;
            s.cursor.pending_wrap = false;
            if count == 0 {
                return;
            }
        }

        // The margins we can move to.
        let top = self.scrolling_region.top;
        let bottom = self.scrolling_region.bottom;
        let right_margin = self.scrolling_region.right;
        let left_margin = if s.cursor.x < self.scrolling_region.left {
            0
        } else {
            self.scrolling_region.left
        };

        // Handle some edge cases when our cursor is already on the left margin.
        if s.cursor.x == left_margin {
            match wrap_mode {
                // In reverse mode, if we're already before the top margin
                // then we just set our cursor to the top-left and we're done.
                WrapMode::Reverse => {
                    if s.cursor.y <= top {
                        s.cursor_absolute(left_margin, top);
                        return;
                    }
                }

                // Handled in while loop
                WrapMode::ReverseExtended => {}

                // Handled above
                WrapMode::None => unreachable!(),
            }
        }

        loop {
            // We can move at most to the left margin.
            let max = (s.cursor.x - left_margin) as usize;

            // We want to move at most the number of columns we have left
            // or our remaining count. Do the move.
            let amount = max.min(count);
            count -= amount;
            s.cursor_left(amount as CellCountInt);

            // If we have no more to move, then we're done.
            if count == 0 {
                break;
            }

            // If we are at the top, then we are done.
            if s.cursor.y == top {
                if wrap_mode != WrapMode::ReverseExtended {
                    break;
                }

                s.cursor_absolute(right_margin, bottom);
                count -= 1;
                continue;
            }

            // UNDEFINED TERMINAL BEHAVIOR. This situation is not handled in xterm
            // and currently results in a crash in xterm. Given no other known
            // terminal [to me] implements XTREVWRAP2, I decided to just mimic
            // the behavior of xterm up and not including the crash by wrapping
            // up to the (0, 0) and stopping there. My reasoning is that for an
            // appropriately sized value of "count" this is the behavior that xterm
            // would have. This is unit tested.
            if s.cursor.y == 0 {
                debug_assert!(s.cursor.x == left_margin);
                break;
            }

            // If our previous line is not wrapped then we are done.
            if wrap_mode != WrapMode::ReverseExtended {
                let prev_row = rw(s.cursor_row_up(1));
                if !prev_row.wrap() {
                    break;
                }
            }

            let y = s.cursor.y - 1;
            s.cursor_absolute(right_margin, y);
            count -= 1;
        }
    }

    /// Save cursor position and further state.
    ///
    /// The primary and alternate screen have distinct save state. One saved state
    /// is kept per screen (main / alternative). If for the current screen state
    /// was already saved it is overwritten.
    pub fn save_cursor(&mut self) {
        let origin = self.modes.get(Mode::Origin);
        let mut s = self.screens.active;
        s.saved_cursor = Some(SavedCursor {
            x: s.cursor.x,
            y: s.cursor.y,
            style: s.cursor.style,
            protected: s.cursor.protected,
            pending_wrap: s.cursor.pending_wrap,
            origin,
            charset: s.charset,
        });
    }

    /// Restore cursor position and other state.
    ///
    /// The primary and alternate screen have distinct save state.
    /// If no save was done before values are reset to their initial values.
    pub fn restore_cursor(&mut self) {
        let mut s = self.screens.active;
        let saved: SavedCursor = s.saved_cursor.unwrap_or(SavedCursor {
            x: 0,
            y: 0,
            style: Style::default(),
            protected: false,
            pending_wrap: false,
            origin: false,
            charset: screen::CharsetState::default(),
        });

        // Set the style first because it can fail
        s.cursor.style = saved.style;
        if let Err(err) = s.manual_style_update() {
            // Regardless of the error here, we revert back to an unstyled
            // cursor. It is more important that the restore succeeds in
            // other attributes because terminals have no way to communicate
            // failure back.
            log::warn!(target: LOG_TARGET, "restoreCursor error updating style err={err:?}");
            s.cursor.style = Style::default();
            s.manual_style_update().unwrap();
        }

        s.charset = saved.charset;
        self.modes.set(Mode::Origin, saved.origin);
        s.cursor.pending_wrap = saved.pending_wrap;
        s.cursor.protected = saved.protected;
        s.cursor_absolute(saved.x.min(self.cols - 1), saved.y.min(self.rows - 1));

        // Ensure our screen is consistent
        s.assert_integrity();
    }

    /// Set the character protection mode for the terminal.
    pub fn set_protected_mode(&mut self, mode: ansi::ProtectedMode) {
        let mut s = self.screens.active;
        match mode {
            ansi::ProtectedMode::Off => {
                s.cursor.protected = false;

                // screen.protected_mode is NEVER reset to ".off" because
                // logic such as eraseChars depends on knowing what the
                // _most recent_ mode was.
            }

            ansi::ProtectedMode::Iso => {
                s.cursor.protected = true;
                s.protected_mode = ansi::ProtectedMode::Iso;
            }

            ansi::ProtectedMode::Dec => {
                s.cursor.protected = true;
                s.protected_mode = ansi::ProtectedMode::Dec;
            }
        }
    }

    /// Perform a semantic prompt command.
    ///
    /// If there is an error, we do our best to get the terminal into
    /// some coherent state, since callers typically can't handle errors
    /// (since they're sending sequences via the pty).
    pub fn semantic_prompt(&mut self, cmd: &osc::SemanticPrompt<'_>) -> Result<(), AllocError> {
        use crate::terminal::osc::semantic_prompt::{options, Action};
        match cmd.action {
            Action::FreshLine => self.semantic_prompt_fresh_line()?,

            Action::FreshLineNewPrompt => {
                // "First do a fresh-line."
                self.semantic_prompt_fresh_line()?;

                let mut screen = self.screens.active;

                // "Subsequent text (until a OSC "133;B" or OSC "133;I" command)
                // is a prompt string (as if followed by OSC 133;P;k=i\007)."
                screen.cursor_set_semantic_content(SemanticContentUpdate::Prompt(
                    cmd.read_option::<options::PromptKind>()
                        .unwrap_or(osc_sp::PromptKind::Initial),
                ));

                // This is a kitty-specific flag that notes that the shell
                // is NOT capable of redraw. Redraw defaults to true so this
                // usually just disables it, but either is possible.
                if let Some(v) = cmd.read_option::<options::Redraw>() {
                    self.flags.shell_redraws_prompt = v;
                }

                'click: {
                    // Handle click_events as a priority over cl. click_events
                    // is another Kitty-specific extension that converts clicks
                    // within a prompt area to SGR mouse events and defers to the
                    // shell to handle them.
                    if let Some(v) = cmd.read_option::<options::ClickEvents>() {
                        screen.semantic_prompt.click = SemanticClick::ClickEvents(v);
                        break 'click;
                    }

                    // If click_events was not set or disabled, fallback to `cl`.
                    if let Some(v) = cmd.read_option::<options::Cl>() {
                        screen.semantic_prompt.click = SemanticClick::Cl(v);
                    }
                }

                // The "aid" and "cl" options are also valid for this
                // command but we don't yet handle these in any meaningful way.
            }

            Action::NewCommand => {
                // Spec:
                // Same as OSC "133;A" but may first implicitly terminate a
                // previous command: if the options specify an aid and there
                // is an active (open) command with matching aid, finish the
                // innermost such command (as well as any other commands
                // nested more deeply). If no aid is specified, treat as an
                // aid whose value is the empty string.

                // Ghostty:
                // We don't currently do explicit command tracking in any way
                // so there is no need to terminate prior commands. We just
                // perform the `A` action.
                self.semantic_prompt(&osc::SemanticPrompt {
                    action: Action::FreshLineNewPrompt,
                    options_unvalidated: cmd.options_unvalidated,
                })?;
            }

            Action::PromptStart => {
                // Explicit start of prompt. Optional after an A or N command.
                // The k (kind) option specifies the type of prompt:
                // regular primary prompt (k=i or default),
                // right-side prompts (k=r), or prompts for continuation lines (k=c or k=s).
                self.screens.active.cursor_set_semantic_content(SemanticContentUpdate::Prompt(
                    cmd.read_option::<options::PromptKind>()
                        .unwrap_or(osc_sp::PromptKind::Initial),
                ));
            }

            Action::EndPromptStartInput => {
                // End of prompt and start of user input, terminated by a OSC
                // "133;C" or another prompt (OSC "133;P").
                self.screens
                    .active
                    .cursor_set_semantic_content(SemanticContentUpdate::Input(InputClear::ClearExplicit));
            }

            Action::EndPromptStartInputTerminateEol => {
                // End of prompt and start of user input, terminated by end-of-line.
                self.screens
                    .active
                    .cursor_set_semantic_content(SemanticContentUpdate::Input(InputClear::ClearEol));
            }

            Action::EndInputStartOutput => {
                // "End of input, and start of output."
                let mut s = self.screens.active;
                s.cursor_set_semantic_content(SemanticContentUpdate::Output);

                // If our current row is marked as a prompt and we're
                // at column zero then we assume we're un-prompting. This
                // is a heuristic to deal with fish, mostly. The issue that
                // fish brings up is that it has no PS2 equivalent and its
                // builtin OSC133 marking doesn't output continuation lines
                // as k=s. So, we assume when we get a newline with a prompt
                // cursor that the new line is also a prompt. But fish changes
                // to output on the newline. So if we're at col 0 we just assume
                // we're overwriting the prompt.
                if rw(s.cursor.page_row).semantic_prompt() != RowSemanticPrompt::None && s.cursor.x == 0 {
                    rw(s.cursor.page_row).set_semantic_prompt(RowSemanticPrompt::None);
                }
            }

            Action::EndCommand => {
                // From a terminal state perspective, this doesn't really do
                // anything. Other terminals appear to do nothing here. I think
                // its reasonable at this point to reset our semantic content
                // state but the spec doesn't really say what to do.
                self.screens.active.cursor_set_semantic_content(SemanticContentUpdate::Output);
            }
        }
        Ok(())
    }

    // OSC 133;L
    fn semantic_prompt_fresh_line(&mut self) -> Result<(), AllocError> {
        let left_margin = if self.screens.active.cursor.x < self.scrolling_region.left {
            0
        } else {
            self.scrolling_region.left
        };

        // Spec: "If the cursor is the initial column (left, assuming
        // left-to-right writing), do nothing" This specification is very under
        // specified. We are taking the liberty to assume that in a left/right
        // margin context, if the cursor is outside of the left margin, we treat
        // it as being at the left margin for the purposes of this command.
        // This is arbitrary. If someone has a better reasonable idea we can
        // apply it.
        if self.screens.active.cursor.x == left_margin {
            return Ok(());
        }

        self.carriage_return();
        self.index()
    }

    /// Returns true if the cursor is currently at a prompt. Another way to look
    /// at this is it returns false if the shell is currently outputting something.
    /// This requires shell integration (semantic prompt integration).
    ///
    /// If the shell integration doesn't exist, this will always return false.
    pub fn cursor_is_at_prompt(&self) -> bool {
        // If we're on the secondary screen, we're never at a prompt.
        if self.screens.active_key == ScreenKey::Alternate {
            return false;
        }

        // If our page row is a prompt then we're always at a prompt
        let cursor: &screen::Cursor = &self.screens.active.cursor;
        if rw(cursor.page_row).semantic_prompt() != RowSemanticPrompt::None {
            return true;
        }

        // Otherwise, determine our cursor state
        match cursor.semantic_content {
            SemanticContent::Input | SemanticContent::Prompt => true,
            SemanticContent::Output => false,
        }
    }

    /// Horizontal tab moves the cursor to the next tabstop, clearing
    /// the screen to the left the tabstop.
    pub fn horizontal_tab(&mut self) {
        let mut s = self.screens.active;
        while s.cursor.x < self.scrolling_region.right {
            // Move the cursor right
            s.cursor_right(1);

            // If the last cursor position was a tabstop we return. We do
            // "last cursor position" because we want a space to be written
            // at the tabstop unless we're at the end (the while condition).
            if self.tabstops.get(s.cursor.x as usize) {
                return;
            }
        }
    }

    // Same as horizontalTab but moves to the previous tabstop instead of the next.
    pub fn horizontal_tab_back(&mut self) {
        // With origin mode enabled, our leftmost limit is the left margin.
        let left_limit = if self.modes.get(Mode::Origin) {
            self.scrolling_region.left
        } else {
            0
        };

        let mut s = self.screens.active;
        loop {
            // If we're already at the edge of the screen, then we're done.
            if s.cursor.x <= left_limit {
                return;
            }

            // Move the cursor left
            s.cursor_left(1);
            if self.tabstops.get(s.cursor.x as usize) {
                return;
            }
        }
    }

    /// Clear tab stops.
    pub fn tab_clear(&mut self, cmd: csi::TabClear) {
        if cmd == csi::TabClear::CURRENT {
            self.tabstops.unset(self.screens.active.cursor.x as usize);
        } else if cmd == csi::TabClear::ALL {
            self.tabstops.reset(0);
        } else {
            log::warn!(target: LOG_TARGET, "invalid or unknown tab clear setting: {cmd:?}");
        }
    }

    /// Set a tab stop on the current cursor.
    /// TODO: test
    pub fn tab_set(&mut self) {
        self.tabstops.set(self.screens.active.cursor.x as usize);
    }

    /// TODO: test
    pub fn tab_reset(&mut self) {
        self.tabstops.reset(TABSTOP_INTERVAL);
    }

    /// Move the cursor to the next line in the scrolling region, possibly scrolling.
    ///
    /// If the cursor is outside of the scrolling region: move the cursor one line
    /// down if it is not on the bottom-most line of the screen.
    ///
    /// If the cursor is inside the scrolling region:
    ///   If the cursor is on the bottom-most line of the scrolling region:
    ///     invoke scroll up with amount=1
    ///   If the cursor is not on the bottom-most line of the scrolling region:
    ///     move the cursor one line down
    ///
    /// This unsets the pending wrap state without wrapping.
    pub fn index(&mut self) -> Result<(), AllocError> {
        let mut screen = self.screens.active;

        // Unset pending wrap state
        screen.cursor.pending_wrap = false;

        let r = self.index_(screen);

        // We handle our cursor semantic prompt state AFTER doing the
        // scrolling, because we may need to apply to new rows.
        if screen.cursor.semantic_content != SemanticContent::Output {
            // Always reset any semantic content clear-eol state.
            //
            // The specification is not clear what "end-of-line" means. If we
            // discover that there are more scenarios we should be unsetting
            // this we should document and test it.
            if screen.cursor.semantic_content_clear_eol {
                screen.cursor.semantic_content = SemanticContent::Output;
                screen.cursor.semantic_content_clear_eol = false;
            } else {
                // If we aren't clearing our state at EOL and we're not output,
                // then we mark the new row as a prompt continuation. This is
                // to work around shells that don't send OSC 133 k=s sequences
                // for continuations.
                //
                // This can be a false positive if the shell changes content
                // type later and outputs something. We handle that in the
                // semanticPrompt function.
                rw(screen.cursor.page_row).set_semantic_prompt(RowSemanticPrompt::PromptContinuation);
            }
        } else {
            // This should never be set in the output mode.
            debug_assert!(!screen.cursor.semantic_content_clear_eol);
        }

        r
    }

    fn index_(&mut self, mut screen: ScreenPtr) -> Result<(), AllocError> {
        // Outside of the scroll region we move the cursor one line down.
        if screen.cursor.y < self.scrolling_region.top || screen.cursor.y > self.scrolling_region.bottom {
            // We only move down if we're not already at the bottom of
            // the screen.
            if screen.cursor.y < self.rows - 1 {
                screen.cursor_down(1);
            }

            return Ok(());
        }

        // If the cursor is inside the scrolling region and on the bottom-most
        // line, then we scroll up. If our scrolling region is the full screen
        // we create scrollback.
        if screen.cursor.y == self.scrolling_region.bottom
            && screen.cursor.x >= self.scrolling_region.left
            && screen.cursor.x <= self.scrolling_region.right
        {
            if build_options::KITTY_GRAPHICS {
                // Scrolling dirties the images because it updates their placements pins.
                screen.kitty_images.mark_dirty();
            }

            // If our scrolling region is at the top, we create scrollback,
            // but only if our screen retains scrollback. If our screen
            // doesn't retain scrollback (e.g. the alternate screen) then
            // creating scrollback is pure overhead: the rows are never
            // visible and are simply pruned later. In that case we use the
            // in-place region scroll below, unless the region is a single
            // row (a one row screen) which cursorScrollRegionUp can't
            // handle (and cursorDownScroll special-cases).
            if self.scrolling_region.top == 0
                && self.scrolling_region.left == 0
                && self.scrolling_region.right == self.cols - 1
                && (!screen.no_scrollback || self.scrolling_region.bottom == 0)
            {
                // If a bottom margin is set, kitty image placements may
                // need adjusting around the scroll. The rare placements-
                // present case is handled out of line so this hot path
                // only pays a count check (a load from a cache line we
                // already write, above).
                if build_options::KITTY_GRAPHICS && kitty_placement_count(&screen.kitty_images) != 0 {
                    return self.index_scroll_with_images(KittyScrollOp::WindowShift);
                }

                return screen.cursor_scroll_above();
            }

            // Slow path for left and right scrolling region margins.
            // scrollUp handles the kitty image adjustment itself.
            if self.scrolling_region.left != 0 || self.scrolling_region.right != self.cols - 1 {
                return self.scroll_up(1);
            }

            // Kitty image placements may need adjusting around the scroll;
            // handled out of line like the scrollback path above.
            if build_options::KITTY_GRAPHICS && kitty_placement_count(&screen.kitty_images) != 0 {
                return self.index_scroll_with_images(KittyScrollOp::InPlace);
            }

            // Otherwise use a fast path function to efficiently scroll
            // the contents of the scrolling region.
            return screen.cursor_scroll_region_up((self.scrolling_region.bottom - self.scrolling_region.top) as usize);
        }

        // Increase cursor by 1, maximum to bottom of scroll region
        if screen.cursor.y < self.scrolling_region.bottom {
            screen.cursor_down(1);
        }

        Ok(())
    }

    /// The operation when we have Kitty image placements during index.
    /// Split out of index() so its hot paths don't carry the adjustment
    /// state in their stack frame, which measurably slows the scroll
    /// hot path.
    #[cold]
    fn index_scroll_with_images(&mut self, op: KittyScrollOp) -> Result<(), AllocError> {
        let mut kitty_scroll = self.kitty_scroll_margins_begin(-1, op);
        let r = match op {
            KittyScrollOp::WindowShift => self.screens.active.cursor_scroll_above(),
            KittyScrollOp::InPlace => {
                let n = (self.scrolling_region.bottom - self.scrolling_region.top) as usize;
                self.screens.active.cursor_scroll_region_up(n)
            }
        };
        if let Some(state) = kitty_scroll.as_mut() {
            state.end();
        }
        r
    }

    /// Begin adjusting kitty image placements for a scroll of the
    /// scrolling region by delta rows (negative moves content up). If
    /// adjustment is needed this returns state whose end() must be called
    /// after the scroll's row operations complete (see
    /// ImageStorage.scrollMarginsBegin for why this is two phases). This
    /// returns null when the scrolling region is the full screen, because
    /// placements then follow their anchored rows via pin tracking which
    /// matches kitty's marginless behavior.
    ///
    /// Callers must comptime-gate on build_options.kitty_graphics and
    /// check that placements exist before calling, which keeps the cost
    /// on the hot scroll paths cheap.
    #[cold]
    fn kitty_scroll_margins_begin(&mut self, delta: isize, op: KittyScrollOp) -> Option<KittyScrollMargins> {
        // Full-screen scrolls need no adjustment: placements follow their
        // anchored rows (possibly into the scrollback) via pin tracking.
        if self.scrolling_region.top == 0
            && self.scrolling_region.bottom == self.rows - 1
            && self.scrolling_region.left == 0
            && self.scrolling_region.right == self.cols - 1
        {
            return None;
        }

        // TODO(port-dedupe): screen.kitty_images.scrollMarginsBegin(self,
        // delta, op) is pending the graphics_storage port. Without stored
        // placements there is nothing to adjust.
        let _ = (delta, op);
        None
    }

    /// Move the cursor to the previous line in the scrolling region, possibly
    /// scrolling.
    ///
    /// If the cursor is outside of the scrolling region, move the cursor one
    /// line up if it is not on the top-most line of the screen.
    ///
    /// If the cursor is inside the scrolling region:
    ///
    ///   * If the cursor is on the top-most line of the scrolling region:
    ///     invoke scroll down with amount=1
    ///   * If the cursor is not on the top-most line of the scrolling region:
    ///     move the cursor one line up
    pub fn reverse_index(&mut self) {
        if self.screens.active.cursor.y != self.scrolling_region.top
            || self.screens.active.cursor.x < self.scrolling_region.left
            || self.screens.active.cursor.x > self.scrolling_region.right
        {
            self.cursor_up(1);
            return;
        }

        self.scroll_down(1);
    }

    /// Set Cursor Position. Move cursor to the position indicated
    /// by row and column (1-indexed). If column is 0, it is adjusted to 1.
    /// If column is greater than the right-most column it is adjusted to
    /// the right-most column. If row is 0, it is adjusted to 1. If row is
    /// greater than the bottom-most row it is adjusted to the bottom-most
    /// row.
    pub fn set_cursor_pos(&mut self, row_req: usize, col_req: usize) {
        // If cursor origin mode is set the cursor row will be moved relative to
        // the top margin row and adjusted to be above or at bottom-most row in
        // the current scroll region.
        //
        // If origin mode is set and left and right margin mode is set the cursor
        // will be moved relative to the left margin column and adjusted to be on
        // or left of the right margin column.
        let (x_offset, y_offset, x_max, y_max): (usize, usize, usize, usize) = if self.modes.get(Mode::Origin) {
            (
                self.scrolling_region.left as usize,
                self.scrolling_region.top as usize,
                self.scrolling_region.right as usize + 1, // We need this 1-indexed
                self.scrolling_region.bottom as usize + 1, // We need this 1-indexed
            )
        } else {
            (0, 0, self.cols as usize, self.rows as usize)
        };

        let mut s = self.screens.active;

        // Unset pending wrap state
        s.cursor.pending_wrap = false;

        // Calculate our new x/y
        let row = if row_req == 0 { 1 } else { row_req };
        let col = if col_req == 0 { 1 } else { col_req };
        let x = x_max.min(col.saturating_add(x_offset)).saturating_sub(1) as CellCountInt;
        let y = y_max.min(row.saturating_add(y_offset)).saturating_sub(1) as CellCountInt;

        // If the y is unchanged then this is fast pointer math
        if y == s.cursor.y {
            if x > s.cursor.x {
                let n = x - s.cursor.x;
                s.cursor_right(n);
            } else {
                let n = s.cursor.x - x;
                s.cursor_left(n);
            }

            return;
        }

        // If everything changed we do an absolute change which is slightly slower
        s.cursor_absolute(x, y);
        // log.info("set cursor position: col={} row={}", .{ self.screens.active.cursor.x, self.screens.active.cursor.y });
    }

    /// The cursor position in the coordinates setCursorPos takes: 1-indexed,
    /// and relative to the top-left of the scrolling region in origin mode.
    /// Handing either coordinate back to setCursorPos leaves it unchanged.
    fn cursor_pos_for_set(&self) -> (usize, usize) {
        let cursor = &self.screens.active.cursor;
        let origin = self.modes.get(Mode::Origin);
        let row = cursor.y.saturating_sub(if origin { self.scrolling_region.top } else { 0 }) as usize + 1;
        let col = cursor.x.saturating_sub(if origin { self.scrolling_region.left } else { 0 }) as usize + 1;
        (row, col)
    }

    /// Move the cursor to the given row (1-indexed), keeping its column (VPA).
    /// In origin mode the row is relative to the top margin.
    pub fn set_cursor_row(&mut self, row: usize) {
        let (_, col) = self.cursor_pos_for_set();
        self.set_cursor_pos(row, col);
    }

    /// Move the cursor to the given column (1-indexed), keeping its row (CHA
    /// and HPA). In origin mode the column is relative to the left margin.
    pub fn set_cursor_col(&mut self, col: usize) {
        let (row, _) = self.cursor_pos_for_set();
        self.set_cursor_pos(row, col);
    }

    /// Move the cursor down by count rows, keeping its column (VPR). Unlike
    /// cursorDown, this stops only at the bottom of the screen, or of the
    /// scrolling region in origin mode.
    pub fn cursor_row_relative(&mut self, count: usize) {
        let (row, col) = self.cursor_pos_for_set();
        self.set_cursor_pos(row.saturating_add(count), col);
    }

    /// Move the cursor right by count columns, keeping its row (HPR). Unlike
    /// cursorRight, this stops only at the right edge of the screen, or of
    /// the scrolling region in origin mode.
    pub fn cursor_col_relative(&mut self, count: usize) {
        let (row, col) = self.cursor_pos_for_set();
        self.set_cursor_pos(row, col.saturating_add(count));
    }

    /// Set Top and Bottom Margins If bottom is not specified, 0 or bigger than
    /// the number of the bottom-most row, it is adjusted to the number of the
    /// bottom most row.
    ///
    /// If top < bottom set the top and bottom row of the scroll region according
    /// to top and bottom and move the cursor to the top-left cell of the display
    /// (when in cursor origin mode is set to the top-left cell of the scroll region).
    ///
    /// Otherwise: Set the top and bottom row of the scroll region to the top-most
    /// and bottom-most line of the screen.
    ///
    /// Top and bottom are 1-indexed.
    pub fn set_top_and_bottom_margin(&mut self, top_req: usize, bottom_req: usize) {
        let top = top_req.max(1);
        let bottom = (self.rows as usize).min(if bottom_req == 0 { self.rows as usize } else { bottom_req });
        if top >= bottom {
            return;
        }

        self.scrolling_region.top = (top - 1) as CellCountInt;
        self.scrolling_region.bottom = (bottom - 1) as CellCountInt;
        self.set_cursor_pos(1, 1);
    }

    /// DECSLRM
    pub fn set_left_and_right_margin(&mut self, left_req: usize, right_req: usize) {
        // We must have this mode enabled to do anything
        if !self.modes.get(Mode::EnableLeftAndRightMargin) {
            return;
        }

        let left = left_req.max(1);
        let right = (self.cols as usize).min(if right_req == 0 { self.cols as usize } else { right_req });
        if left >= right {
            return;
        }

        self.scrolling_region.left = (left - 1) as CellCountInt;
        self.scrolling_region.right = (right - 1) as CellCountInt;
        self.set_cursor_pos(1, 1);
    }

    /// Scroll the text down by one row.
    pub fn scroll_down(&mut self, count: usize) {
        // Preserve our x/y to restore.
        let old_x = self.screens.active.cursor.x;
        let old_y = self.screens.active.cursor.y;
        let old_wrap = self.screens.active.cursor.pending_wrap;

        // If margins are set and kitty image placements exist, they need
        // adjusting around the scroll. Note this wraps scrollDown and NOT
        // insertLines: kitty scrolls images for SD/RI but leaves them
        // alone for IL/DL.
        let mut kitty_scroll: Option<KittyScrollMargins> = None;
        if build_options::KITTY_GRAPHICS && kitty_placement_count(&self.screens.active.kitty_images) != 0 {
            let region_height: usize = (self.scrolling_region.bottom - self.scrolling_region.top) as usize + 1;
            kitty_scroll = self.kitty_scroll_margins_begin(count.min(region_height) as isize, KittyScrollOp::InPlace);
        }

        // Move to the top of the scroll region
        let (left, top) = (self.scrolling_region.left, self.scrolling_region.top);
        self.screens.active.cursor_absolute(left, top);
        self.insert_lines(count);

        if let Some(state) = kitty_scroll.as_mut() {
            state.end();
        }
        self.screens.active.cursor_absolute(old_x, old_y);
        self.screens.active.cursor.pending_wrap = old_wrap;
    }

    /// Removes amount lines from the top of the scroll region. The remaining lines
    /// to the bottom margin are shifted up and space from the bottom margin up
    /// is filled with empty lines.
    ///
    /// The new lines are created according to the current SGR state.
    ///
    /// Does not change the (absolute) cursor position.
    pub fn scroll_up(&mut self, count: usize) -> Result<(), AllocError> {
        // Preserve our x/y to restore.
        let old_x = self.screens.active.cursor.x;
        let old_y = self.screens.active.cursor.y;
        let old_wrap = self.screens.active.cursor.pending_wrap;

        // If margins are set and kitty image placements exist, they need
        // adjusting around the scroll. Note this wraps scrollUp and NOT
        // deleteLines: kitty scrolls images for SU/IND but leaves them
        // alone for IL/DL.
        let mut kitty_scroll: Option<KittyScrollMargins> = None;
        if build_options::KITTY_GRAPHICS && kitty_placement_count(&self.screens.active.kitty_images) != 0 {
            // The op must mirror the branch below: the scrollback path
            // shifts the active window while the deleteLines path
            // moves rows in place.
            let region_height: usize = (self.scrolling_region.bottom - self.scrolling_region.top) as usize + 1;
            let op = if self.scrolling_region.top == 0
                && self.scrolling_region.left == 0
                && self.scrolling_region.right == self.cols - 1
                && (!self.screens.active.no_scrollback || self.scrolling_region.bottom == self.rows - 1)
            {
                KittyScrollOp::WindowShift
            } else {
                KittyScrollOp::InPlace
            };
            kitty_scroll = self.kitty_scroll_margins_begin(-(count.min(region_height) as isize), op);
        }

        let r = self.scroll_up_(count);

        if let Some(state) = kitty_scroll.as_mut() {
            state.end();
        }
        self.screens.active.cursor_absolute(old_x, old_y);
        self.screens.active.cursor.pending_wrap = old_wrap;
        r
    }

    fn scroll_up_(&mut self, count: usize) -> Result<(), AllocError> {
        let mut s = self.screens.active;

        // If our scroll region is at the top and we have no left/right
        // margins then we move the scrolled out text into the scrollback.
        //
        // If our screen doesn't retain scrollback (e.g. the alternate
        // screen) then creating scrollback is pure overhead, so we use the
        // deleteLines path below instead, unless the region is the full
        // screen where cursorScrollAbove has a specialized fast path
        // (cursorDownScroll) for scrolling without scrollback.
        if self.scrolling_region.top == 0
            && self.scrolling_region.left == 0
            && self.scrolling_region.right == self.cols - 1
            && (!s.no_scrollback || self.scrolling_region.bottom == self.rows - 1)
        {
            // Scrolling dirties the images because it updates their placements pins.
            if build_options::KITTY_GRAPHICS {
                s.kitty_images.mark_dirty();
            }

            // Clamp count to the scroll region height.
            let region_height = self.scrolling_region.bottom as usize + 1;
            let adjusted_count = count.min(region_height);

            // TODO: Create an optimized version that can scroll N times
            // This isn't critical because in most cases, scrollUp is used
            // with count=1, but it's still a big optimization opportunity.

            // Move our cursor to the bottom of the scroll region so we can
            // use the cursorScrollAbove function to create scrollback
            s.cursor_absolute(0, self.scrolling_region.bottom);
            for _ in 0..adjusted_count {
                s.cursor_scroll_above()?;
            }
            return Ok(());
        }

        // Move to the top of the scroll region
        s.cursor_absolute(self.scrolling_region.left, self.scrolling_region.top);
        self.delete_lines(count);
        Ok(())
    }

    /// Scroll the viewport of the terminal grid.
    pub fn scroll_viewport(&mut self, behavior: ScrollViewport) {
        self.screens.active.scroll(match behavior {
            ScrollViewport::Top => screen::Scroll::Top,
            ScrollViewport::Bottom => screen::Scroll::Active,
            ScrollViewport::Delta(delta) => screen::Scroll::DeltaRow(delta),
            ScrollViewport::Row(row) => screen::Scroll::Row(row),
            ScrollViewport::DeltaPrompt(delta) => screen::Scroll::DeltaPrompt(delta),
        });
    }

    /// Return the current compression activity value.
    ///
    /// Callers should schedule a `compress` call whenever this value changes. The
    /// direction of the change has no meaning; this is an opaque change token
    /// rather than a monotonic sequence exposed by Terminal.
    ///
    /// It is up to the terminal what it decides to compress, but currently
    /// we compress cold (non-viewed, non-editable) scrollback history on
    /// the primary screen.
    ///
    /// Note that compression requires specific system features, namely
    /// the ability to retain virtual memory allocations while discarding their
    /// physical memory backings. Callers must still use `compress` to determine
    /// whether compression is supported on the current target.
    pub fn compression_activity(&self) -> u64 {
        let primary = self.screens.get(ScreenKey::Primary).unwrap();
        let state = &primary.pages.page_compression;
        // For now we don't use the extra 16 bits.
        state.activity_serial
    }

    /// Compress cold memory to save resident memory space.
    ///
    /// Full compression does a full pass compressing everything it can before
    /// returning. This is not recommended for interactive terminals because
    /// compression is relatively slow and with large scrollbacks this can cause
    /// stalls.
    ///
    /// Incremental compression bounds itself on how much data it can look
    /// up to compress and how much compression work it does before returning.
    /// It is stateful (we maintain the state) and the return value tells callers
    /// whether they should continue calling it in the future.
    ///
    /// Callers should schedule compression when it doesn't impact user
    /// experience, for example during idle times.
    pub fn compress(&mut self, mode: CompressionMode) -> CompressionResult {
        let mut primary = self.screens.get(ScreenKey::Primary).unwrap();
        let pages = &mut primary.pages;
        let result = match mode {
            CompressionMode::Incremental => pages.compress(CompressMode::Incremental),
            CompressionMode::Full => pages.compress(CompressMode::Full),
        };

        match result {
            IncrementalCompressionResult::Unsupported => CompressionResult::Unsupported,
            IncrementalCompressionResult::Pending => CompressionResult::Pending,
            IncrementalCompressionResult::Complete => CompressionResult::Complete,
        }
    }

    /// To be called before shifting a row (as in insertLines and deleteLines)
    ///
    /// Takes care of boundary conditions such as potentially split wide chars
    /// across scrolling region boundaries and orphaned spacer heads at line
    /// ends.
    fn row_will_be_shifted(&self, page: &mut Page, row: &mut Row) {
        // SAFETY: the row belongs to the page.
        let cells = unsafe { &mut *page.get_cells(row) };

        // If our scrolling region includes the rightmost column then we
        // need to turn any spacer heads in to normal empty cells, since
        // once we move them they no longer correspond with soft-wrapped
        // wide characters.
        //
        // If it contains either of the 2 leftmost columns, then the wide
        // characters in the first column which may be associated with a
        // spacer head will be either moved or cleared, so we also need
        // to turn the spacer heads in to empty cells in that case.
        if self.scrolling_region.right == self.cols - 1 || self.scrolling_region.left < 2 {
            let end_cell = &mut cells[page.size.cols as usize - 1];
            if end_cell.wide() == Wide::SpacerHead {
                end_cell.set_wide(Wide::Narrow);
            }
        }

        // If the leftmost or rightmost cells of our scrolling region
        // are parts of wide chars, we need to clear the cells' contents
        // since they'd be split by the move.
        let left = self.scrolling_region.left as usize;
        let right = self.scrolling_region.right as usize;

        if cells[left].wide() == Wide::SpacerTail {
            let wide_cell = &mut cells[left - 1];
            if wide_cell.has_grapheme() {
                page.clear_grapheme(wide_cell);
                page.update_row_grapheme_flag(row);
            }
            wide_cell.set_content_codepoint(0);
            wide_cell.set_wide(Wide::Narrow);
            cells[left].set_wide(Wide::Narrow);
        }

        if cells[right].wide() == Wide::Wide {
            let right_cell = &mut cells[right];
            if right_cell.has_grapheme() {
                page.clear_grapheme(right_cell);
                page.update_row_grapheme_flag(row);
            }
            right_cell.set_content_codepoint(0);
            right_cell.set_wide(Wide::Narrow);
            cells[right + 1].set_wide(Wide::Narrow);
        }
    }

    /// Renew every live page generation in an inclusive range before a full-width
    /// line operation moves logical rows between their coordinates.
    fn invalidate_full_width_row_range(&mut self, first: NodePtr, last: NodePtr) {
        let mut node = first;
        loop {
            // Full-width line movement remaps cached row coordinates in this page.
            self.screens.active.pages.invalidate_node_layout(node);
            if node == last {
                break;
            }
            node = nd(node).next.unwrap();
        }
    }

    // TODO(qwerasd): `insertLines` and `deleteLines` are 99% identical,
    // the majority of their logic can (and should) be abstracted in to
    // a single shared helper function, probably on `Screen` not here.
    // I'm just too lazy to do that rn :p

    /// Insert amount lines at the current cursor row. The contents of the line
    /// at the current cursor row and below (to the bottom-most line in the
    /// scrolling region) are shifted down by amount lines. The contents of the
    /// amount bottom-most lines in the scroll region are lost.
    ///
    /// This unsets the pending wrap state without wrapping. If the current cursor
    /// position is outside of the current scroll region it does nothing.
    ///
    /// If amount is greater than the remaining number of lines in the scrolling
    /// region it is adjusted down (still allowing for scrolling out every remaining
    /// line in the scrolling region)
    ///
    /// In left and right margin mode the margins are respected; lines are only
    /// scrolled in the scroll region.
    ///
    /// All cleared space is colored according to the current SGR state.
    ///
    /// Moves the cursor to the left margin.
    pub fn insert_lines(&mut self, count: usize) {
        // Rare, but happens
        if count == 0 {
            return;
        }

        let mut s = self.screens.active;

        // If the cursor is outside the scroll region we do nothing.
        if s.cursor.y < self.scrolling_region.top
            || s.cursor.y > self.scrolling_region.bottom
            || s.cursor.x < self.scrolling_region.left
            || s.cursor.x > self.scrolling_region.right
        {
            return;
        }

        if build_options::KITTY_GRAPHICS {
            // Scrolling dirties the images because it updates their placements pins.
            s.kitty_images.mark_dirty();
        }

        // At the end we need to return the cursor to the row it started on.
        let start_y = s.cursor.y;

        // We have a slower path if we have left or right scroll margins.
        let left_right = self.scrolling_region.left > 0 || self.scrolling_region.right < self.cols - 1;

        // Remaining rows from our cursor to the bottom of the scroll region.
        let rem = (self.scrolling_region.bottom - s.cursor.y) as usize + 1;

        // We can only insert lines up to our remaining lines in the scroll
        // region. So we take whichever is smaller.
        let adjusted_count = count.min(rem);

        // Create a new tracked pin which we'll use to navigate the page list
        // so that if we need to adjust capacity it will be properly tracked.
        let start_pin = tp(s.cursor.page_pin).down(rem - 1).unwrap();
        let cur_p = match s.pages.track_pin(start_pin) {
            Ok(p) => p,
            Err(err) => {
                // This error scenario means that our GPA is OOM. This is not a
                // situation we can gracefully handle. We can't just ignore insertLines
                // because it'll result in a corrupted screen. Ideally in the future
                // we flag the state as broken and show an error message to the user.
                // For now, we panic.
                log::error!(target: LOG_TARGET, "insertLines trackPin error err={err:?}");
                panic!("insertLines trackPin OOM");
            }
        };

        // Partial-width margins edit cells in stable rows; full-width moves rows.
        if !left_right {
            let first = tp(s.cursor.page_pin).node;
            let last = tp(cur_p).node;
            self.invalidate_full_width_row_range(first, last);
        }

        let left = self.scrolling_region.left as usize;
        let right = self.scrolling_region.right as usize;

        // Our current y position relative to the cursor
        let mut y: usize = rem;

        // Traverse from the bottom up
        while y > 0 {
            let cur_rac = tp(cur_p).row_and_cell();
            let cur_row: &mut Row = rw(cur_rac.row);

            // If this is one of the lines we need to shift, do so
            if y > adjusted_count {
                let off_p = tp(cur_p).up(adjusted_count).unwrap();
                let off_rac = off_p.row_and_cell();
                let off_row: &mut Row = rw(off_rac.row);

                self.row_will_be_shifted(nd(tp(cur_p).node).page(), cur_row);
                self.row_will_be_shifted(nd(off_p.node).page(), off_row);

                // If our scrolling region is full width, then we unset wrap.
                if !left_right {
                    off_row.set_wrap(false);
                    cur_row.set_wrap(false);
                    off_row.set_wrap_continuation(false);
                    cur_row.set_wrap_continuation(false);
                }

                let src_p = off_p;
                let src_row = off_row;
                let dst_p = *tp(cur_p);
                let dst_row = cur_row;

                // If our page doesn't match, then we need to do a copy from
                // one page to another. This is the slow path.
                if src_p.node != dst_p.node {
                    // The copy may replace the destination node in order
                    // to increase its capacity. Our pins are tracked so
                    // they update automatically; we can discard the
                    // replacement because the remainder of this iteration
                    // only accesses rows through the pins.
                    let _ = s.clone_partial_row_grow_capacity(
                        dst_p.node,
                        dst_p.y as usize,
                        nd(src_p.node).page() as *const Page,
                        src_row as *const Row,
                        left,
                        right + 1,
                    );
                } else if !left_right {
                    // Swap the src/dst cells. This ensures that our dst gets the
                    // proper shifted rows and src gets non-garbage cell data that
                    // we can clear.
                    std::mem::swap(dst_row, src_row);

                    // Ensure what we did didn't corrupt the page
                    nd(tp(cur_p).node).page().assert_integrity();
                } else {
                    // Left/right scroll margins we have to
                    // copy cells, which is much slower...
                    let page = nd(tp(cur_p).node).page();
                    page.move_cells(src_row, left, dst_row, left, (right - left) + 1);
                }
            } else {
                // Clear the cells for this row, it has been shifted.
                self.row_will_be_shifted(nd(tp(cur_p).node).page(), cur_row);
                let page = nd(tp(cur_p).node).page();
                // SAFETY: the row belongs to the page.
                let cells = unsafe { &mut *page.get_cells(cur_row) };
                s.clear_cells(page, cur_row, &mut cells[left..right + 1]);

                // With a full-width scroll region the entire row is a
                // fresh blank row: reset the metadata so nothing (wrap
                // state, semantic prompt) is retained from the row whose
                // storage it recycles. With left/right margins the row
                // keeps content outside the margins so the metadata is
                // preserved, matching the shift case above.
                if !left_right {
                    cur_row.reset();
                }
            }

            // Mark the row as dirty
            tp(cur_p).mark_dirty();

            // We have successfully processed a line.
            y -= 1;
            // Move our pin up to the next row.
            if let Some(p) = tp(cur_p).up(1) {
                *tp(cur_p) = p;
            }
        }

        s.pages.untrack_pin(cur_p);
        s.cursor_absolute(self.scrolling_region.left, start_y);

        // Always unset pending wrap
        s.cursor.pending_wrap = false;
    }

    /// Removes amount lines from the current cursor row down. The remaining lines
    /// to the bottom margin are shifted up and space from the bottom margin up is
    /// filled with empty lines.
    ///
    /// If the current cursor position is outside of the current scroll region it
    /// does nothing. If amount is greater than the remaining number of lines in the
    /// scrolling region it is adjusted down.
    ///
    /// In left and right margin mode the margins are respected; lines are only
    /// scrolled in the scroll region.
    ///
    /// If the cell movement splits a multi cell character that character cleared,
    /// by replacing it by spaces, keeping its current attributes. All other
    /// cleared space is colored according to the current SGR state.
    ///
    /// Moves the cursor to the left margin.
    pub fn delete_lines(&mut self, count: usize) {
        // Rare, but happens
        if count == 0 {
            return;
        }

        let mut s = self.screens.active;

        // If the cursor is outside the scroll region we do nothing.
        if s.cursor.y < self.scrolling_region.top
            || s.cursor.y > self.scrolling_region.bottom
            || s.cursor.x < self.scrolling_region.left
            || s.cursor.x > self.scrolling_region.right
        {
            return;
        }

        if build_options::KITTY_GRAPHICS {
            // Scrolling dirties the images because it updates their placements pins.
            s.kitty_images.mark_dirty();
        }

        // At the end we need to return the cursor to the row it started on.
        let start_y = s.cursor.y;

        // We have a slower path if we have left or right scroll margins.
        let left_right = self.scrolling_region.left > 0 || self.scrolling_region.right < self.cols - 1;

        // Remaining rows from our cursor to the bottom of the scroll region.
        let rem = (self.scrolling_region.bottom - s.cursor.y) as usize + 1;

        // We can only insert lines up to our remaining lines in the scroll
        // region. So we take whichever is smaller.
        let adjusted_count = count.min(rem);

        // Create a new tracked pin which we'll use to navigate the page list
        // so that if we need to adjust capacity it will be properly tracked.
        let start_pin = *tp(s.cursor.page_pin);
        let cur_p = match s.pages.track_pin(start_pin) {
            Ok(p) => p,
            Err(err) => {
                // See insertLines
                log::error!(target: LOG_TARGET, "deleteLines trackPin error err={err:?}");
                panic!("deleteLines trackPin OOM");
            }
        };

        // Partial-width margins edit cells in stable rows; full-width moves rows.
        if !left_right {
            let first = tp(cur_p).node;
            let last = tp(cur_p).down(rem - 1).unwrap().node;
            self.invalidate_full_width_row_range(first, last);
        }

        let left = self.scrolling_region.left as usize;
        let right = self.scrolling_region.right as usize;

        // Our current y position relative to the cursor
        let mut y: usize = 0;

        // Traverse from the top down
        while y < rem {
            let cur_rac = tp(cur_p).row_and_cell();
            let cur_row: &mut Row = rw(cur_rac.row);

            // If this is one of the lines we need to shift, do so
            if y < rem - adjusted_count {
                let off_p = tp(cur_p).down(adjusted_count).unwrap();
                let off_rac = off_p.row_and_cell();
                let off_row: &mut Row = rw(off_rac.row);

                self.row_will_be_shifted(nd(tp(cur_p).node).page(), cur_row);
                self.row_will_be_shifted(nd(off_p.node).page(), off_row);

                // If our scrolling region is full width, then we unset wrap.
                if !left_right {
                    off_row.set_wrap(false);
                    cur_row.set_wrap(false);
                    off_row.set_wrap_continuation(false);
                    cur_row.set_wrap_continuation(false);
                }

                let src_p = off_p;
                let src_row = off_row;
                let dst_p = *tp(cur_p);
                let dst_row = cur_row;

                // If our page doesn't match, then we need to do a copy from
                // one page to another. This is the slow path.
                if src_p.node != dst_p.node {
                    // The copy may replace the destination node in order
                    // to increase its capacity. Our pins are tracked so
                    // they update automatically; we can discard the
                    // replacement because the remainder of this iteration
                    // only accesses rows through the pins.
                    let _ = s.clone_partial_row_grow_capacity(
                        dst_p.node,
                        dst_p.y as usize,
                        nd(src_p.node).page() as *const Page,
                        src_row as *const Row,
                        left,
                        right + 1,
                    );
                } else if !left_right {
                    // Swap the src/dst cells. This ensures that our dst gets the
                    // proper shifted rows and src gets non-garbage cell data that
                    // we can clear.
                    std::mem::swap(dst_row, src_row);

                    // Ensure what we did didn't corrupt the page
                    nd(tp(cur_p).node).page().assert_integrity();
                } else {
                    // Left/right scroll margins we have to
                    // copy cells, which is much slower...
                    let page = nd(tp(cur_p).node).page();
                    page.move_cells(src_row, left, dst_row, left, (right - left) + 1);
                }
            } else {
                // Clear the cells for this row, it's from out of bounds.
                self.row_will_be_shifted(nd(tp(cur_p).node).page(), cur_row);
                let page = nd(tp(cur_p).node).page();
                // SAFETY: the row belongs to the page.
                let cells = unsafe { &mut *page.get_cells(cur_row) };
                s.clear_cells(page, cur_row, &mut cells[left..right + 1]);

                // With a full-width scroll region the entire row is a
                // fresh blank row: reset the metadata so nothing (wrap
                // state, semantic prompt) is retained from the row whose
                // storage it recycles. With left/right margins the row
                // keeps content outside the margins so the metadata is
                // preserved, matching the shift case above.
                if !left_right {
                    cur_row.reset();
                }
            }

            // Mark the row as dirty
            tp(cur_p).mark_dirty();

            // We have successfully processed a line.
            y += 1;
            // Move our pin down to the next row.
            if let Some(p) = tp(cur_p).down(1) {
                *tp(cur_p) = p;
            }
        }

        s.pages.untrack_pin(cur_p);
        s.cursor_absolute(self.scrolling_region.left, start_y);
        // Always unset pending wrap
        s.cursor.pending_wrap = false;
    }

    /// Inserts spaces at current cursor position moving existing cell contents
    /// to the right. The contents of the count right-most columns in the scroll
    /// region are lost. The cursor position is not changed.
    ///
    /// This unsets the pending wrap state without wrapping.
    ///
    /// The inserted cells are colored according to the current SGR state.
    pub fn insert_blanks(&mut self, count: usize) {
        let mut s = self.screens.active;

        // Unset pending wrap state without wrapping. Note: this purposely
        // happens BEFORE the scroll region check below, because that's what
        // xterm does.
        s.cursor.pending_wrap = false;

        // If we're given a zero then we do nothing. The rest of this function
        // assumes count > 0 and will crash if zero so return early. Note that
        // this shouldn't be possible with real CSI sequences because the value
        // is clamped to 1 min.
        if count == 0 {
            return;
        }

        // If our cursor is outside the margins then do nothing. We DO reset
        // wrap state still so this must remain below the above logic.
        if s.cursor.x < self.scrolling_region.left || s.cursor.x > self.scrolling_region.right {
            return;
        }

        // If our count is larger than the remaining amount, we just erase right.
        // We only do this if we can erase the entire line (no right margin).
        // if (right_limit == self.cols and
        //     count > right_limit - self.screens.active.cursor.x)
        // {
        //     self.eraseLine(.right, false);
        //     return;
        // }

        // left is just the cursor position but as a multi-pointer
        let left: *mut Cell = s.cursor.page_cell;
        let page = cursor_page(s);
        let row = rw(s.cursor.page_row);

        // If our X is a wide spacer tail then we need to erase the
        // previous cell too so we don't split a multi-cell character.
        if cl(left).wide() == Wide::SpacerTail {
            debug_assert!(s.cursor.x > 0);
            s.clear_cells(page, row, cells_mut(left.wrapping_sub(1), 2));
        }

        // Remaining cols from our cursor to the right margin.
        let rem = (self.scrolling_region.right - s.cursor.x) as usize + 1;

        // If the cell at the right margin is wide, its spacer tail is
        // outside the scroll region and would be orphaned by either the
        // shift or the clear. Clean up both halves up front.
        {
            let right_cell = left.wrapping_add(rem - 1);
            if cl(right_cell).wide() == Wide::Wide {
                s.clear_cells(page, row, cells_mut(right_cell, 2));
            }
        }

        // We can only insert blanks up to our remaining cols
        let adjusted_count = count.min(rem);

        // This is the amount of space at the right of the scroll region
        // that will NOT be blank, so we need to shift the correct cols right.
        // "scroll_amount" is the number of such cols.
        let scroll_amount = rem - adjusted_count;
        if scroll_amount > 0 {
            page.pause_integrity_checks(true);

            let x = left.wrapping_add(scroll_amount - 1);

            // If our last cell we're shifting is wide, then we need to clear
            // it to be empty so we don't split the multi-cell char.
            let end = x;
            if cl(end).wide() == Wide::Wide {
                debug_assert!(cl(end.wrapping_add(1)).wide() == Wide::SpacerTail);
                s.clear_cells(page, row, cells_mut(end, 2));
            }

            // We work backwards so we don't overwrite data.
            for i in (0..scroll_amount).rev() {
                let src = cl(left.wrapping_add(i));
                let dst = cl(left.wrapping_add(i + adjusted_count));
                page.swap_cells(src, dst);
            }

            page.pause_integrity_checks(false);
        }

        // Insert blanks. The blanks preserve the background color.
        s.clear_cells(page, row, cells_mut(left, adjusted_count));

        // Our row is always dirty
        s.cursor_mark_dirty();
    }

    /// Removes amount characters from the current cursor position to the right.
    /// The remaining characters are shifted to the left and space from the right
    /// margin is filled with spaces.
    ///
    /// If amount is greater than the remaining number of characters in the
    /// scrolling region, it is adjusted down.
    ///
    /// Does not change the cursor position.
    pub fn delete_chars(&mut self, count_req: usize) {
        if count_req == 0 {
            return;
        }

        let mut s = self.screens.active;

        // If our cursor is outside the margins then do nothing. We DO reset
        // wrap state still so this must remain below the above logic.
        if s.cursor.x < self.scrolling_region.left || s.cursor.x > self.scrolling_region.right {
            return;
        }

        // Remaining cols from our cursor to the right margin.
        let rem = (self.scrolling_region.right - s.cursor.x) as usize + 1;

        // We can only insert blanks up to our remaining cols
        let count = count_req.min(rem);

        let x0 = s.cursor.x;
        s.split_cell_boundary(x0);
        s.split_cell_boundary(x0 + count as CellCountInt);
        s.split_cell_boundary(self.scrolling_region.right + 1);

        // left is just the cursor position but as a multi-pointer
        let left: *mut Cell = s.cursor.page_cell;
        let page = cursor_page(s);
        let row = rw(s.cursor.page_row);

        // This is the amount of space at the right of the scroll region
        // that will NOT be blank, so we need to shift the correct cols right.
        // "scroll_amount" is the number of such cols.
        let scroll_amount = rem - count;
        let mut x: usize = 0;
        if scroll_amount > 0 {
            page.pause_integrity_checks(true);

            while x < scroll_amount {
                let src = cl(left.wrapping_add(x + count));
                let dst = cl(left.wrapping_add(x));
                page.swap_cells(src, dst);
                x += 1;
            }

            page.pause_integrity_checks(false);
        }

        // Insert blanks. The blanks preserve the background color.
        s.clear_cells(page, row, cells_mut(left.wrapping_add(x), rem - scroll_amount));

        // Our row's soft-wrap is always reset.
        s.cursor_reset_wrap();

        // Our row is always dirty
        s.cursor_mark_dirty();
    }

    /// DECIC - Insert Column. Inserts count blank columns at the cursor
    /// column in every row of the scrolling region, moving existing cell
    /// contents to the right. The contents of the count right-most columns
    /// in the scroll region are lost. The cursor position is not changed.
    ///
    /// This does nothing if the cursor is outside the scrolling region.
    pub fn insert_columns(&mut self, count: usize) {
        if !self.cursor_in_scrolling_region() {
            return;
        }

        let old_x = self.screens.active.cursor.x;
        let old_y = self.screens.active.cursor.y;

        let mut y = self.scrolling_region.top;
        while y <= self.scrolling_region.bottom {
            self.screens.active.cursor_absolute(old_x, y);
            self.insert_blanks(count);
            y += 1;
        }

        self.screens.active.cursor_absolute(old_x, old_y);
    }

    /// DECDC - Delete Column. Deletes count columns starting at the cursor
    /// column in every row of the scrolling region, moving the remaining
    /// cell contents to the left and filling from the right margin with
    /// blanks. The cursor position is not changed.
    ///
    /// This does nothing if the cursor is outside the scrolling region.
    pub fn delete_columns(&mut self, count: usize) {
        if !self.cursor_in_scrolling_region() {
            return;
        }

        let old_x = self.screens.active.cursor.x;
        let old_y = self.screens.active.cursor.y;

        let mut y = self.scrolling_region.top;
        while y <= self.scrolling_region.bottom {
            self.screens.active.cursor_absolute(old_x, y);
            self.delete_chars(count);
            y += 1;
        }

        self.screens.active.cursor_absolute(old_x, old_y);
    }

    /// DECBI - Back Index. If the cursor is on the left margin, the contents
    /// of the scrolling region are shifted one column right and a blank
    /// column is inserted at the left margin. Otherwise the cursor moves one
    /// column left, unless it is already at the left edge of the screen.
    ///
    /// This unsets the pending wrap state without wrapping.
    pub fn back_index(&mut self) {
        self.screens.active.cursor.pending_wrap = false;

        if self.screens.active.cursor.x == self.scrolling_region.left {
            self.insert_columns(1);
            return;
        }

        if self.screens.active.cursor.x > 0 {
            self.screens.active.cursor_left(1);
        }
    }

    /// DECFI - Forward Index. If the cursor is on the right margin, the
    /// contents of the scrolling region are shifted one column left, losing
    /// the column at the left margin, and a blank column is inserted at the
    /// right margin. Otherwise the cursor
    /// moves one column right, unless it is already at the right edge of
    /// the screen.
    ///
    /// This unsets the pending wrap state without wrapping.
    pub fn forward_index(&mut self) {
        self.screens.active.cursor.pending_wrap = false;

        if self.screens.active.cursor.x == self.scrolling_region.right {
            // Scrolling the region left means deleting its left-most column,
            // not the one under the cursor.
            let old_x = self.screens.active.cursor.x;
            let old_y = self.screens.active.cursor.y;
            let left = self.scrolling_region.left;
            self.screens.active.cursor_absolute(left, old_y);
            self.delete_columns(1);
            self.screens.active.cursor_absolute(old_x, old_y);
            return;
        }

        if self.screens.active.cursor.x < self.cols - 1 {
            self.screens.active.cursor_right(1);
        }
    }

    /// Returns true if the cursor is inside the scrolling region, including
    /// the left and right margins.
    fn cursor_in_scrolling_region(&self) -> bool {
        let cursor = &self.screens.active.cursor;
        cursor.x >= self.scrolling_region.left
            && cursor.x <= self.scrolling_region.right
            && cursor.y >= self.scrolling_region.top
            && cursor.y <= self.scrolling_region.bottom
    }

    pub fn erase_chars(&mut self, count_req: usize) {
        let mut s = self.screens.active;
        let count = {
            let remaining = (self.cols - s.cursor.x) as usize;
            let mut end = remaining.min(count_req.max(1));

            // If our last cell is a wide char then we need to also clear the
            // cell beyond it since we can't just split a wide char.
            if end != remaining {
                let last = cl(s.cursor_cell_right((end - 1) as CellCountInt));
                if last.wide() == Wide::Wide {
                    end += 1;
                }
            }

            end
        };

        // Handle any boundary conditions on the edges of the erased area.
        //
        // TODO(qwerasd): This isn't actually correct if you take in to account
        // protected modes. We need to figure out how to make `clearCells` or at
        // least `clearUnprotectedCells` handle boundary conditions...
        let x = s.cursor.x;
        s.split_cell_boundary(x);
        s.split_cell_boundary(x + count as CellCountInt);

        // Reset our row's soft-wrap.
        s.cursor_reset_wrap();

        // Mark our cursor row as dirty
        s.cursor_mark_dirty();

        // Clear the cells
        let cells: *mut Cell = s.cursor.page_cell;

        // If we never had a protection mode, then we can assume no cells
        // are protected and go with the fast path. If the last protection
        // mode was not ISO we also always ignore protection attributes.
        if s.protected_mode != ansi::ProtectedMode::Iso {
            clear_cursor_row_cells(s, cells_mut(cells, count));
            return;
        }

        clear_cursor_row_unprotected_cells(s, cells_mut(cells, count));
    }

    /// Erase the line.
    pub fn erase_line(&mut self, mode: csi::EraseLine, protected_req: bool) {
        let mut s = self.screens.active;

        // Get our start/end positions depending on mode.
        let (start, end): (usize, usize) = if mode == csi::EraseLine::RIGHT {
            let mut x = s.cursor.x;

            // If our X is a wide spacer tail then we need to erase the
            // previous cell too so we don't split a multi-cell character.
            if x > 0 && cl(s.cursor.page_cell).wide() == Wide::SpacerTail {
                x -= 1;
            }

            // Reset our row's soft-wrap.
            s.cursor_reset_wrap();

            (x as usize, self.cols as usize)
        } else if mode == csi::EraseLine::LEFT {
            let mut x = s.cursor.x;

            // If our x is a wide char we need to delete the tail too.
            if cl(s.cursor.page_cell).wide() == Wide::Wide {
                x += 1;
            }

            (0, x as usize + 1)
        } else if mode == csi::EraseLine::COMPLETE {
            // Xterm preserves this flag for EL2, but it also doesn't reflow
            // rows when resizing. Since we do, the erased row must no longer
            // continue onto the next row.
            s.cursor_reset_wrap();

            (0, self.cols as usize)
        } else {
            log::error!(target: LOG_TARGET, "unimplemented erase line mode: {mode:?}");
            return;
        };

        // All modes will clear the pending wrap state and we know we have
        // a valid mode at this point.
        s.cursor.pending_wrap = false;

        // We always mark our row as dirty
        s.cursor_mark_dirty();

        // Start of our cells
        let cells: *mut Cell = s.cursor.page_cell.wrapping_sub(s.cursor.x as usize);
        let range = cells_mut(cells.wrapping_add(start), end - start);

        // We respect protected attributes if explicitly requested (probably
        // a DECSEL sequence) or if our last protected mode was ISO even if its
        // not currently set.
        let protected = s.protected_mode == ansi::ProtectedMode::Iso || protected_req;

        // If we're not respecting protected attributes, we can use a fast-path
        // to fill the entire line.
        if !protected {
            clear_cursor_row_cells(s, range);
            return;
        }

        clear_cursor_row_unprotected_cells(s, range);
    }

    /// Erase the display.
    pub fn erase_display(&mut self, mode: csi::EraseDisplay, protected_req: bool) {
        let mut s = self.screens.active;

        // We respect protected attributes if explicitly requested (probably
        // a DECSEL sequence) or if our last protected mode was ISO even if its
        // not currently set.
        let protected = s.protected_mode == ansi::ProtectedMode::Iso || protected_req;

        match mode {
            csi::EraseDisplay::ScrollComplete => {
                if let Err(err) = s.scroll_clear() {
                    log::warn!(target: LOG_TARGET, "scroll clear failed, doing a normal clear err={err:?}");
                    self.erase_display(csi::EraseDisplay::Complete, protected_req);
                    return;
                }

                // Unsets pending wrap state
                s.cursor.pending_wrap = false;

                if build_options::KITTY_GRAPHICS {
                    // Clear only placements still visible after moving the active
                    // area into scrollback.
                    // TODO(port-dedupe): kitty_images.clearScreen is pending the
                    // graphics_storage port; without stored placements it is a no-op.
                }
            }

            csi::EraseDisplay::Complete => {
                // If we're on the primary screen and our last non-empty row is
                // a prompt, then we do a scroll_complete instead. This is a
                // heuristic to get the generally desirable behavior that ^L
                // at a prompt scrolls the screen contents prior to clearing.
                // Most shells send `ESC [ H ESC [ 2 J` so we can't just check
                // our current cursor position. See #905
                if self.screens.active_key == ScreenKey::Primary {
                    'at_prompt: {
                        // Go from the bottom of the active up and see if we're
                        // at a prompt.
                        let Some(active_br) = s.pages.get_bottom_right(point::Tag::Active) else {
                            break 'at_prompt;
                        };
                        let it = active_br.row_iterator(Direction::LeftUp, Some(s.pages.get_top_left(point::Tag::Active)));
                        let mut found = false;
                        for p in it {
                            let row = rw(p.row_and_cell().row);
                            match row.semantic_prompt() {
                                // If we're at a prompt or input area, then we are at a prompt.
                                RowSemanticPrompt::Prompt | RowSemanticPrompt::PromptContinuation => {
                                    found = true;
                                    break;
                                }

                                RowSemanticPrompt::None => {
                                    // Blank rows below the prompt tell us nothing,
                                    // keep searching for the last non-empty row.
                                    // SAFETY: valid pin.
                                    if !Cell::has_text_any(unsafe { &*p.cells(CellSubset::All) }) {
                                        continue;
                                    }

                                    // If we have command output, then we're most
                                    // certainly not at a prompt.
                                    break 'at_prompt;
                                }
                            }
                        }
                        if !found {
                            break 'at_prompt;
                        }

                        // If we fail, we just fall back to doing a normal clear
                        // so we don't worry about the error.
                        let _ = s.scroll_clear();
                    }
                }

                // All active area
                s.clear_rows(Point::Active(Coordinate::default()), None, protected);

                // Unsets pending wrap state
                s.cursor.pending_wrap = false;

                if build_options::KITTY_GRAPHICS {
                    // ED2 clears visible placements but preserves graphics that
                    // are wholly in scrollback.
                    // TODO(port-dedupe): kitty_images.clearScreen is pending the
                    // graphics_storage port; without stored placements it is a no-op.
                }

                // Cleared screen dirty bit
                self.flags.dirty.clear = true;
            }

            csi::EraseDisplay::Below => {
                // All lines to the right (including the cursor)
                self.erase_line(csi::EraseLine::RIGHT, protected_req);

                // All lines below
                if s.cursor.y + 1 < self.rows {
                    let y = s.cursor.y as u32 + 1;
                    s.clear_rows(Point::Active(Coordinate { x: 0, y }), None, protected);
                }

                // Unsets pending wrap state. Should be done by eraseLine.
                debug_assert!(!s.cursor.pending_wrap);
            }

            csi::EraseDisplay::Above => {
                // Erase to the left (including the cursor)
                self.erase_line(csi::EraseLine::LEFT, protected_req);

                // All lines above
                if s.cursor.y > 0 {
                    let y = s.cursor.y as u32 - 1;
                    s.clear_rows(
                        Point::Active(Coordinate { x: 0, y: 0 }),
                        Some(Point::Active(Coordinate { x: 0, y })),
                        protected,
                    );
                }

                // Unsets pending wrap state
                debug_assert!(!s.cursor.pending_wrap);
            }

            csi::EraseDisplay::Scrollback => s.erase_history(None),
        }
    }

    /// Resets all margins and fills the whole screen with the character 'E'
    ///
    /// Sets the cursor to the top left corner.
    pub fn decaln(&mut self) -> Result<(), IncreaseCapacityError> {
        let mut s = self.screens.active;

        // Clear our stylistic attributes. This is the only thing that can
        // fail so we do it first so we can undo it.
        let old_style = s.cursor.style;
        s.cursor.style = Style {
            bg_color: s.cursor.style.bg_color,
            fg_color: s.cursor.style.fg_color,
            ..Default::default()
        };
        if let Err(e) = s.manual_style_update() {
            s.cursor.style = old_style;
            return Err(e);
        }

        // Reset margins, also sets cursor to top-left
        self.scrolling_region = ScrollingRegion {
            top: 0,
            bottom: self.rows - 1,
            left: 0,
            right: self.cols - 1,
        };

        // Origin mode is disabled
        self.modes.set(Mode::Origin, false);

        // Move our cursor to the top-left
        self.set_cursor_pos(1, 1);

        // Use clearRows instead of eraseDisplay because we must NOT respect
        // protected attributes here.
        s.clear_rows(Point::Active(Coordinate::default()), None, false);

        // Fill with Es by moving the cursor but reset it after.
        loop {
            let page = cursor_page(s);
            let row = rw(s.cursor.page_row);
            // SAFETY: the row belongs to the page.
            let cells_multi = unsafe { &mut *page.get_cells(row) };
            let cells = &mut cells_multi[..page.size.cols as usize];
            let mut fill = Cell::init('E' as u32);
            fill.set_style_id(s.cursor.style_id);
            // DECALN does not respect protected state. Verified with xterm.
            fill.set_protected(false);
            cells.fill(fill);

            // If we have a ref-counted style, increase
            if s.cursor.style_id != style::DEFAULT_ID {
                // SAFETY: the style is in this page's set.
                unsafe { page.styles.use_multiple(page.memory, s.cursor.style_id, cells.len() as _) };
                row.set_styled(true);
            }

            // We messed with the page so assert its integrity here.
            page.assert_integrity();

            s.cursor_mark_dirty();
            if s.cursor.y == self.rows - 1 {
                break;
            }
            s.cursor_down(1);
        }

        // Reset the cursor to the top-left
        self.set_cursor_pos(1, 1);
        Ok(())
    }

    // TODO(port-dedupe): `kittyGraphics(io, alloc, cmd)` wraps
    // `kitty.graphics.execute(io, alloc, self, cmd)`, which is pending the
    // `kitty/graphics_exec` port; that port provides it on `Terminal`.

    /// Execute a Glyph Protocol APC command against this terminal's per-session
    /// glossary. The returned response, if any, should be sent back to the pty as
    /// a complete APC sequence via `Response.formatWire`.
    pub fn glyph_protocol(&mut self, req: &glyph::Request) -> Option<glyph::Response<'static>> {
        let resp = glyph::execute(&mut self.glyph_glossary, req);
        match req {
            glyph::Request::Register(_) | glyph::Request::Clear(_) => self.flags.dirty.glyph_glossary = true,
            glyph::Request::Support | glyph::Request::Query(_) => {}
        }
        resp
    }

    /// Set the storage size limit for Kitty graphics across all screens.
    pub fn set_kitty_graphics_size_limit(&mut self, limit: usize) {
        if !build_options::KITTY_GRAPHICS {
            return;
        }
        for (_, mut ptr) in self.screens.iter() {
            let screen: &mut Screen = &mut ptr;
            screen.kitty_images.set_limit(&mut screen.pages, limit);
        }
    }

    /// Set the allowed medium types for Kitty graphics image loading
    /// across all screens.
    pub fn set_kitty_graphics_loading_limits(&mut self, limits: LoadingImageLimits) {
        if !build_options::KITTY_GRAPHICS {
            return;
        }
        for (_, mut screen) in self.screens.iter() {
            screen.kitty_images.image_limits = limits.clone();
        }
    }

    /// Set a style attribute.
    pub fn set_attribute(&mut self, attr: sgr::Attribute<'_>) -> Result<(), IncreaseCapacityError> {
        self.screens.active.set_attribute(attr)
    }

    /// Print the active attributes as a string. This is used to respond to DECRQSS
    /// requests.
    ///
    /// Boolean attributes are printed first, followed by foreground color, then
    /// background color. Each attribute is separated by a semicolon.
    pub fn print_attributes<'b>(&self, buf: &'b mut [u8]) -> std::io::Result<&'b [u8]> {
        let mut writer = std::io::Cursor::new(&mut *buf);

        // The SGR response always starts with a 0. See https://vt100.net/docs/vt510-rm/DECRPSS
        writer.write_all(b"0")?;

        let pen = self.screens.active.cursor.style;
        let mut attrs = [0u8; 9];
        let mut i: usize = 0;

        if pen.flags.bold {
            attrs[i] = 1;
            i += 1;
        }

        if pen.flags.faint {
            attrs[i] = 2;
            i += 1;
        }

        if pen.flags.italic {
            attrs[i] = 3;
            i += 1;
        }

        if pen.flags.underline != sgr::Underline::None {
            attrs[i] = 4;
            i += 1;
        }

        if pen.flags.overline {
            attrs[i] = 53;
            i += 1;
        }

        if pen.flags.blink {
            attrs[i] = 5;
            i += 1;
        }

        if pen.flags.inverse {
            attrs[i] = 7;
            i += 1;
        }

        if pen.flags.invisible {
            attrs[i] = 8;
            i += 1;
        }

        if pen.flags.strikethrough {
            attrs[i] = 9;
            i += 1;
        }

        for &attr in &attrs[..i] {
            // Preserve underline styles. Kind of a hack to special case 4
            // here but its easier than changing how we do all attributes.
            if attr == 4 && pen.flags.underline != sgr::Underline::Single {
                write!(writer, ";4:{}", pen.flags.underline as u8)?;
                continue;
            }

            write!(writer, ";{}", attr)?;
        }

        match pen.fg_color {
            style::Color::None => {}
            style::Color::Palette(idx) => {
                if idx >= 16 {
                    write!(writer, ";38:5:{}", idx)?;
                } else if idx >= 8 {
                    write!(writer, ";9{}", idx - 8)?;
                } else {
                    write!(writer, ";3{}", idx)?;
                }
            }
            style::Color::Rgb(rgb) => write!(writer, ";38:2::{}:{}:{}", rgb.r, rgb.g, rgb.b)?,
        }

        match pen.bg_color {
            style::Color::None => {}
            style::Color::Palette(idx) => {
                if idx >= 16 {
                    write!(writer, ";48:5:{}", idx)?;
                } else if idx >= 8 {
                    write!(writer, ";10{}", idx - 8)?;
                } else {
                    write!(writer, ";4{}", idx)?;
                }
            }
            style::Color::Rgb(rgb) => write!(writer, ";48:2::{}:{}:{}", rgb.r, rgb.g, rgb.b)?,
        }

        let n = writer.position() as usize;
        Ok(&buf[..n])
    }

    /// DECCOLM changes the terminal width between 80 and 132 columns. This
    /// function call will do NOTHING unless `setDeccolmSupported` has been
    /// called with "true".
    ///
    /// This breaks the expectation around modern terminals that they resize
    /// with the window. This will fix the grid at either 80 or 132 columns.
    /// The rows will continue to be variable.
    pub fn deccolm(&mut self, mode: DeccolmMode) -> Result<(), ResizeError> {
        // If DEC mode 40 isn't enabled, then this is ignored. We also make
        // sure that we don't have deccolm set because we want to fully ignore
        // set mode.
        if !self.modes.get(Mode::EnableMode3) {
            self.modes.set(Mode::Column132, false);
            return Ok(());
        }

        // Enable it
        self.modes.set(Mode::Column132, mode == DeccolmMode::Cols132);

        // Resize to the requested size
        self.resize(Resize::new(
            match mode {
                DeccolmMode::Cols132 => 132,
                DeccolmMode::Cols80 => 80,
            },
            self.rows,
        ))?;

        // Erase our display and move our cursor.
        self.erase_display(csi::EraseDisplay::Complete, false);
        self.set_cursor_pos(1, 1);
        Ok(())
    }

    /// Resize the underlying terminal.
    ///
    /// This has follow-on impacts:
    ///
    ///   - If the column count changes, tabstops are reset.
    ///   - The scroll region is always reset
    ///   - Synchronized output mode is reset for every successful resize.
    ///
    /// This handles errors gracefully and recovers the terminal back to
    /// a clean usable state.
    ///
    /// The only error handling edge case is in the highly exceptional scenario
    /// where the primary screen can be resized but the alternate screen cannot.
    /// In this scenario, we attempt to clear the alt screen at the desired
    /// new size. If that fails, we unconditionally deallocate the alt screen
    /// and move to primary screen. This can break terminal programs but it
    /// requires a really particular scenario where memory exists for one
    /// but not the other and we do our best.
    pub fn resize(&mut self, opts: Resize) -> Result<(), ResizeError> {
        // Screen and scrolling-region invariants require non-zero dimensions.
        // Validate before changing any terminal state.
        if opts.cols == 0 || opts.rows == 0 {
            return Err(ResizeError::InvalidValue);
        }

        // Pixel geometry and synchronized output are updated on every valid
        // resize attempt, including one that doesn't change the grid dimensions.
        // Save their old values so later allocation failures can roll them back.
        let old_width_px = self.width_px;
        let old_height_px = self.height_px;
        let old_synchronized_output = self.modes.get(Mode::SynchronizedOutput);

        let r = self.resize_(opts);
        if r.is_err() {
            self.width_px = old_width_px;
            self.height_px = old_height_px;
            self.modes.set(Mode::SynchronizedOutput, old_synchronized_output);
        }
        r
    }

    fn resize_(&mut self, opts: Resize) -> Result<(), ResizeError> {
        let tw = &RESIZE_TW;

        // If our pixel geometry was set, then we set it even if our rows/cols
        // didn't change.
        if let Some(cell_size) = opts.cell_size_px {
            self.width_px = (opts.cols as u32).checked_mul(cell_size.width).unwrap_or(u32::MAX);
            self.height_px = (opts.rows as u32).checked_mul(cell_size.height).unwrap_or(u32::MAX);
        }

        self.modes.set(Mode::SynchronizedOutput, false);

        // If our cols/rows didn't change, skip grid work but still apply pixels.
        if self.cols == opts.cols && self.rows == opts.rows {
            return Ok(());
        }

        // Build replacement tabstops without touching the current table. Keep
        // ownership here until every fallible resize operation has succeeded.
        let mut new_tabstops: Option<Tabstops> = None;
        if self.cols != opts.cols {
            tw.check(ResizeFailPoint::Tabstops)?;
            new_tabstops = Some(Tabstops::init(opts.cols as usize, TABSTOP_INTERVAL)?);
        }

        // Resize primary screen, which supports reflow. We do this first
        // because the cleanup situation is a lot better if this succeeds
        // and alt fails than the reverse.
        tw.check(ResizeFailPoint::PrimaryScreen)?;
        let mut primary = self.screens.get(ScreenKey::Primary).unwrap();
        primary.resize(ScreenResize {
            cols: opts.cols,
            rows: opts.rows,
            reflow: self.modes.get(Mode::Wraparound),
            prompt_redraw: self.flags.shell_redraws_prompt,
            pull_scrollback: self.flags.resize_pull_scrollback,
        })?;

        // Alternate screen, if it exists, doesn't reflow. The primary resize
        // above can't be losslessly undone, so if the alternate resize fails we
        // replace it with an empty screen at the requested size. If that
        // also fails, we fall back to the primary screen.
        if let Some(mut alt) = self.screens.get(ScreenKey::Alternate) {
            'alt: {
                let err: ResizeError = 'resize: {
                    if let Err(err) = tw.check(ResizeFailPoint::AlternateScreen) {
                        break 'resize err;
                    }
                    if let Err(err) = alt.resize(ScreenResize {
                        cols: opts.cols,
                        rows: opts.rows,
                        reflow: false,
                        pull_scrollback: self.flags.resize_pull_scrollback,
                        ..Default::default()
                    }) {
                        break 'resize err.into();
                    }

                    // Resize succeeded.
                    break 'alt;
                };

                log::warn!(target: LOG_TARGET, "alternate screen resize failed, replacing it err={err:?}");

                // If the alternate screen isn't active, then we just free it
                // and move on. It'll be reallocated when it gets reinitialized lazily.
                // In this case, we just lose the prior data if the terminal program
                // expected it to be saved.
                if self.screens.active_key != ScreenKey::Alternate {
                    self.screens.remove(ScreenKey::Alternate);
                    break 'alt;
                }

                // The alt screen is active, so we temporarily switch to primary
                // so we can safely remove the alt and recreate it blank. This loses
                // the data but hopefully keeps us on the alt screen.
                let charset = alt.charset;
                self.screens.switch_to(ScreenKey::Primary);
                self.screens.remove(ScreenKey::Alternate);

                // Replace the alt screen with an empty version. If this fails
                // we just go back to the primary screen. Not great, but best
                // we can do.
                if tw.check(ResizeFailPoint::AlternateScreenInit).is_err() {
                    break 'alt;
                }
                let replacement = self.screens.get_init(
                    ScreenKey::Alternate,
                    screen::Options {
                        cols: opts.cols,
                        rows: opts.rows,
                        max_scrollback_bytes: Some(0),
                        max_scrollback_lines: None,
                        kitty_image_storage_limit: if build_options::KITTY_GRAPHICS {
                            primary.kitty_images.total_limit
                        } else {
                            0
                        },
                        kitty_image_loading_limits: primary.kitty_images.image_limits.clone(),
                    },
                );
                let mut replacement = match replacement {
                    Ok(r) => r,
                    Err(init_err) => {
                        log::warn!(
                            target: LOG_TARGET,
                            "alternate screen replacement failed, falling back to primary err={init_err:?}"
                        );
                        break 'alt;
                    }
                };

                replacement.charset = charset;
                self.screens.switch_to(ScreenKey::Alternate);
            }
        }

        // No more failures are allowed after this point because the screens have
        // committed their new sizes and the remaining Terminal state must follow.

        // All fallible work is complete. Replace the old tabstop table only now.
        if let Some(v) = new_tabstops.take() {
            self.tabstops = v;
        }

        // Whenever we resize we just mark it as a screen clear
        self.flags.dirty.clear = true;

        // Set our size
        self.cols = opts.cols;
        self.rows = opts.rows;

        // Reset the scrolling region
        self.scrolling_region = ScrollingRegion {
            top: 0,
            bottom: opts.rows - 1,
            left: 0,
            right: opts.cols - 1,
        };
        Ok(())
    }

    /// Set the pwd for the terminal.
    pub fn set_pwd(&mut self, pwd: &[u8]) -> Result<(), AllocError> {
        set_sentinel_string(&mut self.pwd, pwd)
    }

    /// Returns the pwd for the terminal, if any. The memory is owned by the
    /// Terminal and is not copied. It is safe until a reset or setPwd.
    pub fn get_pwd(&self) -> Option<&[u8]> {
        get_sentinel_string(&self.pwd)
    }

    /// Like [`Terminal::get_pwd`] but includes the NUL sentinel (Zig `[:0]const u8`).
    pub fn get_pwd_z(&self) -> Option<&CStr> {
        get_sentinel_cstr(&self.pwd)
    }

    /// Set the title for the terminal, as set by escape sequences (e.g. OSC 0/2).
    pub fn set_title(&mut self, t: &[u8]) -> Result<(), AllocError> {
        set_sentinel_string(&mut self.title, t)
    }

    /// Returns the title for the terminal, if any. The memory is owned by the
    /// Terminal and is not copied. It is safe until a reset or setTitle.
    pub fn get_title(&self) -> Option<&[u8]> {
        get_sentinel_string(&self.title)
    }

    /// Like [`Terminal::get_title`] but includes the NUL sentinel (Zig `[:0]const u8`).
    pub fn get_title_z(&self) -> Option<&CStr> {
        get_sentinel_cstr(&self.title)
    }

    /// Switch to the given screen type (alternate or primary).
    ///
    /// This does NOT handle behaviors such as clearing the screen,
    /// copying the cursor, etc. This should be handled by downstream
    /// callers.
    ///
    /// After calling this function, the `self.screen` field will point
    /// to the current screen, and the returned value will be the previous
    /// screen. If the return value is null, then the screen was not
    /// switched because it was already the active screen.
    ///
    /// Note: This is written in a generic way so that we can support
    /// more than two screens in the future if needed. There isn't
    /// currently a spec for this, but it is something I think might
    /// be useful in the future.
    pub fn switch_screen(&mut self, key: ScreenKey) -> Result<Option<ScreenPtr>, AllocError> {
        // If we're already on the requested screen we do nothing.
        if self.screens.active_key == key {
            return Ok(None);
        }
        let mut old = self.screens.active;

        // We always end hyperlink state when switching screens.
        // We need to do this on the original screen.
        old.end_hyperlink();

        // Switch the screens/
        let mut new = match self.screens.get(key) {
            Some(s) => s,
            None => {
                let primary = self.screens.get(ScreenKey::Primary).unwrap();
                self.screens.get_init(
                    key,
                    screen::Options {
                        cols: self.cols,
                        rows: self.rows,
                        max_scrollback_bytes: match key {
                            ScreenKey::Primary => Some(primary.pages.limits.bytes.explicit),
                            ScreenKey::Alternate => Some(0),
                        },
                        max_scrollback_lines: None,

                        // Inherit our Kitty image settings from the primary
                        // screen if we have to initialize.
                        kitty_image_storage_limit: if build_options::KITTY_GRAPHICS {
                            primary.kitty_images.total_limit
                        } else {
                            0
                        },
                        kitty_image_loading_limits: primary.kitty_images.image_limits.clone(),
                    },
                )?
            }
        };

        // The new screen should not have any hyperlinks set
        debug_assert!(new.cursor.hyperlink_id == 0);

        // Bring our charset state with us
        new.charset = old.charset;

        // Clear our selection
        new.clear_selection();

        if build_options::KITTY_GRAPHICS {
            // Mark kitty images as dirty so they redraw. Without this the
            // images would stay where they were, because the screen's own
            // dirty flags only cover the terminal grid.
            //
            // Each screen has its own counter. A consumer that last read the
            // old screen will compare that value against this screen's, so
            // the two must differ. Moving past both counters guarantees it.
            new.kitty_images.dirty = new.kitty_images.dirty.max(old.kitty_images.dirty).wrapping_add(1);
        }

        // Mark our terminal as dirty to redraw the grid.
        self.flags.dirty.clear = true;

        // Finalize the switch
        self.screens.switch_to(key);

        Ok(Some(old))
    }

    /// Switch screen via a mode switch (e.g. mode 47, 1047, 1049).
    /// This is a much more opinionated operation than `switchScreen`
    /// since it also handles the behaviors of the specific mode,
    /// such as clearing the screen, saving/restoring the cursor,
    /// etc.
    ///
    /// This should be used for legacy compatibility with VT protocols,
    /// but more modern usage should use `switchScreen` instead and handle
    /// details like clearing the screen, cursor saving, etc. manually.
    pub fn switch_screen_mode(&mut self, mode: SwitchScreenMode, enabled: bool) -> Result<(), AllocError> {
        // The behavior in this function is completely based on reading
        // the xterm source, specifically "charproc.c" for
        // `srm_ALTBUF`, `srm_OPT_ALTBUF`, and `srm_OPT_ALTBUF_CURSOR`.
        // We shouldn't touch anything in here without adding a unit
        // test AND verifying the behavior with xterm.

        match mode {
            SwitchScreenMode::Mode47 => {}

            // If we're disabling 1047 and we're on alt screen then
            // we clear the screen.
            SwitchScreenMode::Mode1047 => {
                if !enabled && self.screens.active_key == ScreenKey::Alternate {
                    self.erase_display(csi::EraseDisplay::Complete, false);
                }
            }

            // 1049 unconditionally saves the cursor on enabling, even
            // if we're already on the alternate screen.
            SwitchScreenMode::Mode1049 => {
                if enabled {
                    self.save_cursor();
                }
            }
        }

        // Switch screens first to whatever we're going to.
        let to = if enabled { ScreenKey::Alternate } else { ScreenKey::Primary };
        let old_ = self.switch_screen(to)?;

        match mode {
            // For these modes, we need to copy the cursor. We only copy
            // the cursor if the screen actually changed, otherwise the
            // cursor is already copied. The cursor is copied regardless
            // of destination screen.
            SwitchScreenMode::Mode47 | SwitchScreenMode::Mode1047 => {
                if let Some(old) = old_ {
                    if let Err(err) = self.screens.active.cursor_copy(&old.cursor, CursorCopy { hyperlink: false }) {
                        log::warn!(target: LOG_TARGET, "cursor copy failed entering alt screen err={err:?}");
                    }
                }
            }

            // Mode 1049 restores cursor on the primary screen when
            // we disable it.
            SwitchScreenMode::Mode1049 => {
                if enabled {
                    debug_assert!(self.screens.active_key == ScreenKey::Alternate);
                    self.erase_display(csi::EraseDisplay::Complete, false);

                    // When we enter alt screen with 1049, we always copy the
                    // cursor from the primary screen (if we weren't already
                    // on it).
                    if let Some(old) = old_ {
                        if let Err(err) =
                            self.screens.active.cursor_copy(&old.cursor, CursorCopy { hyperlink: false })
                        {
                            log::warn!(target: LOG_TARGET, "cursor copy failed entering alt screen err={err:?}");
                        }
                    }
                } else {
                    debug_assert!(self.screens.active_key == ScreenKey::Primary);
                    self.restore_cursor();
                }
            }
        }
        Ok(())
    }

    /// Return the current string value of the terminal. Newlines are
    /// encoded as "\n". This omits any formatting such as fg/bg.
    pub fn plain_string(&self) -> Result<String, DumpStringError> {
        self.screens.active.dump_string_alloc(Point::Viewport(Coordinate::default()))
    }

    /// Same as plainString, but respects row wrap state when building the string.
    pub fn plain_string_unwrapped(&self) -> Result<String, DumpStringError> {
        self.screens
            .active
            .dump_string_alloc_unwrapped(Point::Viewport(Coordinate::default()))
    }

    /// Full reset.
    ///
    /// This will attempt to free the existing screen memory but if that fails
    /// this will reuse the existing memory. In the latter case, memory may
    /// be wasted (since its unused) but it isn't leaked.
    pub fn full_reset(&mut self) {
        // Ensure we're back on primary screen
        self.screens.switch_to(ScreenKey::Primary);

        // Remove alternate screen
        self.screens.remove(ScreenKey::Alternate);

        // Reset primary screen
        self.screens.active.reset();

        // Reset our basic state
        self.flags = Flags {
            // Visibility belongs to the view rather than terminal state, so a
            // terminal reset must not make a hidden view potentially visible.
            visible: self.flags.visible,

            // This is configuration based on the pty rather than terminal
            // state, so a terminal reset must not change it.
            resize_pull_scrollback: self.flags.resize_pull_scrollback,

            xt_checksum: self.default_xt_checksum,
            ..Default::default()
        };
        self.modes.reset();
        self.tabstops.reset(TABSTOP_INTERVAL);
        self.previous_char = None;
        self.pwd.clear();
        self.title.clear();
        self.glyph_glossary.clear_and_free();
        // A reset only interrupts an in-progress chunked OSC 72 command;
        // drag and drop registration survives, matching kitty.
        if let Some(dnd) = self.kitty_dnd.as_mut() {
            dnd.chunking = Default::default();
        }
        self.status_display = ansi::StatusDisplay::Main;
        self.scrolling_region = ScrollingRegion {
            top: 0,
            bottom: self.rows - 1,
            left: 0,
            right: self.cols - 1,
        };
        self.set_cursor_style(ansi::CursorStyle::Default);
        self.colors.palette.reset_all();

        // Always mark dirty so we redraw everything
        self.flags.dirty.clear = true;
    }

    /// DECSTR - Soft Terminal Reset. Resets the modes and state a program may
    /// have left behind, without clearing the screen or moving the cursor.
    /// Beyond what the VT510 manual lists, this also resets what xterm's soft
    /// reset does: left and right margins, the cursor style, the color palette,
    /// modifyOtherKeys, and the XTCHECKSUM variant.
    pub fn soft_reset(&mut self) {
        const RESET_MODES: [Mode; 10] = [
            Mode::CursorVisible,
            Mode::Insert,
            Mode::Origin,
            Mode::Wraparound,
            Mode::ReverseWrap,
            Mode::ReverseWrapExtended,
            Mode::DisableKeyboard,
            Mode::CursorKeys,
            Mode::KeypadKeys,
            Mode::EnableLeftAndRightMargin,
        ];
        for mode in RESET_MODES {
            let v = self.modes.get_default(mode);
            self.modes.set(mode, v);
        }

        self.scrolling_region = ScrollingRegion {
            top: 0,
            bottom: self.rows - 1,
            left: 0,
            right: self.cols - 1,
        };

        // The default style needs no allocation so this can't fail.
        let mut screen = self.screens.active;
        screen.cursor.style = Style::default();
        screen.manual_style_update().unwrap();
        screen.cursor.protected = false;
        // xterm also forgets which protection mode was used last, so erasing
        // after a soft reset no longer skips ISO (SPA) protected cells.
        screen.protected_mode = ansi::ProtectedMode::Off;
        screen.charset = screen::CharsetState::default();

        // A saved cursor now restores to the home position with the defaults.
        screen.saved_cursor = None;

        self.status_display = ansi::StatusDisplay::Main;
        self.flags.modify_other_keys_2 = false;
        self.flags.xt_checksum = self.default_xt_checksum;
        self.set_cursor_style(ansi::CursorStyle::Default);

        // xterm resets the palette for DECSTR as well as RIS, as OSC 104
        // with no arguments would. Dynamic colors (OSC 10-19) are kept.
        if self.colors.palette.mask.count() > 0 {
            self.colors.palette.reset_all();
            self.flags.dirty.palette = true;
        }
    }

    /// Returns true if the point is dirty, used for testing.
    #[allow(dead_code)]
    fn is_dirty(&self, pt: Point) -> bool {
        self.screens.active.pages.get_cell(pt).unwrap().is_dirty()
    }

    /// Clear all dirty bits. Testing only.
    #[allow(dead_code)]
    fn clear_dirty(&mut self) {
        self.screens.active.pages.clear_dirty();
    }
}

/// Shared implementation of `setPwd`/`setTitle`: store `v` followed by a
/// NUL sentinel, or clear the storage if `v` is empty.
fn set_sentinel_string(storage: &mut Vec<u8>, v: &[u8]) -> Result<(), AllocError> {
    if v.is_empty() {
        storage.clear();
        return Ok(());
    }

    let capacity = v.len().checked_add(1).ok_or(AllocError::OutOfMemory)?;
    let additional = capacity.saturating_sub(storage.len());
    storage.try_reserve(additional).map_err(|_| AllocError::OutOfMemory)?;
    storage.clear();
    storage.extend_from_slice(v);
    storage.push(0);
    Ok(())
}

fn get_sentinel_string(storage: &[u8]) -> Option<&[u8]> {
    if storage.is_empty() {
        return None;
    }
    Some(&storage[..storage.len() - 1])
}

fn get_sentinel_cstr(storage: &[u8]) -> Option<&CStr> {
    if storage.is_empty() {
        return None;
    }
    CStr::from_bytes_until_nul(storage).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::page_list;

    // ----- test helpers (Rust-only) -----

    fn init<C, R>(cols: C, rows: R) -> Terminal
    where
        C: TryInto<CellCountInt>,
        R: TryInto<CellCountInt>,
    {
        Terminal::init(Options::new(cols.try_into().ok().unwrap(), rows.try_into().ok().unwrap())).unwrap()
    }

    fn coord<X: TryInto<CellCountInt>, Y: TryInto<u32>>(x: X, y: Y) -> Coordinate {
        Coordinate {
            x: x.try_into().ok().unwrap(),
            y: y.try_into().ok().unwrap(),
        }
    }

    fn scr<X: TryInto<CellCountInt>, Y: TryInto<u32>>(x: X, y: Y) -> Point {
        Point::Screen(coord(x, y))
    }

    fn act<X: TryInto<CellCountInt>, Y: TryInto<u32>>(x: X, y: Y) -> Point {
        Point::Active(coord(x, y))
    }

    fn vp<X: TryInto<CellCountInt>, Y: TryInto<u32>>(x: X, y: Y) -> Point {
        Point::Viewport(coord(x, y))
    }

    #[allow(dead_code)]
    fn hist<X: TryInto<CellCountInt>, Y: TryInto<u32>>(x: X, y: Y) -> Point {
        Point::History(coord(x, y))
    }

    /// The page of a page_list::Cell.
    #[allow(dead_code)]
    fn lc_page<'a>(lc: &page_list::Cell) -> &'a mut Page {
        nd(lc.node).page()
    }

    /// The URI bytes of a hyperlink entry in a page.
    #[allow(dead_code)]
    fn link_uri(page: &Page, id: crate::terminal::hyperlink::Id) -> Vec<u8> {
        // SAFETY: the id is in this page's set and the URI is in page memory.
        unsafe {
            let entry = *page.hyperlink_set.get(page.memory, id);
            (*entry.uri.slice_ptr(page.memory)).to_vec()
        }
    }
    #[test]
    fn terminal_setcursorpos_saturates_overflowing_origin_offsets() {
        let mut t = init(10, 10);

        t.scrolling_region = ScrollingRegion {
            top: 2,
            bottom: 7,
            left: 3,
            right: 8,
        };
        t.modes.set(Mode::Origin, true);

        t.set_cursor_pos(usize::MAX, usize::MAX);
        assert_eq!(8, t.screens.active.cursor.x);
        assert_eq!(7, t.screens.active.cursor.y);
    }

    #[test]
    fn terminal_setcursorrow_and_setcursorcol_in_origin_mode() {
        let mut t = init(10, 10);

        t.scrolling_region = ScrollingRegion {
            top: 2,
            bottom: 7,
            left: 3,
            right: 8,
        };
        t.modes.set(Mode::Origin, true);
        t.set_cursor_pos(2, 2);

        // Only the requested coordinate changes, relative to the margins.
        t.set_cursor_col(1);
        assert_eq!(3, t.screens.active.cursor.x);
        assert_eq!(3, t.screens.active.cursor.y);

        t.set_cursor_row(1);
        assert_eq!(3, t.screens.active.cursor.x);
        assert_eq!(2, t.screens.active.cursor.y);
    }

    #[test]
    fn terminal_cursorrowrelative_and_cursorcolrelative_in_origin_mode() {
        let mut t = init(10, 10);

        t.scrolling_region = ScrollingRegion {
            top: 2,
            bottom: 7,
            left: 3,
            right: 8,
        };
        t.modes.set(Mode::Origin, true);
        t.set_cursor_pos(2, 2);

        t.cursor_col_relative(2);
        assert_eq!(6, t.screens.active.cursor.x);
        assert_eq!(3, t.screens.active.cursor.y);

        t.cursor_row_relative(2);
        assert_eq!(6, t.screens.active.cursor.x);
        assert_eq!(5, t.screens.active.cursor.y);

        // Large moves stop at the edges of the scrolling region.
        t.cursor_col_relative(100);
        t.cursor_row_relative(100);
        assert_eq!(8, t.screens.active.cursor.x);
        assert_eq!(7, t.screens.active.cursor.y);
    }

    #[test]
    fn terminal_cursorrowrelative_and_cursorcolrelative_stop_at_screen_edge() {
        let mut t = init(10, 10);

        t.set_cursor_pos(3, 4);
        t.cursor_col_relative(2);
        t.cursor_row_relative(1);
        assert_eq!(5, t.screens.active.cursor.x);
        assert_eq!(3, t.screens.active.cursor.y);

        t.cursor_col_relative(usize::MAX);
        t.cursor_row_relative(usize::MAX);
        assert_eq!(9, t.screens.active.cursor.x);
        assert_eq!(9, t.screens.active.cursor.y);
    }

    #[test]
    fn terminal_input_with_no_control_characters() {
        let mut t = init(40, 40);

        // Basic grid writing
        for c in "hello".chars() { t.print(c as u32).unwrap(); }
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(5, t.screens.active.cursor.x);
        {
            let str = t.plain_string().unwrap();
            assert_eq!("hello", str);
        }

        // The first row should be dirty
        assert!(t.is_dirty(scr(5, 0)));
        assert!(!t.is_dirty(scr(5, 1)));
    }

    #[test]
    fn terminal_input_with_basic_wraparound() {
        let mut t = init(5, 40);

        // Basic grid writing
        for c in "helloworldabc12".chars() { t.print(c as u32).unwrap(); }
        assert_eq!(2, t.screens.active.cursor.y);
        assert_eq!(4, t.screens.active.cursor.x);
        assert!(t.screens.active.cursor.pending_wrap);
        {
            let str = t.plain_string().unwrap();
            assert_eq!("hello\nworld\nabc12", str);
        }
    }

    #[test]
    fn terminal_input_with_basic_wraparound_dirty() {
        let mut t = init(5, 40);

        for c in "hello".chars() { t.print(c as u32).unwrap(); }
        assert!(t.is_dirty(scr(4, 0)));
        t.clear_dirty();
        t.print('w' as u32).unwrap();

        // Old row is dirty because cursor moved from there
        assert!(t.is_dirty(scr(4, 0)));
        assert!(t.is_dirty(scr(0, 1)));
    }

    #[test]
    fn terminal_input_that_forces_scroll() {
        let mut t = init(1, 5);

        // Basic grid writing
        for c in "abcdef".chars() { t.print(c as u32).unwrap(); }
        assert_eq!(4, t.screens.active.cursor.y);
        assert_eq!(0, t.screens.active.cursor.x);
        {
            let str = t.plain_string().unwrap();
            assert_eq!("b\nc\nd\ne\nf", str);
        }
    }

    #[test]
    fn terminal_input_unique_style_per_cell() {
        let mut t = init(30, 30);

        for y in 0..t.rows {
            for x in 0..t.cols {
                t.set_cursor_pos(y as usize, x as usize);
                t.set_attribute(sgr::Attribute::DirectColorBg(color::Rgb {
                    r: x as _,
                    g: y as _,
                    b: 0,
                }))
                .unwrap();
                t.print('x' as u32).unwrap();
            }
        }
    }

    #[test]
    fn terminal_input_glitch_text() {
        let glitch = include_bytes!("res/glitch.txt");
        let mut t = init(30, 30);

        // Get our initial grapheme capacity.
        let grapheme_cap = {
            let page = nd(t.screens.active.pages.pages.first.unwrap());
            page.capacity().grapheme_bytes
        };

        // Print glitch text until our capacity changes
        loop {
            let page = nd(t.screens.active.pages.pages.first.unwrap());
            if page.capacity().grapheme_bytes != grapheme_cap { break; }
            t.print_string(glitch).unwrap();
        }

        // We're testing to make sure that grapheme capacity gets increased.
        let page = nd(t.screens.active.pages.pages.first.unwrap());
        assert!(page.capacity().grapheme_bytes > grapheme_cap);
    }

    #[test]
    fn terminal_zero_width_character_at_start() {
        let mut t = init(80, 80);

        // This used to crash the terminal. This is not allowed so we should
        // just ignore it.
        t.print(0x200D).unwrap();

        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(0, t.screens.active.cursor.x);

        // Should not be dirty since we changed nothing.
        assert!(!t.is_dirty(scr(0, 0)));
    }

    // https://github.com/ghostty-org/ghostty/pull/12581
    #[test]
    fn terminal_zero_width_character_attaches_to_pending_wrap_cell() {
        let mut t = init(2, 2);

        // Disable grapheme clustering to exercise the fallback path.
        t.modes.set(Mode::GraphemeCluster, false);

        t.print('x' as u32).unwrap();
        t.print('å' as u32).unwrap();
        t.print(0x0332).unwrap(); // Combining low line.

        let str = t.plain_string().unwrap();
        assert_eq!("xå̲", str);
    }

    #[test]
    fn terminal_caps_zero_width_codepoints_attached_to_one_cell() {
        let mut t = init(2, 2);

        t.modes.set(Mode::GraphemeCluster, false);
        t.print('A' as u32).unwrap();

        let initial_capacity = nd(tp(t.screens.active.cursor.page_pin).node).capacity().grapheme_bytes;
        for _ in 0..pagepkg::GRAPHEME_MAX_LEN * 4 { t.print(0x0301).unwrap(); }

        let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
        assert_eq!(
            pagepkg::GRAPHEME_MAX_LEN,
            nd(list_cell.node).page().lookup_grapheme(cl(list_cell.cell)).unwrap().len(),
        );
        assert_eq!(
            initial_capacity,
            nd(list_cell.node).capacity().grapheme_bytes,
        );
    }

    // https://github.com/mitchellh/ghostty/issues/1400
    #[test]
    fn terminal_print_single_very_long_line() {
        let mut t = init(5, 5);

        // This would crash for issue 1400. So the assertion here is
        // that we simply do not crash.
        for _ in 0..1000 { t.print('x' as u32).unwrap(); }
    }

    #[test]
    fn terminal_print_wide_char() {
        let mut t = init(80, 80);

        t.print(0x1F600).unwrap(); // Smiley face
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x1F600, cell.content_codepoint());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(Wide::SpacerTail, cell.wide());
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_wide_char_at_edge_creates_spacer_head() {
        let mut t = init(10, 10);

        t.set_cursor_pos(1, 10);
        t.print(0x1F600).unwrap(); // Smiley face
        assert_eq!(1, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        {
            let list_cell = t.screens.active.pages.get_cell(scr(9, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(Wide::SpacerHead, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x1F600, cell.content_codepoint());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(Wide::SpacerTail, cell.wide());
        }

        // Our first row just had a spacer head added which does not affect
        // rendering so only the place where the wide char was printed
        // should be marked.
        // BUT old row is dirty because cursor moved from there
        assert!(t.is_dirty(scr(0, 0)));
        assert!(t.is_dirty(scr(0, 1)));
    }

    #[test]
    fn terminal_print_wide_char_with_1_column_width() {
        let mut t = init(1, 2);

        t.print('😀' as u32).unwrap(); // 0x1F600

        // This prints a space so we should be dirty.
        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_wide_char_in_single_width_terminal() {
        let mut t = init(1, 80);

        t.print(0x1F600).unwrap(); // Smiley face
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(0, t.screens.active.cursor.x);
        assert!(t.screens.active.cursor.pending_wrap);

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_over_wide_char_at_0_0() {
        let mut t = init(80, 80);

        t.print(0x1F600).unwrap(); // Smiley face
        t.set_cursor_pos(0, 0);
        t.print('A' as u32).unwrap();

        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('A' as u32, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }

        assert!(t.is_dirty(scr(0, 0)));
        assert!(!t.is_dirty(scr(0, 1)));
    }

    #[test]
    fn terminal_print_over_wide_char_at_col_0_corrupts_previous_row() {
        // Crash found by AFL++ fuzzer (afl-out/stream/default/crashes/id:000002).
        //
        // printCell, when overwriting a wide cell with a narrow cell at x<=1
        // and y>0, sets the last cell of the previous row to .narrow — even
        // when that cell is a .spacer_tail rather than a .spacer_head. This
        // orphans the .wide cell at cols-2.
        let mut t = init(10, 3);

        // Fill rows 0 and 1 with wide chars (5 per row on a 10-col terminal).
        for _ in 0..10 { t.print(0x4E2D).unwrap(); }

        // Move cursor to row 1, col 0 (on top of a wide char) and print a
        // narrow character. This triggers printCell's .wide branch which
        // corrupts row 0's last cell: col 9 changes from .spacer_tail to
        // .narrow, orphaning the .wide at col 8.
        t.set_cursor_pos(2, 1);
        t.print('A' as u32).unwrap();

        // Row 1, col 0 should be narrow (we just overwrote the wide char).
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 1)).unwrap();
            assert_eq!(Wide::Narrow, cl(list_cell.cell).wide());
        }
        // Row 0, col 8 should still be .wide (the last wide char on the row).
        {
            let list_cell = t.screens.active.pages.get_cell(scr(8, 0)).unwrap();
            assert_eq!(Wide::Wide, cl(list_cell.cell).wide());
        }
        // Row 0, col 9 must remain .spacer_tail to pair with the .wide at col 8.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(9, 0)).unwrap();
            assert_eq!(Wide::SpacerTail, cl(list_cell.cell).wide());
        }
    }

    #[test]
    fn terminal_print_over_wide_spacer_tail() {
        let mut t = init(5, 5);

        t.print('橋' as u32).unwrap();
        t.set_cursor_pos(1, 2);
        t.print('X' as u32).unwrap();

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('X' as u32, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }

        {
            let str = t.plain_string().unwrap();
            assert_eq!(" X", str);
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_over_wide_char_with_bold() {
        let mut t = init(80, 80);

        t.set_attribute(sgr::Attribute::Bold).unwrap();
        t.print(0x1F600).unwrap(); // Smiley face
        // verify we have styles in our style map
        {
            let page = cursor_page(t.screens.active);
            assert_eq!(1, page.styles.count());
        }

        // Go back and overwrite with no style
        t.set_cursor_pos(0, 0);
        t.set_attribute(sgr::Attribute::Unset).unwrap();
        t.print('A' as u32).unwrap(); // Smiley face

        // verify our style is gone
        {
            let page = cursor_page(t.screens.active);
            assert_eq!(0, page.styles.count());
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_over_wide_char_with_bg_color() {
        let mut t = init(80, 80);

        t.set_attribute(sgr::Attribute::DirectColorBg(color::Rgb {
            r: 0xFF,
            g: 0,
            b: 0,
        })).unwrap();
        t.print(0x1F600).unwrap(); // Smiley face
        // verify we have styles in our style map
        {
            let page = cursor_page(t.screens.active);
            assert_eq!(1, page.styles.count());
        }

        // Go back and overwrite with no style
        t.set_cursor_pos(0, 0);
        t.set_attribute(sgr::Attribute::Unset).unwrap();
        t.print('A' as u32).unwrap(); // Smiley face

        // verify our style is gone
        {
            let page = cursor_page(t.screens.active);
            assert_eq!(0, page.styles.count());
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_multicodepoint_grapheme_disabled_mode_2027() {
        let mut t = init(80, 80);

        // https://github.com/mitchellh/ghostty/issues/289
        // This is: 👨‍👩‍👧 (which may or may not render correctly)
        t.print(0x1F468).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F469).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F467).unwrap();

        // We should have 6 cells taken up
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(6, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x1F468, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(1, cps.len());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::SpacerTail, cell.wide());
            assert!(nd(list_cell.node).page().lookup_grapheme(cell).is_none());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(2, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x1F469, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(1, cps.len());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(3, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::SpacerTail, cell.wide());
            assert!(nd(list_cell.node).page().lookup_grapheme(cell).is_none());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(4, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x1F467, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
            assert!(nd(list_cell.node).page().lookup_grapheme(cell).is_none());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(5, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::SpacerTail, cell.wide());
            assert!(nd(list_cell.node).page().lookup_grapheme(cell).is_none());
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_enabling_grapheme_mode_handles_stored_breaks() {
        let mut t = init(5, 1);

        t.modes.set(Mode::GraphemeCluster, false);
        t.print('a' as u32).unwrap();
        t.print(0x200B).unwrap(); // Zero width space is stored on the prior cell.

        t.modes.set(Mode::GraphemeCluster, true);
        t.print(0x0301).unwrap();

        let str = t.plain_string().unwrap();
        assert_eq!(&b"a\xE2\x80\x8B\xCC\x81"[..], str.as_bytes());
    }

    // Terminal.print receives one codepoint at a time, so it can't use
    // unicode.graphemeWidth directly; that API requires a complete buffered
    // cluster or string end. This keeps the streaming printer's cursor advance
    // in sync with the buffered measurement API for representative clusters.
    fn expect_grapheme_width_parity(cps: &[u32]) {
        let mut t = init(80, 5);

        t.modes.set(Mode::GraphemeCluster, true);

        let mut expected: usize = 0;
        let mut i: usize = 0;
        while i < cps.len() {
            let result = unicode::grapheme_width(&cps[i..]);
            assert!(result.len > 0);
            i += result.len;
            expected += result.width as usize;
        }

        for &cp in cps {
            t.print(cp).unwrap();
        }
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(expected, t.screens.active.cursor.x as usize);
    }

    #[test]
    fn terminal_graphemewidth_parity() {
        expect_grapheme_width_parity(&[ 0x2764, 0xFE0F ]);
        expect_grapheme_width_parity(&[ 'x' as u32, 0xFE0F, 0xFE0F ]);
        expect_grapheme_width_parity(&[ 0x231A, 0xFE0E, 0xFE0F ]);
        expect_grapheme_width_parity(&[ 0x1F3F4, 0x200D, 0x2620, 0xFE0F ]);
        expect_grapheme_width_parity(&[ 0x1F468, 0x200D, 0x1F469, 0x200D, 0x1F467 ]);
        expect_grapheme_width_parity(&[ 0x23, 0xFE0F, 0x20E3 ]);
        expect_grapheme_width_parity(&[ '1' as u32, 0x20E3 ]);
        expect_grapheme_width_parity(&[ 0x1F44B, 0x1F3FF ]);
        expect_grapheme_width_parity(&[ 0x1F1E6, 0x1F1E7, 0x1F1E8 ]);
        expect_grapheme_width_parity(&[ 'a' as u32, 'b' as u32 ]);
        expect_grapheme_width_parity(&[ 0x0301, 0x0302 ]);
    }

    #[test]
    fn terminal_vs16_doesn_t_make_character_with_2027_disabled() {
        let mut t = init(5, 5);

        // Disable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, false);

        t.print(0x2764).unwrap(); // Heart
        t.print(0xFE0F).unwrap(); // VS16 to make wide

        {
            let str = t.plain_string().unwrap();
            assert_eq!("❤️", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x2764, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(1, cps.len());
        }
    }

    #[test]
    fn terminal_ignored_vs16_doesn_t_mark_dirty() {
        let mut t = init(5, 5);

        // Disable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, false);

        t.print(0x2764).unwrap(); // Heart
        assert!(t.is_dirty(scr(0, 0)));

        t.clear_dirty();
        t.print(0xFE0F).unwrap(); // VS16 to make wide
        assert!(!t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_invalid_vs16_non_grapheme() {
        let mut t = init(80, 80);

        // https://github.com/mitchellh/ghostty/issues/1482
        t.print('x' as u32).unwrap();
        t.print(0xFE0F).unwrap();

        // We should have 1 narrow cell.
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('x' as u32, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
        }
    }

    #[test]
    fn terminal_invalid_vs16_doesn_t_mark_dirty() {
        let mut t = init(5, 5);

        // Disable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, false);

        t.print('x' as u32).unwrap();
        assert!(t.is_dirty(scr(0, 0)));

        t.clear_dirty();
        t.print(0xFE0F).unwrap(); // VS16 to make wide
        assert!(!t.is_dirty(scr(0, 0)));
    }

    // https://github.com/ghostty-org/ghostty/pull/12596
    #[test]
    fn terminal_variation_selectors_apply_to_preceding_codepoint() {
        let mut t = init(5, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // Pirate flag: black flag + ZWJ + skull and crossbones + VS16.
        t.print(0x1F3F4).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x2620).unwrap();
        t.print(0xFE0F).unwrap();

        let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
        let cell = cl(list_cell.cell);
        assert_eq!(0x1F3F4, cell.content_codepoint());
        assert!(cell.has_grapheme());
        assert_eq!(&[ 0x200D, 0x2620, 0xFE0F ][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
    }

    #[test]
    fn terminal_print_multicodepoint_grapheme_mode_2027() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // https://github.com/mitchellh/ghostty/issues/289
        // This is: 👨‍👩‍👧 (which may or may not render correctly)
        t.print(0x1F468).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F469).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F467).unwrap();

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // Row should be dirty
        assert!(t.is_dirty(scr(0, 0)));

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x1F468, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(4, cps.len());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::SpacerTail, cell.wide());
        }
    }

    #[test]
    fn terminal_keypad_sequence_vs15() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // This is: "#︎" (number sign with text presentation selector)
        t.print(0x23).unwrap(); // # Number sign (valid base)
        t.print(0xFE0E).unwrap(); // VS15 (text presentation selector)

        // VS15 should combine with the base character into a single grapheme cluster,
        // taking 1 cell (narrow character).
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);

        // Row should be dirty
        assert!(t.is_dirty(scr(0, 0)));

        // The base emoji should be in cell 0 with the skin tone as a grapheme
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x23, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
        }
    }

    #[test]
    fn terminal_keypad_sequence_vs16() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // This is: "#️" (number sign with emoji presentation selector)
        t.print(0x23).unwrap(); // # Number sign (valid base)
        t.print(0xFE0F).unwrap(); // VS16 (emoji presentation selector)

        // VS16 should combine with the base character into a single grapheme cluster,
        // taking 2 cells (wide character).
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // Row should be dirty
        assert!(t.is_dirty(scr(0, 0)));

        // The base emoji should be in cell 0 with the skin tone as a grapheme
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x23, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
        }
    }

    #[test]
    fn terminal_fitzpatrick_skin_tone_next_valid_base() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // This is: "👋🏿" (waving hand with dark skin tone)
        t.print(0x1F44B).unwrap(); // 👋 Waving hand (valid base)
        t.print(0x1F3FF).unwrap(); // 🏿 Dark skin tone modifier

        // The skin tone should combine with the base emoji into a single grapheme cluster,
        // taking 2 cells (wide character).
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // Row should be dirty
        assert!(t.is_dirty(scr(0, 0)));

        // The base emoji should be in cell 0 with the skin tone as a grapheme
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x1F44B, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
        }
    }

    #[test]
    fn terminal_fitzpatrick_skin_tone_next_to_non_base() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // This is: "🏿" (which may not render correctly in your editor!)
        t.print(0x22).unwrap(); // "
        t.print(0x1F3FF).unwrap(); // Dark skin tone
        t.print(0x22).unwrap(); // "

        // We should have 4 cells taken up. Importantly, the skin tone
        // should not join with the quotes.
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(4, t.screens.active.cursor.x);

        // Row should be dirty
        assert!(t.is_dirty(scr(0, 0)));

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x22, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x1F3FF, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(3, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x22, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
        }
    }

    #[test]
    fn terminal_multicodepoint_grapheme_marks_dirty_on_every_codepoint() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // https://github.com/mitchellh/ghostty/issues/289
        // This is: 👨‍👩‍👧 (which may or may not render correctly)
        t.print(0x1F468).unwrap();
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();
        t.print(0x200D).unwrap();
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();
        t.print(0x1F469).unwrap();
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();
        t.print(0x200D).unwrap();
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();
        t.print(0x1F467).unwrap();
        assert!(t.is_dirty(scr(0, 0)));

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);
    }

    #[test]
    fn terminal_vs15_to_make_narrow_character() {
        let mut t = init(5, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.print(0x2614).unwrap(); // Umbrella with rain drops, width=2
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        t.print(0xFE0E).unwrap(); // VS15 to make narrow
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();

        // VS15 should send us back a cell since our char is no longer wide.
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("☔︎", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x2614, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(1, cps.len());
        }
    }

    #[test]
    fn terminal_vs15_on_already_narrow_emoji() {
        let mut t = init(5, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.print(0x26C8).unwrap(); // Thunder cloud and rain, width=1
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();
        t.print(0xFE0E).unwrap(); // VS15 to make narrow
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();

        // Character takes up one cell
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("⛈︎", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x26C8, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(1, cps.len());
        }
    }

    #[test]
    fn terminal_print_invalid_vs15_following_emoji_is_wide() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.print('\u{1F9E0}' as u32).unwrap(); // 🧠
        t.print(0xFE0E).unwrap(); // not valid with U+1F9E0 as base

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('\u{1F9E0}' as u32, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::SpacerTail, cell.wide());
        }
    }

    #[test]
    fn terminal_print_invalid_vs15_in_emoji_zwj_sequence() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.print('\u{1F469}' as u32).unwrap(); // 👩
        t.print(0xFE0E).unwrap(); // not valid with U+1F469 as base
        t.print('\u{200D}' as u32).unwrap(); // ZWJ
        t.print('\u{1F466}' as u32).unwrap(); // 👦

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('\u{1F469}' as u32, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(&[ '\u{200D}' as u32, '\u{1F466}' as u32 ][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::SpacerTail, cell.wide());
        }
    }

    #[test]
    fn terminal_vs15_to_make_narrow_character_with_pending_wrap() {
        let mut t = init(4, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        assert!(t.modes.get(Mode::Wraparound));

        t.print(0x1F34B).unwrap(); // Lemon, width=2
        t.print(0x2614).unwrap(); // Umbrella with rain drops, width=2
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();

        // We only move to the end of the line because we're in a pending wrap
        // state.
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(3, t.screens.active.cursor.x);
        assert!(t.screens.active.cursor.pending_wrap);

        t.print(0xFE0E).unwrap(); // VS15 to make narrow
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();

        // VS15 should clear the pending wrap state
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(3, t.screens.active.cursor.x);
        assert!(!t.screens.active.cursor.pending_wrap);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("🍋☔︎", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(2, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x2614, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(1, cps.len());
        }

        // VS15 should not affect the previous grapheme
        {
            let lemon_cell = cl(t.screens.active.pages.get_cell(scr(0, 0)).unwrap().cell);
            assert_eq!(0x1F34B, lemon_cell.content_codepoint());
            assert_eq!(Wide::Wide, lemon_cell.wide());
            let spacer_cell = cl(t.screens.active.pages.get_cell(scr(1, 0)).unwrap().cell);
            assert_eq!(0, spacer_cell.content_codepoint());
            assert_eq!(Wide::SpacerTail, spacer_cell.wide());
        }
    }

    #[test]
    fn terminal_vs15_narrows_wide_cell_under_cursor_with_wraparound_disabled() {
        let mut t = init(5, 5);

        t.modes.set(Mode::GraphemeCluster, true);
        t.modes.set(Mode::Wraparound, false);

        // First create a wide cell spanning columns 4 and 5.
        t.set_cursor_pos(1, 4);
        t.print(0x2614).unwrap();

        // Make column 4 the right margin and put the cursor on the wide base.
        // With wraparound disabled, grapheme lookup selects the cell under the
        // cursor when it has content.
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(1, 4);
        t.set_cursor_pos(1, 4);
        t.print(0xFE0E).unwrap();

        assert_eq!(3, t.screens.active.cursor.x);
        assert!(!t.screens.active.cursor.pending_wrap);
        let base = cl(t.screens.active.pages.get_cell(scr(3, 0)).unwrap().cell);
        assert_eq!(Wide::Narrow, base.wide());
        assert!(base.has_grapheme());
        let tail = cl(t.screens.active.pages.get_cell(scr(4, 0)).unwrap().cell);
        assert_eq!(Wide::Narrow, tail.wide());
    }

    #[test]
    fn terminal_vs15_narrows_wide_cell_under_restored_pending_cursor() {
        let mut t = init(5, 5);

        t.modes.set(Mode::GraphemeCluster, true);
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(1, 4);

        // Save a pending-wrap cursor at column 4.
        t.set_cursor_pos(1, 4);
        t.print('X' as u32).unwrap();
        assert!(t.screens.active.cursor.pending_wrap);
        t.save_cursor();

        // Widen the margin and replace that cell with a wide character.
        t.set_left_and_right_margin(1, 5);
        t.set_cursor_pos(1, 4);
        t.print(0x2614).unwrap();

        // Restoring also restores pending_wrap, so grapheme lookup selects the
        // wide base under the cursor rather than its spacer tail.
        t.restore_cursor();
        assert!(t.screens.active.cursor.pending_wrap);
        t.print(0xFE0E).unwrap();

        assert_eq!(4, t.screens.active.cursor.x);
        assert!(!t.screens.active.cursor.pending_wrap);
        let base = cl(t.screens.active.pages.get_cell(scr(3, 0)).unwrap().cell);
        assert_eq!(Wide::Narrow, base.wide());
        assert!(base.has_grapheme());
        let tail = cl(t.screens.active.pages.get_cell(scr(4, 0)).unwrap().cell);
        assert_eq!(Wide::Narrow, tail.wide());
    }

    #[test]
    fn terminal_vs16_to_make_wide_character_on_next_line() {
        let mut t = init(3, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.cursor_right(2);
        t.print('#' as u32).unwrap();
        assert_eq!(2, t.screens.active.cursor.x);
        assert!(t.screens.active.cursor.pending_wrap);
        assert!(t.is_dirty(scr(2, 0)));
        t.clear_dirty();

        t.print(0xFE0F).unwrap(); // VS16 to make wide

        assert!(t.is_dirty(scr(2, 0)));
        t.clear_dirty();
        assert_eq!(1, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);
        assert!(!t.screens.active.cursor.pending_wrap);

        {
            // Previous cell turns into spacer_head
            let list_cell = t.screens.active.pages.get_cell(scr(2, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::SpacerHead, cell.wide());
        }
        {
            // '#' cell is wide
            let list_cell = t.screens.active.pages.get_cell(scr(0, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('#' as u32, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(&[0xFE0F][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            // spacer_tail
            let list_cell = t.screens.active.pages.get_cell(scr(1, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::SpacerTail, cell.wide());
        }
    }

    #[test]
    fn terminal_vs16_to_make_wide_character_on_next_line_with_hyperlink() {
        // Regression test for the crash fixed in print's grapheme `.wide` path:
        // writing a spacer_head at the screen edge before row.wrap() was set.
        let mut t = init(3, 5);

        // Enable grapheme clustering and activate a hyperlink so printCell
        // calls cursorSetHyperlink (which runs page integrity checks).
        t.modes.set(Mode::GraphemeCluster, true);
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();

        t.cursor_right(2);
        t.print('#' as u32).unwrap();
        assert_eq!(2, t.screens.active.cursor.x);
        assert!(t.screens.active.cursor.pending_wrap);

        // Without the fix, this panicked with UnwrappedSpacerHead.
        t.print(0xFE0F).unwrap(); // VS16 to make wide

        assert_eq!(1, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);
        assert!(!t.screens.active.cursor.pending_wrap);

        {
            // Previous cell turns into spacer_head and remains hyperlinked.
            let list_cell = t.screens.active.pages.get_cell(scr(2, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::SpacerHead, cell.wide());
            assert!(cell.hyperlink());
            assert!(rw(list_cell.row).wrap());
        }
        {
            // '#' cell is now wide and still hyperlinked.
            let list_cell = t.screens.active.pages.get_cell(scr(0, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('#' as u32, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(&[0xFE0F][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Wide, cell.wide());
            assert!(cell.hyperlink());
        }
        {
            // spacer_tail inherits hyperlink as part of the same grapheme cell.
            let list_cell = t.screens.active.pages.get_cell(scr(1, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::SpacerTail, cell.wide());
            assert!(cell.hyperlink());
        }
    }

    #[test]
    fn terminal_vs16_widening_when_the_spacer_tail_grows_the_page() {
        // Regression test for a stale cell pointer in print's grapheme `.wide`
        // path: writing the spacer tail can grow the page to fit the hyperlink,
        // which replaces the page and invalidates the pointer to the wide cell.
        let mut t = init(20, 10);

        t.modes.set(Mode::GraphemeCluster, true);
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();

        // Fill the page hyperlink map until a single slot is left. The '#' below
        // takes that slot so the spacer tail is what forces the page to grow.
        loop {
            let page = cursor_page(t.screens.active);
            let map = unsafe { page.hyperlink_map.map(page.memory) };
            if map.max_load() - map.count() == 1 { break; }
            t.print('x' as u32).unwrap();
        }

        let x = t.screens.active.cursor.x;
        let y = t.screens.active.cursor.y;
        t.print('#' as u32).unwrap();

        // Without the fix this crashed appending to a freed page.
        t.print(0xFE0F).unwrap();

        {
            // '#' is wide and carries the VS16 grapheme.
            let list_cell = t.screens.active.pages.get_cell(act(x, y)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('#' as u32, cell.content_codepoint());
            assert_eq!(Wide::Wide, cell.wide());
            assert!(cell.has_grapheme());
            assert_eq!(
                &[0xFE0F][..],
                nd(list_cell.node).page().lookup_grapheme(cell).unwrap(),
            );
        }
        {
            let list_cell = t.screens.active.pages.get_cell(act(x + 1, y)).unwrap();
            assert_eq!(Wide::SpacerTail, cl(list_cell.cell).wide());
        }
    }

    #[test]
    fn terminal_grapheme_transfer_when_widening_wraps_to_the_next_line() {
        // Covers print's grapheme `.wide` path where the previous cell already
        // holds grapheme data and has to be moved to the wrapped row.
        let mut t = init(3, 5);

        t.modes.set(Mode::GraphemeCluster, true);
        t.cursor_right(2);

        // A narrow emoji, then ZWJ, then a second emoji. The ZWJ attaches
        // without changing the width, so the cell has grapheme data by the time
        // the second emoji widens it.
        t.print(0x263A).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x2764).unwrap();

        {
            // The old cell becomes a spacer head on the wrapped row.
            let list_cell = t.screens.active.pages.get_cell(scr(2, 0)).unwrap();
            assert_eq!(Wide::SpacerHead, cl(list_cell.cell).wide());
            assert!(rw(list_cell.row).wrap());
        }
        {
            // The grapheme moved with the base codepoint.
            let list_cell = t.screens.active.pages.get_cell(scr(0, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x263A, cell.content_codepoint());
            assert_eq!(Wide::Wide, cell.wide());
            assert_eq!(
                &[ 0x200D, 0x2764 ][..],
                nd(list_cell.node).page().lookup_grapheme(cell).unwrap(),
            );
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 1)).unwrap();
            assert_eq!(Wide::SpacerTail, cl(list_cell.cell).wide());
        }
    }

    #[test]
    fn terminal_grapheme_transfer_when_widening_wraps_below_the_scroll_region() {
        let mut t = init(3, 3);

        t.modes.set(Mode::GraphemeCluster, true);
        t.set_top_and_bottom_margin(1, 2);
        t.set_cursor_pos(3, 3);
        t.print(0x2764).unwrap(); // Heart
        t.print(0x200D).unwrap(); // ZWJ
        t.print(0x1F44D).unwrap(); // Thumbs up

        let base = t.screens.active.pages.get_cell(act(0, 2)).unwrap();
        assert_eq!(0x2764, cl(base.cell).codepoint());
        assert_eq!(
            &[ 0x200D, 0x1F44D ][..],
            nd(base.node).page().lookup_grapheme(cl(base.cell)).unwrap(),
        );
    }

    #[test]
    fn terminal_grapheme_transfer_when_widening_scrolls_a_single_row() {
        for alternate in [false, true] {
            for margins in [false, true] {
                let mut t = init(5, 1);

                t.modes.set(Mode::GraphemeCluster, true);
                if alternate { t.switch_screen(ScreenKey::Alternate).unwrap(); }

                // Leave a grapheme on the row above, in scrollback. It must stay
                // there untouched when the cluster wraps below it.
                t.set_cursor_pos(1, 5);
                t.print('a' as u32).unwrap();
                t.print(0x0301).unwrap(); // Combining acute accent
                t.index().unwrap();

                if margins {
                    t.modes.set(Mode::EnableLeftAndRightMargin, true);
                    t.set_left_and_right_margin(3, 0);
                }
                t.set_cursor_pos(1, 5);
                t.print(0x2764).unwrap(); // Heart
                t.print(0x200D).unwrap(); // ZWJ
                t.print(0x1F44D).unwrap(); // Thumbs up

                let x: CellCountInt = if margins { 2 } else { 0 };
                let base = t.screens.active.pages.get_cell(act(x, 0)).unwrap();
                assert_eq!(0x2764, cl(base.cell).codepoint());
                assert_eq!(
                    &[ 0x200D, 0x1F44D ][..],
                    nd(base.node).page().lookup_grapheme(cl(base.cell)).unwrap(),
                );

                // The alternate screen has no scrollback to check.
                if !alternate {
                    let history = t.screens.active.pages.get_cell(scr(4, 0)).unwrap();
                    assert_eq!('a' as u32, cl(history.cell).codepoint());
                    assert_eq!(&[0x0301][..], nd(history.node).page().lookup_grapheme(cl(history.cell)).unwrap());
                }
            }
        }
    }

    #[test]
    fn terminal_grapheme_transfer_when_widening_wraps_to_a_page_without_grapheme_capacity() {
        let rows = pagepkg::STD_CAPACITY.rows;
        let cols = pagepkg::STD_CAPACITY.cols;
        let mut t = init(cols, rows);

        t.modes.set(Mode::GraphemeCluster, true);

        // Fill the first page so the bottom row of the screen is on a second
        // page, then compact that page so it has no grapheme capacity.
        t.cursor_down(rows as usize - 1);
        for _ in rows..nd(t.screens.active.pages.pages.first.unwrap()).capacity().rows + 1 {
            t.index().unwrap();
        }
        let mut sp = t.screens.active;
        let pages = &mut sp.pages;
        assert!(pages.pages.first != pages.pages.last);
        assert!(pages.compact(pages.pages.last.unwrap()).unwrap().is_some());
        assert_eq!(0, nd(pages.pages.last.unwrap()).capacity().grapheme_bytes);

        t.set_cursor_pos(rows as usize - 1, cols as usize);
        t.print(0x2764).unwrap(); // Heart
        t.print(0x200D).unwrap(); // ZWJ
        t.print(0x1F44D).unwrap(); // Thumbs up

        // The re-attach had to grow the page's grapheme capacity.
        assert!(nd(pages.pages.last.unwrap()).capacity().grapheme_bytes > 0);
        let base = pages.get_cell(act(0, rows - 1)).unwrap();
        assert_eq!(pages.pages.last.unwrap(), base.node);
        assert_eq!(Wide::Wide, cl(base.cell).wide());
        assert_eq!(0x2764, cl(base.cell).codepoint());
        assert_eq!(
            &[ 0x200D, 0x1F44D ][..],
            nd(base.node).page().lookup_grapheme(cl(base.cell)).unwrap(),
        );
    }

    #[test]
    fn terminal_vs16_to_make_wide_character_with_pending_wrap() {
        let mut t = init(3, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.cursor_right(1);
        t.print('#' as u32).unwrap();
        assert_eq!(2, t.screens.active.cursor.x);
        assert!(!t.screens.active.cursor.pending_wrap);

        t.print(0xFE0F).unwrap(); // VS16 to make wide

        assert_eq!(2, t.screens.active.cursor.x);
        assert_eq!(0, t.screens.active.cursor.y);
        assert!(t.screens.active.cursor.pending_wrap);

        {
            // '#' cell is wide
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('#' as u32, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(&[0xFE0F][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            // spacer_tail
            let list_cell = t.screens.active.pages.get_cell(scr(2, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::SpacerTail, cell.wide());
        }
    }

    #[test]
    fn terminal_vs16_to_make_wide_character_with_mode_2027() {
        let mut t = init(5, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.print(0x2764).unwrap(); // Heart
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();
        t.print(0xFE0F).unwrap(); // VS16 to make wide
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("❤️", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x2764, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(1, cps.len());
        }
    }

    #[test]
    fn terminal_vs16_repeated_with_mode_2027() {
        let mut t = init(5, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.print(0x2764).unwrap(); // Heart
        t.print(0xFE0F).unwrap(); // VS16 to make wide
        t.print(0x2764).unwrap(); // Heart
        t.print(0xFE0F).unwrap(); // VS16 to make wide

        assert!(t.is_dirty(scr(0, 0)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("❤️❤️", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x2764, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(1, cps.len());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(2, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x2764, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(Wide::Wide, cell.wide());
            let cps = nd(list_cell.node).page().lookup_grapheme(cell).unwrap();
            assert_eq!(1, cps.len());
        }
    }

    #[test]
    fn terminal_print_invalid_vs16_grapheme() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // https://github.com/mitchellh/ghostty/issues/1482
        t.print('x' as u32).unwrap();
        t.print(0xFE0F).unwrap(); // invalid VS16

        // We should have 1 cells taken up, and narrow.
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('x' as u32, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }
    }

    #[test]
    fn terminal_print_invalid_vs16_with_second_char() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // https://github.com/mitchellh/ghostty/issues/1482
        t.print('x' as u32).unwrap();
        t.print(0xFE0F).unwrap();
        t.print('y' as u32).unwrap();

        // We should have 2 cells taken up, from two separate narrow characters.
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('x' as u32, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('y' as u32, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
        }
    }

    #[test]
    fn terminal_print_grapheme_o_o_with_nonspacing_mark_should_be_narrow() {
        let mut t = init(5, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.print('o' as u32).unwrap();
        t.print(0x0300).unwrap(); // combining grave accent

        // We should have 1 cell taken up.
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('o' as u32, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(&[0x0300][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Narrow, cell.wide());
        }
    }

    #[test]
    fn terminal_print_devanagari_grapheme_should_be_wide() {
        let mut t = init(5, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // क्‍ष
        t.print(0x0915).unwrap();
        t.print(0x094D).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x0937).unwrap();

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x0915, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(&[ 0x094D, 0x200D, 0x0937 ][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(Wide::SpacerTail, cell.wide());
        }
    }

    #[test]
    fn terminal_print_devanagari_grapheme_should_be_wide_on_next_line() {
        let mut t = init(3, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.cursor_right(2);

        // क्‍ष
        t.print(0x0915).unwrap();
        t.print(0x094D).unwrap();
        t.print(0x200D).unwrap();
        assert_eq!(2, t.screens.active.cursor.x);
        assert!(t.screens.active.cursor.pending_wrap);

        // This one increases the width to wide
        t.print(0x0937).unwrap();

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(1, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);
        assert!(!t.screens.active.cursor.pending_wrap);

        {
            // Previous cell turns into spacer_head
            let list_cell = t.screens.active.pages.get_cell(scr(2, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::SpacerHead, cell.wide());
        }
        {
            // Devanagari grapheme is wide
            let list_cell = t.screens.active.pages.get_cell(scr(0, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x0915, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(&[ 0x094D, 0x200D, 0x0937 ][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(Wide::SpacerTail, cell.wide());
        }
    }

    #[test]
    fn terminal_print_devanagari_grapheme_should_be_wide_on_next_page() {
        let rows = pagepkg::STD_CAPACITY.rows;
        let cols = pagepkg::STD_CAPACITY.cols;
        let mut t = init(cols, rows);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.cursor_down(rows as usize - 1);

        for _ in rows..nd(t.screens.active.pages.pages.first.unwrap()).capacity().rows {
            t.index().unwrap();
        }

        t.cursor_right(cols as usize - 1);

        assert_eq!(cols - 1, t.screens.active.cursor.x);
        assert_eq!(rows - 1, t.screens.active.cursor.y);

        // क्‍ष
        t.print(0x0915).unwrap();
        t.print(0x094D).unwrap();
        t.print(0x200D).unwrap();
        assert_eq!(cols - 1, t.screens.active.cursor.x);
        assert!(t.screens.active.cursor.pending_wrap);

        // This one increases the width to wide
        t.print(0x0937).unwrap();

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(rows - 1, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);
        assert!(!t.screens.active.cursor.pending_wrap);

        {
            // Previous cell turns into spacer_head
            let list_cell = t.screens.active.pages.get_cell(act(cols - 1, rows - 2)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::SpacerHead, cell.wide());
        }
        {
            // Devanagari grapheme is wide
            let list_cell = t.screens.active.pages.get_cell(act(0, rows - 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x0915, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(&[ 0x094D, 0x200D, 0x0937 ][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(act(1, rows - 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(Wide::SpacerTail, cell.wide());
        }
    }

    #[test]
    fn terminal_print_invalid_vs16_with_second_char_combining() {
        let mut t = init(80, 80);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // https://github.com/mitchellh/ghostty/issues/1482
        t.print('n' as u32).unwrap();
        t.print(0xFE0F).unwrap(); // invalid VS16
        t.print(0x0303).unwrap(); // combining tilde

        // We should have 1 cells taken up, and narrow.
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('n' as u32, cell.content_codepoint());
            assert!(cell.has_grapheme());
            assert_eq!(&['\u{0303}' as u32][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Narrow, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }
    }

    #[test]
    fn terminal_overwrite_grapheme_should_clear_grapheme_data() {
        let mut t = init(5, 5);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.print(0x26C8).unwrap(); // Thunder cloud and rain
        t.print(0xFE0E).unwrap(); // VS15 to make narrow
        assert!(t.is_dirty(scr(0, 0)));
        t.clear_dirty();

        t.set_cursor_pos(1, 1);
        t.print('A' as u32).unwrap();
        assert!(t.is_dirty(scr(0, 0)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('A' as u32, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
        }
    }

    #[test]
    fn terminal_overwrite_multicodepoint_grapheme_clears_grapheme_data() {
        let mut t = init(10, 10);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // https://github.com/mitchellh/ghostty/issues/289
        // This is: 👨‍👩‍👧 (which may or may not render correctly)
        t.print(0x1F468).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F469).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F467).unwrap();

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // We should have one cell with graphemes
        let page = cursor_page(t.screens.active);
        assert_eq!(1, page.grapheme_count());

        // Move back and overwrite wide
        t.set_cursor_pos(1, 1);
        t.clear_dirty();
        t.print('X' as u32).unwrap();
        assert!(t.is_dirty(scr(0, 0)));

        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);
        assert_eq!(0, page.grapheme_count());

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X", str);
        }
    }

    #[test]
    fn terminal_overwrite_multicodepoint_grapheme_tail_clears_grapheme_data() {
        let mut t = init(10, 10);

        // Enable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        // https://github.com/mitchellh/ghostty/issues/289
        // This is: 👨‍👩‍👧 (which may or may not render correctly)
        t.print(0x1F468).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F469).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F467).unwrap();

        // We should have 2 cells taken up. It is one character but "wide".
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // We should have one cell with graphemes
        let page = cursor_page(t.screens.active);
        assert_eq!(1, page.grapheme_count());

        // Move back and overwrite wide
        t.set_cursor_pos(1, 2);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!(" X", str);
        }

        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);
        assert_eq!(0, page.grapheme_count());
    }

    #[test]
    fn terminal_print_breaks_valid_grapheme_cluster_with_prepend_ascii_for_speed() {
        let mut t = init(5, 5);
        t.modes.set(Mode::GraphemeCluster, true);

        // Make sure we're not at cursor.x == 0 for the next char.
        t.print('_' as u32).unwrap();

        // U+0600 ARABIC NUMBER SIGN (Prepend)
        t.print(0x0600).unwrap();
        t.print('1' as u32).unwrap();

        // We should have 3 cells taken up, each narrow. Note that this is
        // **incorrect** grapheme break behavior, since a Prepend code point should
        // not break with the one following it per UAX #29 GB9b. However, as an
        // optimization we assume a grapheme break when c <= 255, and note that
        // this deviation only affects these very uncommon scenarios (e.g. the
        // Arabic number sign should precede Arabic-script digits).
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(3, t.screens.active.cursor.x);
        // This is what we'd expect if we did break correctly:
        //assert_eq!(2, t.screens.active.cursor.x);

        // Assert various properties about our screen to verify
        // we have all expected cells.
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x0600, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            // This is what we'd expect if we did break correctly:
            //assert!(cell.has_grapheme());
            //assert_eq!(&['1' as u32][..], nd(list_cell.node).page().lookup_grapheme(cell).unwrap());
            assert_eq!(Wide::Narrow, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(2, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('1' as u32, cell.content_codepoint());
            // This is what we'd expect if we did break correctly:
            //assert_eq!(0, cell.content_codepoint());
            assert!(!cell.has_grapheme());
            assert_eq!(Wide::Narrow, cell.wide());
        }
    }

    #[test]
    fn terminal_print_writes_to_bottom_if_scrolled() {
        let mut t = init(5, 2);

        // Basic grid writing
        for c in "hello".chars() { t.print(c as u32).unwrap(); }
        t.set_cursor_pos(0, 0);

        // Make newlines so we create scrollback
        // 3 pushes hello off the screen
        t.index().unwrap();
        t.index().unwrap();
        t.index().unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!("", str);
        }

        // Scroll to the top
        t.screens.active.scroll(screen::Scroll::Top);
        {
            let str = t.plain_string().unwrap();
            assert_eq!("hello", str);
        }

        // Type
        t.print('A' as u32).unwrap();
        t.screens.active.scroll(screen::Scroll::Active);
        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nA", str);
        }

        assert!(t.is_dirty(act(t.screens.active.cursor.x, t.screens.active.cursor.y)));
    }

    #[test]
    fn terminal_print_charset() {
        let mut t = init(80, 80);

        // G1 should have no effect
        t.configure_charset(charsets::Slots::G1, charsets::Charset::DecSpecial);
        t.configure_charset(charsets::Slots::G2, charsets::Charset::DecSpecial);
        t.configure_charset(charsets::Slots::G3, charsets::Charset::DecSpecial);

        // No dirty to configure charset
        assert!(!t.is_dirty(scr(0, 0)));

        // Basic grid writing
        t.print('`' as u32).unwrap();
        t.configure_charset(charsets::Slots::G0, charsets::Charset::Utf8);
        t.print('`' as u32).unwrap();
        t.configure_charset(charsets::Slots::G0, charsets::Charset::Ascii);
        t.print('`' as u32).unwrap();
        t.configure_charset(charsets::Slots::G0, charsets::Charset::DecSpecial);
        t.print('`' as u32).unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!("```◆", str);
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_charset_outside_of_ascii() {
        let mut t = init(80, 80);

        // G1 should have no effect
        t.configure_charset(charsets::Slots::G1, charsets::Charset::DecSpecial);
        t.configure_charset(charsets::Slots::G2, charsets::Charset::DecSpecial);
        t.configure_charset(charsets::Slots::G3, charsets::Charset::DecSpecial);

        // No dirty to configure charset
        assert!(!t.is_dirty(scr(0, 0)));

        // Basic grid writing
        t.configure_charset(charsets::Slots::G0, charsets::Charset::DecSpecial);
        t.print('`' as u32).unwrap();
        t.print(0x1F600).unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!("◆😀", str);
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_wide_codepoint_in_a_charset_prints_it_unmapped() {
        let mut t = init(10, 2);

        // The charset has no such codepoint, so it prints as it is, two
        // columns wide, and the characters after it are mapped again.
        t.configure_charset(charsets::Slots::G0, charsets::Charset::DecSpecial);
        t.print('`' as u32).unwrap();
        t.print(0x1F600).unwrap();
        t.print('a' as u32).unwrap();
        assert_eq!(4, t.screens.active.cursor.x);
        let cell = cl(t.screens.active.pages.get_cell(scr(1, 0)).unwrap().cell);
        assert_eq!(0x1F600, cell.codepoint());
        assert_eq!(Wide::Wide, cell.wide());
        {
            let str = t.plain_string().unwrap();
            assert_eq!("◆😀▒", str);
        }
    }

    #[test]
    fn terminal_print_single_shift_is_used_up_by_a_combining_character() {
        // As in xterm, the single shift applies to the combining mark, the next
        // character printed, whether or not it clusters with the one before.
        for cluster in [false, true] {
            let mut t = init(10, 2);
            t.modes.set(Mode::GraphemeCluster, cluster);

            t.configure_charset(charsets::Slots::G2, charsets::Charset::British);
            t.print('#' as u32).unwrap();
            t.invoke_charset(charsets::ActiveSlot::GL, charsets::Slots::G2, true);
            t.print(0x0301).unwrap();
            assert_eq!(None, t.screens.active.charset.single_shift);
            t.print('#' as u32).unwrap();

            assert_eq!(2, t.screens.active.cursor.x);
            let cell = cl(t.screens.active.pages.get_cell(scr(1, 0)).unwrap().cell);
            assert_eq!('#' as u32, cell.codepoint());
        }
    }

    #[test]
    fn terminal_vs16_moving_a_character_to_the_next_line_keeps_it_unmapped() {
        let mut t = init(3, 5);

        // The '#' is printed with ASCII in the last column. VS16 widens it after
        // the British set (which maps '#' to '£') is selected, which moves it to
        // the next line: it is the same character, not one to map again.
        t.modes.set(Mode::GraphemeCluster, true);
        t.cursor_right(2);
        t.print('#' as u32).unwrap();
        t.configure_charset(charsets::Slots::G0, charsets::Charset::British);
        t.print(0xFE0F).unwrap();

        let cell = cl(t.screens.active.pages.get_cell(scr(0, 1)).unwrap().cell);
        assert_eq!('#' as u32, cell.codepoint());
        assert_eq!(Wide::Wide, cell.wide());
    }

    #[test]
    fn terminal_print_invoke_charset() {
        let mut t = init(80, 80);

        t.configure_charset(charsets::Slots::G1, charsets::Charset::DecSpecial);

        t.print('`' as u32).unwrap();

        // Invokecharset but should not mark dirty on its own
        t.clear_dirty();
        t.invoke_charset(charsets::ActiveSlot::GL, charsets::Slots::G1, false);
        assert!(!t.is_dirty(scr(0, 0)));
        t.print('`' as u32).unwrap();
        assert!(t.is_dirty(scr(0, 0)));
        t.print('`' as u32).unwrap();
        t.invoke_charset(charsets::ActiveSlot::GL, charsets::Slots::G0, false);
        t.print('`' as u32).unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!("`◆◆`", str);
        }
    }

    #[test]
    fn terminal_print_invoke_charset_single() {
        let mut t = init(80, 80);

        t.configure_charset(charsets::Slots::G1, charsets::Charset::DecSpecial);

        // Basic grid writing
        t.print('`' as u32).unwrap();
        t.invoke_charset(charsets::ActiveSlot::GL, charsets::Slots::G1, true);
        t.print('`' as u32).unwrap();
        t.print('`' as u32).unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!("`◆`", str);
        }
    }

    #[test]
    fn terminal_print_kitty_unicode_placeholder() {
        if !build_options::KITTY_GRAPHICS {
            return;
        }

        let mut t = init(10, 10);

        t.print(kitty::graphics::unicode::PLACEHOLDER).unwrap();
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(1, t.screens.active.cursor.x);

        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(kitty::graphics::unicode::PLACEHOLDER, cell.content_codepoint());
            assert!(rw(list_cell.row).kitty_virtual_placeholder());
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_soft_wrap() {
        let mut t = init(3, 80);

        // Basic grid writing
        for c in "hello".chars() { t.print(c as u32).unwrap(); }
        assert_eq!(1, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);
        {
            let str = t.plain_string().unwrap();
            assert_eq!("hel\nlo", str);
        }
    }

    #[test]
    fn terminal_soft_wrap_with_semantic_prompt() {
        let mut t = init(3, 80);

        // Mark our prompt.
        t.semantic_prompt(&osc::SemanticPrompt::init(osc_sp::Action::PromptStart)).unwrap();
        // Should not make anything dirty on its own.
        assert!(!t.is_dirty(scr(0, 0)));

        // Write and wrap
        for c in "hello".chars() { t.print(c as u32).unwrap(); }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 0)).unwrap();
            assert_eq!(RowSemanticPrompt::Prompt, rw(list_cell.row).semantic_prompt());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 1)).unwrap();
            assert_eq!(RowSemanticPrompt::PromptContinuation, rw(list_cell.row).semantic_prompt());
        }
    }

    #[test]
    fn terminal_disabled_wraparound_with_wide_char_and_one_space() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, false);

        // This puts our cursor at the end and there is NO SPACE for a
        // wide character.
        t.print_string("AAAA").unwrap();
        t.clear_dirty();
        t.print(0x1F6A8).unwrap(); // Police car light
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(4, t.screens.active.cursor.x);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("AAAA", str);
        }

        // Make sure we printed nothing
        {
            let list_cell = t.screens.active.pages.get_cell(scr(4, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }

        // Should not be dirty since we didn't modify anything
        assert!(!t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_disabled_wraparound_with_wide_char_and_no_space() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, false);

        // This puts our cursor at the end and there is NO SPACE for a
        // wide character.
        t.print_string("AAAAA").unwrap();
        t.clear_dirty();
        t.print(0x1F6A8).unwrap(); // Police car light
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(4, t.screens.active.cursor.x);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("AAAAA", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(4, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('A' as u32, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }

        // Should not be dirty since we didn't modify anything
        assert!(!t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_disabled_wraparound_with_wide_grapheme_and_half_space() {
        let mut t = init(5, 5);

        t.modes.set(Mode::GraphemeCluster, true);
        t.modes.set(Mode::Wraparound, false);

        // This puts our cursor at the end and there is NO SPACE for a
        // wide character.
        t.print_string("AAAA").unwrap();
        t.print(0x2764).unwrap(); // Heart
        t.clear_dirty();
        t.print(0xFE0F).unwrap(); // VS16 to make wide
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(4, t.screens.active.cursor.x);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("AAAA❤", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(scr(4, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!('❤' as u32, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());
        }

        // Should not be dirty since we didn't modify anything
        assert!(!t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_with_wraparound_disabled_clears_pending_wrap_after_cbt() {
        for wrap_on_fill in [true, false] {
            let mut t = init(6, 2);

            t.modes.set(Mode::Wraparound, wrap_on_fill);
            t.print_string("abcdef").unwrap();
            t.modes.set(Mode::Wraparound, false);
            // CBT preserves pending wrap even though the cursor leaves the margin.
            t.horizontal_tab_back();
            assert!(t.screens.active.cursor.pending_wrap);
            t.print('x' as u32).unwrap();
            assert!(!t.screens.active.cursor.pending_wrap);

            t.modes.set(Mode::Wraparound, true);
            t.print('y' as u32).unwrap();
            assert_eq!(0, t.screens.active.cursor.y);
            assert_eq!(2, t.screens.active.cursor.x);

            let str = t.plain_string().unwrap();
            assert_eq!("xycdef", str);
        }
    }

    #[test]
    fn terminal_print_right_margin_wrap() {
        let mut t = init(10, 5);

        t.print_string("123456789").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(3, 5);
        t.set_cursor_pos(1, 5);
        t.print_string("XY").unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("1234X6789\n  Y", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(act(0, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(!row.wrap());
        }
    }

    #[test]
    fn terminal_print_right_margin_wrap_dirty_tracking() {
        let mut t = init(10, 5);

        t.print_string("123456789").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(3, 5);
        t.set_cursor_pos(1, 5);

        // Writing our X on the first line should mark only that line dirty.
        t.clear_dirty();
        t.print('X' as u32).unwrap();
        assert!(t.is_dirty(scr(4, 0)));
        assert!(!t.is_dirty(scr(2, 1)));

        // Writing our Y should wrap. It marks both rows dirty because the
        // cursor moved.
        t.clear_dirty();
        t.print('Y' as u32).unwrap();
        assert!(t.is_dirty(scr(4, 0)));
        assert!(t.is_dirty(scr(2, 1)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("1234X6789\n  Y", str);
        }
    }

    #[test]
    fn terminal_print_right_margin_outside() {
        let mut t = init(10, 5);

        t.print_string("123456789").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(3, 5);
        t.set_cursor_pos(1, 6);
        t.clear_dirty();
        t.print_string("XY").unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("12345XY89", str);
        }

        assert!(t.is_dirty(scr(5, 0)));
    }

    #[test]
    fn terminal_print_right_margin_outside_wrap() {
        let mut t = init(10, 5);

        t.print_string("123456789").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(3, 5);
        t.set_cursor_pos(1, 10);
        t.print_string("XY").unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("123456789X\n  Y", str);
        }
    }

    #[test]
    fn terminal_print_wide_char_at_right_margin_does_not_create_spacer_head() {
        let mut t = init(10, 10);

        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(3, 5);
        t.set_cursor_pos(1, 5);
        t.print(0x1F600).unwrap(); // Smiley face
        assert_eq!(1, t.screens.active.cursor.y);
        assert_eq!(4, t.screens.active.cursor.x);

        // Both rows dirty because the cursor moved
        assert!(t.is_dirty(scr(4, 0)));
        assert!(t.is_dirty(scr(4, 1)));

        {
            let list_cell = t.screens.active.pages.get_cell(scr(4, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0, cell.content_codepoint());
            assert_eq!(Wide::Narrow, cell.wide());

            let row = rw(list_cell.row);
            assert!(!row.wrap());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(2, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(0x1F600, cell.content_codepoint());
            assert_eq!(Wide::Wide, cell.wide());
        }
        {
            let list_cell = t.screens.active.pages.get_cell(scr(3, 1)).unwrap();
            let cell = cl(list_cell.cell);
            assert_eq!(Wide::SpacerTail, cell.wide());
        }
    }

    #[test]
    fn terminal_print_with_hyperlink() {
        let mut t = init(80, 80);

        // Setup our hyperlink and print
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print_string("123456").unwrap();

        // Verify all our cells have a hyperlink
        for x in 0..6 {
            let list_cell = t.screens.active.pages.get_cell(scr(x, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
            assert_eq!(1, id);
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_over_cell_with_same_hyperlink() {
        let mut t = init(80, 80);

        // Setup our hyperlink and print
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print_string("123456").unwrap();
        t.set_cursor_pos(1, 1);
        t.print_string("123456").unwrap();

        // Verify all our cells have a hyperlink
        for x in 0..6 {
            let list_cell = t.screens.active.pages.get_cell(scr(x, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
            assert_eq!(1, id);
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_and_end_hyperlink() {
        let mut t = init(80, 80);

        // Setup our hyperlink and print
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print_string("123").unwrap();
        t.screens.active.end_hyperlink();
        t.print_string("456").unwrap();

        // Verify all our cells have a hyperlink
        for x in 0..3 {
            let list_cell = t.screens.active.pages.get_cell(scr(x, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
            assert_eq!(1, id);
        }
        for x in 3..6 {
            let list_cell = t.screens.active.pages.get_cell(scr(x, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(!cell.hyperlink());
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_print_and_change_hyperlink() {
        let mut t = init(80, 80);

        // Setup our hyperlink and print
        t.screens.active.start_hyperlink(b"http://one.example.com", None).unwrap();
        t.print_string("123").unwrap();
        t.screens.active.start_hyperlink(b"http://two.example.com", None).unwrap();
        t.print_string("456").unwrap();

        // Verify all our cells have a hyperlink
        for x in 0..3 {
            let list_cell = t.screens.active.pages.get_cell(scr(x, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert!(cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
            assert_eq!(1, id);
        }
        for x in 3..6 {
            let list_cell = t.screens.active.pages.get_cell(scr(x, 0)).unwrap();
            let cell = cl(list_cell.cell);
            assert!(cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
            assert_eq!(2, id);
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    #[test]
    fn terminal_overwrite_hyperlink() {
        let mut t = init(80, 80);

        // Setup our hyperlink and print
        t.screens.active.start_hyperlink(b"http://one.example.com", None).unwrap();
        t.print_string("123").unwrap();
        t.set_cursor_pos(1, 1);
        t.screens.active.end_hyperlink();
        t.print_string("456").unwrap();

        // Verify all our cells have a hyperlink
        for x in 0..3 {
            let list_cell = t.screens.active.pages.get_cell(scr(x, 0)).unwrap();
            let page = nd(list_cell.node).page();
            let row = rw(list_cell.row);
            assert!(!row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(!cell.hyperlink());
            assert!(page.lookup_hyperlink(cell).is_none());
            assert_eq!(0, page.hyperlink_set.count());
        }

        assert!(t.is_dirty(scr(0, 0)));
    }

    // Printing a wide char at the right edge with an active hyperlink causes
    // writeCell to write a spacer_head before printWrap sets the row wrap
    // flag. The integrity check inside setHyperlink (or increaseCapacity)
    // sees the unwrapped spacer head and panics. Found via fuzzing.
    #[test]
    fn terminal_print_wide_char_at_right_edge_with_hyperlink() {
        let mut t = init(10, 5);

        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();

        // Move cursor to the last column (1-indexed)
        t.set_cursor_pos(1, 10);

        // Print a wide character; this will call writeCell(0, .spacer_head)
        // at the right edge before calling printWrap, triggering the
        // integrity violation.
        t.print(0x4E2D).unwrap(); // U+4E2D '中'

        // Cursor wraps to row 2, after the wide char + spacer tail
        assert_eq!(1, t.screens.active.cursor.y);
        assert_eq!(2, t.screens.active.cursor.x);

        // Row 0, col 9: spacer head with hyperlink
        {
            let list_cell = t.screens.active.pages.get_cell(scr(9, 0)).unwrap();
            assert_eq!(Wide::SpacerHead, cl(list_cell.cell).wide());
            assert!(cl(list_cell.cell).hyperlink());
            assert!(rw(list_cell.row).wrap());
        }
        // Row 1, col 0: the wide char with hyperlink
        {
            let list_cell = t.screens.active.pages.get_cell(scr(0, 1)).unwrap();
            assert_eq!(0x4E2D, cl(list_cell.cell).content_codepoint());
            assert_eq!(Wide::Wide, cl(list_cell.cell).wide());
            assert!(cl(list_cell.cell).hyperlink());
        }
        // Row 1, col 1: spacer tail with hyperlink
        {
            let list_cell = t.screens.active.pages.get_cell(scr(1, 1)).unwrap();
            assert_eq!(Wide::SpacerTail, cl(list_cell.cell).wide());
            assert!(cl(list_cell.cell).hyperlink());
        }
    }

    // A cursor style or hyperlink ID is an index into a set stored in the
    // page memory of the page the cursor pin points at. When scrollClear
    // (here via ED 22, kitty's scroll_complete) pushes the active area onto
    // a later page while the cursor pin is still on an earlier one,
    // cursorReload must migrate the cursor's style and hyperlink references
    // to the destination page. It previously replaced the pin directly,
    // leaving the cursor holding an ID that was dead or aliased an unrelated
    // entry on the new page, and the next print attached a live cell to it.
    // Found via fuzzing.
    #[test]
    fn terminal_scrollclear_across_pages_keeps_cursor_hyperlink_refs_page_local() {
        // Minimized from a 774-byte AFL fuzz input. Reading it:
        //
        //   A                 print, so REP has something to repeat
        //   ESC [ 48111 b     REP, filling the page and spilling onto a second
        //   ESC ] 8 ; ; 0x93  OSC 8; the C1 byte terminates the OSC and makes
        //                     the URI non-empty, so a hyperlink starts
        //   ESC [ 11 A        CUU, moving the cursor back onto the first page
        //   ESC [ 22 J        ED 22, i.e. scroll_complete -> Screen.scrollClear
        //   B                 print, which attaches the cursor hyperlink
        //   ESC ] 8 ; ; ESC   OSC 8 with an empty URI, ending the hyperlink
        //
        // The grid must be wide enough to fill a page from a single REP, so
        // this does not reproduce at 80x24.
        //
        // Port note: the Zig test feeds the input above through `vtStream`,
        // which is provided by the (not yet ported) stream_terminal handler.
        // The equivalent Terminal operations are performed directly here.
        let mut t = init(200, 50);

        {
            t.print('A' as u32).unwrap();
            t.print_repeat(48111).unwrap();
            t.screens.active.start_hyperlink(b"\xC2\x93", None).unwrap();
            t.cursor_up(11);
            t.erase_display(csi::EraseDisplay::ScrollComplete, false);
            t.print('B' as u32).unwrap();
            t.screens.active.end_hyperlink();
        }

        // With slow runtime safety on, the page integrity checks during the
        // stream above already catch the bug. Verify the ref counts explicitly
        // as well so this test is meaningful with runtime safety off: every
        // cell holding a hyperlink ID owns a reference, so a count below the
        // number of holding cells means a live cell points at an entry that
        // was already freed.
        let mut node_ = t.screens.active.pages.pages.first;
        while let Some(node) = node_ {
            node_ = nd(node).next;
            let page = nd(node).page();
            let cap = page.hyperlink_set.layout.cap;
            if cap == 0 {
                continue;
            }

            let mut holders = vec![0u32; cap];

            for y in 0..page.size.rows as usize {
                let row = rw(page.get_row(y));
                if !row.hyperlink() {
                    continue;
                }
                // SAFETY: the row belongs to the page.
                let cells = unsafe { &*page.get_cells(row) };
                for cell in &cells[..page.size.cols as usize] {
                    if !cell.hyperlink() {
                        continue;
                    }
                    let Some(id) = page.lookup_hyperlink(cell) else { continue };
                    if (id as usize) < cap {
                        holders[id as usize] += 1;
                    }
                }
            }

            for (id, &held) in holders.iter().enumerate() {
                if held == 0 {
                    continue;
                }
                // SAFETY: the id is in this page's set.
                let refs = unsafe { page.hyperlink_set.ref_count(page.memory, id as _) };
                assert!(refs as u32 >= held);
            }
        }

        // If the cursor still has an active hyperlink, its own extra
        // reference must live on the cursor's page.
        let cursor = &t.screens.active.cursor;
        if cursor.hyperlink_id != 0 {
            let page = nd(tp(cursor.page_pin).node).page();
            // SAFETY: the id is in this page's set.
            assert!(unsafe { page.hyperlink_set.ref_count(page.memory, cursor.hyperlink_id) } > 0);
        }
    }

    #[test]
    fn terminal_linefeed_and_carriage_return() {
        let mut t = init(80, 80);

        // Print and CR.
        for c in "hello".chars() { t.print(c as u32).unwrap(); }
        t.clear_dirty();
        t.carriage_return();

        // CR should not mark row dirty because it doesn't change rendering.
        assert!(!t.is_dirty(scr(0, 0)));

        t.linefeed().unwrap();

        // LF marks row dirty due to cursor movement
        assert!(t.is_dirty(scr(0, 0)));
        assert!(t.is_dirty(scr(0, 1)));

        for c in "world".chars() { t.print(c as u32).unwrap(); }
        assert_eq!(1, t.screens.active.cursor.y);
        assert_eq!(5, t.screens.active.cursor.x);
        {
            let str = t.plain_string().unwrap();
            assert_eq!("hello\nworld", str);
        }
    }

    #[test]
    fn terminal_linefeed_unsets_pending_wrap() {
        let mut t = init(5, 80);

        // Basic grid writing
        for c in "hello".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap == true);
        t.clear_dirty();
        t.linefeed().unwrap();
        assert!(t.is_dirty(scr(0, 0)));
        assert!(t.is_dirty(scr(0, 1)));
        assert!(t.screens.active.cursor.pending_wrap == false);
    }

    #[test]
    fn terminal_linefeed_mode_automatic_carriage_return() {
        let mut t = init(10, 10);

        // Basic grid writing
        t.modes.set(Mode::Linefeed, true);
        t.print_string("123456").unwrap();
        t.linefeed().unwrap();
        t.print('X' as u32).unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!("123456\nX", str);
        }
    }

    #[test]
    fn terminal_carriage_return_unsets_pending_wrap() {
        let mut t = init(5, 80);

        // Basic grid writing
        for c in "hello".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap == true);
        t.carriage_return();
        assert!(t.screens.active.cursor.pending_wrap == false);
    }

    #[test]
    fn terminal_carriage_return_origin_mode_moves_to_left_margin() {
        let mut t = init(5, 80);

        t.modes.set(Mode::Origin, true);
        t.screens.active.cursor.x = 0;
        t.scrolling_region.left = 2;
        t.carriage_return();
        assert_eq!(2, t.screens.active.cursor.x);
    }

    #[test]
    fn terminal_carriage_return_left_of_left_margin_moves_to_zero() {
        let mut t = init(5, 80);

        t.screens.active.cursor.x = 1;
        t.scrolling_region.left = 2;
        t.carriage_return();
        assert_eq!(0, t.screens.active.cursor.x);
    }

    #[test]
    fn terminal_carriage_return_right_of_left_margin_moves_to_left_margin() {
        let mut t = init(5, 80);

        t.screens.active.cursor.x = 3;
        t.scrolling_region.left = 2;
        t.carriage_return();
        assert_eq!(2, t.screens.active.cursor.x);
    }

    #[test]
    fn terminal_backspace() {
        let mut t = init(80, 80);

        // BS
        for c in "hello".chars() { t.print(c as u32).unwrap(); }
        t.backspace();
        t.print('y' as u32).unwrap();
        assert_eq!(0, t.screens.active.cursor.y);
        assert_eq!(5, t.screens.active.cursor.x);
        {
            let str = t.plain_string().unwrap();
            assert_eq!("helly", str);
        }
    }

    #[test]
    fn terminal_horizontal_tabs() {
        let mut t = init(20, 5);

        // HT
        t.print('1' as u32).unwrap();
        t.horizontal_tab();
        assert_eq!(8, t.screens.active.cursor.x);

        // HT
        t.horizontal_tab();
        assert_eq!(16, t.screens.active.cursor.x);

        // HT at the end
        t.horizontal_tab();
        assert_eq!(19, t.screens.active.cursor.x);
        t.horizontal_tab();
        assert_eq!(19, t.screens.active.cursor.x);
    }

    #[test]
    fn terminal_horizontal_tabs_starting_on_tabstop() {
        let mut t = init(20, 5);

        t.set_cursor_pos(t.screens.active.cursor.y as usize, 9);
        t.print('X' as u32).unwrap();
        t.set_cursor_pos(t.screens.active.cursor.y as usize, 9);
        t.horizontal_tab();
        t.print('A' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("        X       A", str);
        }
    }

    #[test]
    fn terminal_horizontal_tabs_with_right_margin() {
        let mut t = init(20, 5);

        t.scrolling_region.left = 2;
        t.scrolling_region.right = 5;
        t.set_cursor_pos(t.screens.active.cursor.y as usize, 1);
        t.print('X' as u32).unwrap();
        t.horizontal_tab();
        t.print('A' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X    A", str);
        }
    }

    #[test]
    fn terminal_horizontal_tabs_back() {
        let mut t = init(20, 5);

        // Edge of screen
        t.set_cursor_pos(t.screens.active.cursor.y as usize, 20);

        // HT
        t.horizontal_tab_back();
        assert_eq!(16, t.screens.active.cursor.x);

        // HT
        t.horizontal_tab_back();
        assert_eq!(8, t.screens.active.cursor.x);

        // HT
        t.horizontal_tab_back();
        assert_eq!(0, t.screens.active.cursor.x);
        t.horizontal_tab_back();
        assert_eq!(0, t.screens.active.cursor.x);
    }

    #[test]
    fn terminal_horizontal_tabs_back_starting_on_tabstop() {
        let mut t = init(20, 5);

        t.set_cursor_pos(t.screens.active.cursor.y as usize, 9);
        t.print('X' as u32).unwrap();
        t.set_cursor_pos(t.screens.active.cursor.y as usize, 9);
        t.horizontal_tab_back();
        t.print('A' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A       X", str);
        }
    }

    #[test]
    fn terminal_horizontal_tabs_with_left_margin_in_origin_mode() {
        let mut t = init(20, 5);

        t.modes.set(Mode::Origin, true);
        t.scrolling_region.left = 2;
        t.scrolling_region.right = 5;
        t.set_cursor_pos(1, 2);
        t.print('X' as u32).unwrap();
        t.horizontal_tab_back();
        t.print('A' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("  AX", str);
        }
    }

    #[test]
    fn terminal_horizontal_tab_back_with_cursor_before_left_margin() {
        let mut t = init(20, 5);

        t.modes.set(Mode::Origin, true);
        t.save_cursor();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(5, 0);
        t.restore_cursor();
        t.horizontal_tab_back();
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X", str);
        }
    }

    #[test]
    fn terminal_cursorpos_resets_wrap() {
        let mut t = init(5, 5);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.set_cursor_pos(1, 1);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("XBCDE", str);
        }
    }

    #[test]
    fn terminal_cursorpos_off_the_screen() {
        let mut t = init(5, 5);

        t.set_cursor_pos(500, 500);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\n\n\n    X", str);
        }
    }

    #[test]
    fn terminal_cursorpos_relative_to_origin() {
        let mut t = init(5, 5);

        t.scrolling_region.top = 2;
        t.scrolling_region.bottom = 3;
        t.modes.set(Mode::Origin, true);
        t.set_cursor_pos(1, 1);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\nX", str);
        }
    }

    #[test]
    fn terminal_cursorpos_relative_to_origin_with_left_right() {
        let mut t = init(5, 5);

        t.scrolling_region.top = 2;
        t.scrolling_region.bottom = 3;
        t.scrolling_region.left = 2;
        t.scrolling_region.right = 4;
        t.modes.set(Mode::Origin, true);
        t.set_cursor_pos(1, 1);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\n  X", str);
        }
    }

    #[test]
    fn terminal_cursorpos_limits_with_full_scroll_region() {
        let mut t = init(5, 5);

        t.scrolling_region.top = 2;
        t.scrolling_region.bottom = 3;
        t.scrolling_region.left = 2;
        t.scrolling_region.right = 4;
        t.modes.set(Mode::Origin, true);
        t.set_cursor_pos(500, 500);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\n\n    X", str);
        }
    }

    // Probably outdated, but dates back to the original terminal implementation.
    #[test]
    fn terminal_setcursorpos_original_test() {
        let mut t = init(80, 80);

        assert_eq!(0, t.screens.active.cursor.x);
        assert_eq!(0, t.screens.active.cursor.y);

        // Setting it to 0 should keep it zero (1 based)
        t.set_cursor_pos(0, 0);
        assert_eq!(0, t.screens.active.cursor.x);
        assert_eq!(0, t.screens.active.cursor.y);

        // Should clamp to size
        t.set_cursor_pos(81, 81);
        assert_eq!(79, t.screens.active.cursor.x);
        assert_eq!(79, t.screens.active.cursor.y);

        // Should reset pending wrap
        t.set_cursor_pos(0, 80);
        t.print('c' as u32).unwrap();
        assert!(t.screens.active.cursor.pending_wrap);
        t.set_cursor_pos(0, 80);
        assert!(!t.screens.active.cursor.pending_wrap);

        // Origin mode
        t.modes.set(Mode::Origin, true);

        // No change without a scroll region
        t.set_cursor_pos(81, 81);
        assert_eq!(79, t.screens.active.cursor.x);
        assert_eq!(79, t.screens.active.cursor.y);

        // Set the scroll region
        t.set_top_and_bottom_margin(10, t.rows as usize);
        t.set_cursor_pos(0, 0);
        assert_eq!(0, t.screens.active.cursor.x);
        assert_eq!(9, t.screens.active.cursor.y);

        t.set_cursor_pos(1, 1);
        assert_eq!(0, t.screens.active.cursor.x);
        assert_eq!(9, t.screens.active.cursor.y);

        t.set_cursor_pos(100, 0);
        assert_eq!(0, t.screens.active.cursor.x);
        assert_eq!(79, t.screens.active.cursor.y);

        t.set_top_and_bottom_margin(10, 11);
        t.set_cursor_pos(2, 0);
        assert_eq!(0, t.screens.active.cursor.x);
        assert_eq!(10, t.screens.active.cursor.y);
    }

    #[test]
    fn terminal_settopandbottommargin_simple() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_top_and_bottom_margin(0, 0);

        t.clear_dirty();
        t.scroll_down(1);

        // Mark the rows we moved as dirty.
        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nABC\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_settopandbottommargin_top_only() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_top_and_bottom_margin(2, 0);

        t.clear_dirty();
        t.scroll_down(1);

        // This is dirty because the cursor moves from this row
        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\n\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_settopandbottommargin_top_and_bottom() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_top_and_bottom_margin(1, 2);

        t.clear_dirty();
        t.scroll_down(1);

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(!t.is_dirty(act(0, 2)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nABC\nGHI", str);
        }
    }

    #[test]
    fn terminal_settopandbottommargin_top_equal_to_bottom() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_top_and_bottom_margin(2, 2);

        t.clear_dirty();
        t.scroll_down(1);

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nABC\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_setleftandrightmargin_simple() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(0, 0);

        t.clear_dirty();
        t.erase_chars(1);

        assert!(t.is_dirty(act(0, 0)));
        assert!(!t.is_dirty(act(0, 1)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!(" BC\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_setleftandrightmargin_left_only() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(2, 0);
        assert_eq!(1, t.scrolling_region.left);
        assert_eq!(t.cols - 1, t.scrolling_region.right);
        t.set_cursor_pos(1, 2);

        t.clear_dirty();
        t.insert_lines(1);

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\nDBC\nGEF\n HI", str);
        }
    }

    #[test]
    fn terminal_setleftandrightmargin_left_and_right() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(1, 2);
        t.set_cursor_pos(1, 2);

        t.clear_dirty();
        t.insert_lines(1);

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("  C\nABF\nDEI\nGH", str);
        }
    }

    #[test]
    fn terminal_setleftandrightmargin_left_equal_right() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(2, 2);
        t.set_cursor_pos(1, 2);

        t.clear_dirty();
        t.insert_lines(1);

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nABC\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_setleftandrightmargin_mode_69_unset() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, false);
        t.set_left_and_right_margin(1, 2);
        t.set_cursor_pos(1, 2);

        t.clear_dirty();
        t.insert_lines(1);

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nABC\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_insertlines_simple() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_cursor_pos(2, 2);

        let node = tp(t.screens.active.cursor.page_pin).node;
        let serial = nd(node).serial;
        t.clear_dirty();
        t.insert_lines(1);
        assert!(!t.screens.active.pages.node_is_valid(node, serial));

        assert!(!t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\n\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_insertlines_colors_with_bg_color() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_cursor_pos(2, 2);

        t.set_attribute(sgr::Attribute::DirectColorBg(color::Rgb {
            r: 0xFF,
            g: 0,
            b: 0,
        })).unwrap();
        t.insert_lines(1);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\n\nDEF\nGHI", str);
        }

        for x in 0..t.cols {
            let list_cell = t.screens.active.pages.get_cell(act(x, 1)).unwrap();
            assert!(cl(list_cell.cell).content_tag() == pagepkg::ContentTag::BgColorRgb);
            assert_eq!(color::Rgb {
                r: 0xFF,
                g: 0,
                b: 0,
            }, cl(list_cell.cell).content_color_rgb());
        }
    }

    #[test]
    fn terminal_insertlines_handles_style_refs() {
        let mut t = init(5, 3);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();

        // For the line being deleted, create a refcounted style
        t.set_attribute(sgr::Attribute::Bold).unwrap();
        t.print_string("GHI").unwrap();
        t.set_attribute(sgr::Attribute::Unset).unwrap();

        // verify we have styles in our style map
        let page = cursor_page(t.screens.active);
        assert_eq!(1, page.styles.count());

        t.set_cursor_pos(2, 2);
        t.insert_lines(1);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\n\nDEF", str);
        }

        // verify we have no styles in our style map
        assert_eq!(0, page.styles.count());
    }

    #[test]
    fn terminal_insertlines_outside_of_scroll_region() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_top_and_bottom_margin(3, 4);
        t.set_cursor_pos(2, 2);

        t.clear_dirty();
        t.insert_lines(1);

        assert!(!t.is_dirty(act(0, 0)));
        assert!(!t.is_dirty(act(0, 1)));
        assert!(!t.is_dirty(act(0, 2)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_insertlines_top_bottom_scroll_region() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("123").unwrap();
        t.set_top_and_bottom_margin(1, 3);
        t.set_cursor_pos(2, 2);

        t.clear_dirty();
        t.insert_lines(1);

        assert!(!t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(!t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\n\nDEF\n123", str);
        }
    }

    #[test]
    fn terminal_insertlines_across_page_boundary_marks_all_shifted_rows_dirty() {
        let mut t = Terminal::init(Options {
            rows: 5,
            cols: 10,
            max_scrollback_bytes: Some(1024),
            ..Default::default()
        })
        .unwrap();

        let first_page = t.screens.active.pages.pages.first.unwrap();
        let first_page_nrows = nd(first_page).capacity().rows;

        // Fill up the first page minus 3 rows
        for _ in 0..first_page_nrows - 3 {
            t.linefeed().unwrap();
        }

        // Add content that will cross a page boundary
        t.print_string("1AAAA").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("2BBBB").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("3CCCC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("4DDDD").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("5EEEE").unwrap();

        // Verify we now have a second page
        let second_page = nd(first_page).next.unwrap();
        let first_serial = nd(first_page).serial;
        let second_serial = nd(second_page).serial;

        t.set_cursor_pos(1, 1);
        t.clear_dirty();
        t.insert_lines(1);
        assert!(!t.screens.active.pages.node_is_valid(first_page, first_serial));
        assert!(!t.screens.active.pages.node_is_valid(second_page, second_serial));

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));
        assert!(t.is_dirty(act(0, 4)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n1AAAA\n2BBBB\n3CCCC\n4DDDD", str);
        }
    }

    #[test]
    fn terminal_insertlines_hyperlink_dense_row_crosses_page_boundary() {
        // Regression test for the cross-page copy of insertLines: when the
        // shifted row carries more unique hyperlinks than the destination
        // page's hyperlink capacity, the copy must increase the destination
        // page's capacity and retry rather than corrupting the page list.
        let mut t = Terminal::init(Options {
            rows: 5,
            cols: 10,
            max_scrollback_bytes: Some(1024),
            ..Default::default()
        })
        .unwrap();

        let sp = t.screens.active;
        let pages = &sp.pages;

        // Fill the first page so it is exactly full, then two more rows so
        // the second page holds the last two active rows (y=3 and y=4).
        let first_page_rows = nd(pages.pages.first.unwrap()).capacity().rows;
        for _ in 0..first_page_rows + 1 {
            t.linefeed().unwrap();
        }
        assert!(pages.pages.first != pages.pages.last);
        assert_eq!(2, nd(pages.pages.last.unwrap()).rows());

        // Marker rows so we can verify the shift afterwards.
        t.set_cursor_pos(1, 1);
        t.print_string("0").unwrap();
        t.set_cursor_pos(2, 1);
        t.print_string("1").unwrap();
        t.set_cursor_pos(4, 1);
        t.print_string("3").unwrap();
        t.set_cursor_pos(5, 1);
        t.print_string("4").unwrap();

        // Fill the last row of the first page (active y=2) with unique
        // hyperlinks: more than the second page can hold with its default
        // hyperlink capacity. Writing them grows the first page's capacity
        // as needed; the second page keeps its default capacity.
        t.set_cursor_pos(3, 1);
        for i in 0..10u32 {
            let uri = format!("http://example.com/{i}");
            t.screens.active.start_hyperlink(uri.as_bytes(), None).unwrap();
            t.print('A' as u32 + i).unwrap();
            t.screens.active.end_hyperlink();
        }
        {
            let pin = pages.pin(act(0, 2)).unwrap();
            assert_eq!(pages.pages.first.unwrap(), pin.node);
            assert_eq!(nd(pin.node).rows() - 1, pin.y);
        }
        assert!(nd(pages.pages.last.unwrap()).page().hyperlink_set.layout.cap < 10);

        // Insert a line at the top: every row shifts down by one and the
        // dense row crosses the page boundary into the second page.
        t.set_cursor_pos(1, 1);
        t.insert_lines(1);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n0\n1\nABCDEFGHIJ\n3", str);
        }

        // The second page's hyperlink capacity had to grow to receive the
        // row, proving the capacity-retry path ran.
        assert!(nd(pages.pages.last.unwrap()).page().hyperlink_set.layout.cap >= 10);

        // Every cell of the dense row must still resolve to a real
        // hyperlink entry with the correct URI. A half-applied shift
        // leaves cells whose hyperlink flag is set but that have no map
        // entry, which aborts in clearCells later.
        for x in 0..10 {
            let list_cell = pages.get_cell(act(x, 3)).unwrap();
            assert!(cl(list_cell.cell).hyperlink());
            let page: &mut Page = nd(list_cell.node).page();
            let id = page.lookup_hyperlink(cl(list_cell.cell)).unwrap();
            let uri = format!("http://example.com/{x}");
            assert_eq!(uri.as_bytes(), &link_uri(page, id)[..]);
        }

        // All pages must pass integrity checks.
        let mut node_ = pages.pages.first;
        while let Some(node) = node_ {
            nd(node).page().assert_integrity();
            node_ = nd(node).next;
        }
    }

    #[test]
    fn terminal_insertlines_legacy_test() {
        let mut t = init(2, 5);

        // Initial value
        t.print('A' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('B' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('C' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('D' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('E' as u32).unwrap();

        // Move to row 2
        t.set_cursor_pos(2, 1);

        // Insert two lines
        t.insert_lines(2);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\n\n\nB\nC", str);
        }
    }

    #[test]
    fn terminal_insertlines_zero() {
        let mut t = init(2, 5);

        // This should do nothing
        t.set_cursor_pos(1, 1);
        t.insert_lines(0);
    }

    #[test]
    fn terminal_insertlines_with_scroll_region() {
        let mut t = init(2, 6);

        // Initial value
        t.print('A' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('B' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('C' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('D' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('E' as u32).unwrap();

        t.set_top_and_bottom_margin(1, 2);
        t.set_cursor_pos(1, 1);

        t.clear_dirty();
        t.insert_lines(1);

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(!t.is_dirty(act(0, 2)));

        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X\nA\nC\nD\nE", str);
        }
    }

    #[test]
    fn terminal_insertlines_more_than_remaining() {
        let mut t = init(2, 5);

        // Initial value
        t.print('A' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('B' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('C' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('D' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('E' as u32).unwrap();

        // Move to row 2
        t.set_cursor_pos(2, 1);

        // Insert a bunch of  lines
        t.clear_dirty();
        t.insert_lines(20);

        assert!(!t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A", str);
        }
    }

    #[test]
    fn terminal_insertlines_resets_pending_wrap() {
        let mut t = init(5, 5);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.insert_lines(1);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('B' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("B\nABCDE", str);
        }
    }

    #[test]
    fn terminal_insertlines_resets_wrap() {
        let mut t = init(3, 3);

        t.print('1' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        for c in "ABCDEF".chars() { t.print(c as u32).unwrap(); }
        t.set_cursor_pos(1, 1);
        t.insert_lines(1);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X\n1\nABC", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(act(0, 2)).unwrap();
            let row = rw(list_cell.row);
            assert!(!row.wrap());
        }
    }

    #[test]
    fn terminal_insertlines_multi_codepoint_graphemes() {
        let mut t = init(5, 5);

        // Disable grapheme clustering
        t.modes.set(Mode::GraphemeCluster, true);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();

        // This is: 👨‍👩‍👧 (which may or may not render correctly)
        t.print(0x1F468).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F469).unwrap();
        t.print(0x200D).unwrap();
        t.print(0x1F467).unwrap();

        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_cursor_pos(2, 2);
        t.insert_lines(1);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\n\n👨‍👩‍👧\nGHI", str);
        }
    }

    #[test]
    fn terminal_insertlines_left_right_scroll_region() {
        let mut t = init(10, 10);

        t.print_string("ABC123").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF456").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI789").unwrap();
        t.scrolling_region.left = 1;
        t.scrolling_region.right = 3;
        t.set_cursor_pos(2, 2);

        t.clear_dirty();
        t.insert_lines(1);

        assert!(!t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC123\nD   56\nGEF489\n HI7", str);
        }
    }

    #[test]
    fn terminal_scrollup_simple() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_cursor_pos(2, 2);

        let cursor = t.screens.active.cursor.clone();
        let viewport_before = t.screens.active.pages.get_top_left(point::Tag::Viewport);
        t.scroll_up(1).unwrap();
        assert_eq!(cursor.x, t.screens.active.cursor.x);
        assert_eq!(cursor.y, t.screens.active.cursor.y);

        // Viewport should have moved. Our entire page should've scrolled!
        // The viewport moving will cause our render state to make the full
        // frame as dirty.
        let viewport_after = t.screens.active.pages.get_top_left(point::Tag::Viewport);
        assert!(!viewport_before.eql(viewport_after));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("DEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_scrollup_moves_hyperlink() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print_string("DEF").unwrap();
        t.screens.active.end_hyperlink();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_cursor_pos(2, 2);
        t.scroll_up(1).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("DEF\nGHI", str);
        }

        for x in 0..3 {
            let list_cell = t.screens.active.pages.get_cell(vp(x, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
            assert_eq!(1, id);
            let page = nd(list_cell.node).page();
            assert_eq!(1, page.hyperlink_set.count());
        }
        for x in 0..3 {
            let list_cell = t.screens.active.pages.get_cell(vp(x, 1)).unwrap();
            let row = rw(list_cell.row);
            assert!(!row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(!cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell);
            assert!(id.is_none());
        }
    }

    #[test]
    fn terminal_scrollup_clears_hyperlink() {
        let mut t = init(5, 5);

        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print_string("ABC").unwrap();
        t.screens.active.end_hyperlink();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_cursor_pos(2, 2);
        t.scroll_up(1).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("DEF\nGHI", str);
        }

        for x in 0..3 {
            let list_cell = t.screens.active.pages.get_cell(vp(x, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(!row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(!cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell);
            assert!(id.is_none());
        }
    }

    #[test]
    fn terminal_scrollup_top_bottom_scroll_region() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_top_and_bottom_margin(2, 3);
        t.set_cursor_pos(1, 1);

        t.clear_dirty();
        t.scroll_up(1).unwrap();

        // This is dirty because the cursor moves from this row
        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\nGHI", str);
        }
    }

    #[test]
    fn terminal_scrollup_left_right_scroll_region() {
        let mut t = init(10, 10);

        t.print_string("ABC123").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF456").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI789").unwrap();
        t.scrolling_region.left = 1;
        t.scrolling_region.right = 3;
        t.set_cursor_pos(2, 2);

        let cursor = t.screens.active.cursor.clone();
        t.clear_dirty();
        t.scroll_up(1).unwrap();
        assert_eq!(cursor.x, t.screens.active.cursor.x);
        assert_eq!(cursor.y, t.screens.active.cursor.y);

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("AEF423\nDHI756\nG   89", str);
        }
    }

    #[test]
    fn terminal_scrollup_left_right_scroll_region_hyperlink() {
        let mut t = init(10, 10);

        t.print_string("ABC123").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print_string("DEF456").unwrap();
        t.screens.active.end_hyperlink();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI789").unwrap();
        t.scrolling_region.left = 1;
        t.scrolling_region.right = 3;
        t.set_cursor_pos(2, 2);
        t.scroll_up(1).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("AEF423\nDHI756\nG   89", str);
        }

        // First row gets some hyperlinks
        {
            for x in 0..1 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 0)).unwrap();
                let cell = cl(list_cell.cell);
                assert!(!cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell);
                assert!(id.is_none());
            }
            for x in 1..4 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 0)).unwrap();
                let row = rw(list_cell.row);
                assert!(row.hyperlink());
                let cell = cl(list_cell.cell);
                assert!(cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
                assert_eq!(1, id);
                let page = nd(list_cell.node).page();
                assert_eq!(1, page.hyperlink_set.count());
            }
            for x in 4..6 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 0)).unwrap();
                let cell = cl(list_cell.cell);
                assert!(!cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell);
                assert!(id.is_none());
            }
        }

        // Second row preserves hyperlink where we didn't scroll
        {
            for x in 0..1 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 1)).unwrap();
                let row = rw(list_cell.row);
                assert!(row.hyperlink());
                let cell = cl(list_cell.cell);
                assert!(cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
                assert_eq!(1, id);
                let page = nd(list_cell.node).page();
                assert_eq!(1, page.hyperlink_set.count());
            }
            for x in 1..4 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 1)).unwrap();
                let cell = cl(list_cell.cell);
                assert!(!cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell);
                assert!(id.is_none());
            }
            for x in 4..6 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 1)).unwrap();
                let row = rw(list_cell.row);
                assert!(row.hyperlink());
                let cell = cl(list_cell.cell);
                assert!(cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
                assert_eq!(1, id);
                let page = nd(list_cell.node).page();
                assert_eq!(1, page.hyperlink_set.count());
            }
        }
    }

    #[test]
    fn terminal_scrollup_preserves_pending_wrap() {
        let mut t = init(5, 5);

        t.set_cursor_pos(1, 5);
        t.print('A' as u32).unwrap();
        t.set_cursor_pos(2, 5);
        t.print('B' as u32).unwrap();
        t.set_cursor_pos(3, 5);
        t.print('C' as u32).unwrap();
        t.scroll_up(1).unwrap();
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("    B\n    C\n\nX", str);
        }
    }

    #[test]
    fn terminal_scrollup_full_top_bottom_region() {
        let mut t = init(5, 5);

        t.print_string("top").unwrap();
        t.set_cursor_pos(5, 1);
        t.print_string("ABCDE").unwrap();
        t.set_top_and_bottom_margin(2, 5);

        t.clear_dirty();
        t.scroll_up(4).unwrap();

        // This is dirty because the cursor moves from this row
        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("top", str);
        }
    }

    #[test]
    fn terminal_scrollup_full_top_bottomleft_right_scroll_region() {
        let mut t = init(5, 5);

        t.print_string("top").unwrap();
        t.set_cursor_pos(5, 1);
        t.print_string("ABCDE").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_top_and_bottom_margin(2, 5);
        t.set_left_and_right_margin(2, 4);

        t.clear_dirty();
        t.scroll_up(4).unwrap();

        // This is dirty because the cursor moves from this row
        assert!(t.is_dirty(act(0, 0)));
        for y in 1..5 { assert!(t.is_dirty(act(0, y))); }

        {
            let str = t.plain_string().unwrap();
            assert_eq!("top\n\n\n\nA   E", str);
        }
    }

    #[test]
    fn terminal_scrollup_creates_scrollback_in_primary_screen() {
        // When in primary screen with full-width scroll region at top,
        // scrollUp (CSI S) should push lines into scrollback like xterm.
        let mut t = Terminal::init(Options { cols: 5, rows: 5, max_scrollback_bytes: Some(10), ..Default::default() }).unwrap();

        // Fill the screen with content
        t.print_string("AAAAA").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("BBBBB").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("CCCCC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DDDDD").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("EEEEE").unwrap();

        t.clear_dirty();

        // Scroll up by 1, which should push "AAAAA" into scrollback
        t.scroll_up(1).unwrap();

        // The cursor row (new empty row) should be dirty
        assert!(rw(t.screens.active.cursor.page_row).dirty());

        // The active screen should now show BBBBB through EEEEE plus one blank line
        {
            let str = t.plain_string().unwrap();
            assert_eq!("BBBBB\nCCCCC\nDDDDD\nEEEEE", str);
        }

        // Now scroll to the top to see scrollback - AAAAA should be there
        t.screens.active.scroll(screen::Scroll::Top);
        {
            let str = t.plain_string().unwrap();
            // Should see AAAAA in scrollback
            assert_eq!("AAAAA\nBBBBB\nCCCCC\nDDDDD\nEEEEE", str);
        }
    }

    #[test]
    fn terminal_scrollup_with_max_scrollback_bytes_zero() {
        // When max_scrollback_bytes is 0, scrollUp should still work but not retain history
        let mut t = Terminal::init(Options { cols: 5, rows: 5, max_scrollback_bytes: Some(0), ..Default::default() }).unwrap();

        t.print_string("AAAAA").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("BBBBB").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("CCCCC").unwrap();

        t.scroll_up(1).unwrap();

        // Active screen should show scrolled content
        {
            let str = t.plain_string().unwrap();
            assert_eq!("BBBBB\nCCCCC", str);
        }

        // Scroll to top - should be same as active since no scrollback
        t.screens.active.scroll(screen::Scroll::Top);
        {
            let str = t.plain_string().unwrap();
            assert_eq!("BBBBB\nCCCCC", str);
        }
    }

    #[test]
    fn terminal_scrollup_with_max_scrollback_bytes_zero_and_top_margin() {
        // When max_scrollback_bytes is 0 and top margin is set, should use deleteLines path
        let mut t = Terminal::init(Options { cols: 5, rows: 5, max_scrollback_bytes: Some(0), ..Default::default() }).unwrap();

        t.print_string("AAAAA").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("BBBBB").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("CCCCC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DDDDD").unwrap();

        // Set top margin (not at row 0)
        t.set_top_and_bottom_margin(2, 5);

        t.scroll_up(1).unwrap();

        {
            let str = t.plain_string().unwrap();
            // First row preserved, rest scrolled
            assert_eq!("AAAAA\nCCCCC\nDDDDD", str);
        }
    }

    #[test]
    fn terminal_scrollup_with_max_scrollback_bytes_zero_and_left_right_margin() {
        // When max_scrollback_bytes is 0 with left/right margins, uses deleteLines path
        let mut t = Terminal::init(Options { cols: 10, rows: 5, max_scrollback_bytes: Some(0), ..Default::default() }).unwrap();

        t.print_string("AAAAABBBBB").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("CCCCCDDDDD").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("EEEEEFFFFF").unwrap();

        // Set left/right margins (columns 2-6, 1-indexed = indices 1-5)
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(2, 6);

        t.scroll_up(1).unwrap();

        {
            let str = t.plain_string().unwrap();
            // cols 1-5 scroll, col 0 and cols 6+ preserved
            assert_eq!("ACCCCDBBBB\nCEEEEFDDDD\nE     FFFF", str);
        }
    }

    #[test]
    fn terminal_scrolldown_simple() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_cursor_pos(2, 2);

        let cursor = t.screens.active.cursor.clone();
        t.clear_dirty();
        t.scroll_down(1);
        assert_eq!(cursor.x, t.screens.active.cursor.x);
        assert_eq!(cursor.y, t.screens.active.cursor.y);

        for y in 0..5 { assert!(t.is_dirty(act(0, y))); }

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nABC\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_scrolldown_hyperlink_moves() {
        let mut t = init(5, 5);

        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print_string("ABC").unwrap();
        t.screens.active.end_hyperlink();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_cursor_pos(2, 2);
        t.scroll_down(1);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nABC\nDEF\nGHI", str);
        }

        for x in 0..3 {
            let list_cell = t.screens.active.pages.get_cell(vp(x, 1)).unwrap();
            let row = rw(list_cell.row);
            assert!(row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
            assert_eq!(1, id);
            let page = nd(list_cell.node).page();
            assert_eq!(1, page.hyperlink_set.count());
        }
        for x in 0..3 {
            let list_cell = t.screens.active.pages.get_cell(vp(x, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(!row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(!cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell);
            assert!(id.is_none());
        }
    }

    #[test]
    fn terminal_scrolldown_outside_of_scroll_region() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI").unwrap();
        t.set_top_and_bottom_margin(3, 4);
        t.set_cursor_pos(2, 2);

        let cursor = t.screens.active.cursor.clone();
        t.clear_dirty();
        t.scroll_down(1);
        assert_eq!(cursor.x, t.screens.active.cursor.x);
        assert_eq!(cursor.y, t.screens.active.cursor.y);

        assert!(!t.is_dirty(act(0, 0)));

        // This is dirty because the cursor moves from this row
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));
        assert!(t.is_dirty(act(0, 3)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\nDEF\n\nGHI", str);
        }
    }

    #[test]
    fn terminal_scrolldown_left_right_scroll_region() {
        let mut t = init(10, 10);

        t.print_string("ABC123").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF456").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI789").unwrap();
        t.scrolling_region.left = 1;
        t.scrolling_region.right = 3;
        t.set_cursor_pos(2, 2);

        let cursor = t.screens.active.cursor.clone();
        t.clear_dirty();
        t.scroll_down(1);
        assert_eq!(cursor.x, t.screens.active.cursor.x);
        assert_eq!(cursor.y, t.screens.active.cursor.y);

        for y in 0..4 { assert!(t.is_dirty(act(0, y))); }

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A   23\nDBC156\nGEF489\n HI7", str);
        }
    }

    #[test]
    fn terminal_scrolldown_left_right_scroll_region_hyperlink() {
        let mut t = init(10, 10);

        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print_string("ABC123").unwrap();
        t.screens.active.end_hyperlink();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF456").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI789").unwrap();
        t.scrolling_region.left = 1;
        t.scrolling_region.right = 3;
        t.set_cursor_pos(2, 2);
        t.scroll_down(1);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A   23\nDBC156\nGEF489\n HI7", str);
        }

        // First row preserves hyperlink where we didn't scroll
        {
            for x in 0..1 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 0)).unwrap();
                let row = rw(list_cell.row);
                assert!(row.hyperlink());
                let cell = cl(list_cell.cell);
                assert!(cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
                assert_eq!(1, id);
                let page = nd(list_cell.node).page();
                assert_eq!(1, page.hyperlink_set.count());
            }
            for x in 1..4 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 0)).unwrap();
                let cell = cl(list_cell.cell);
                assert!(!cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell);
                assert!(id.is_none());
            }
            for x in 4..6 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 0)).unwrap();
                let row = rw(list_cell.row);
                assert!(row.hyperlink());
                let cell = cl(list_cell.cell);
                assert!(cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
                assert_eq!(1, id);
                let page = nd(list_cell.node).page();
                assert_eq!(1, page.hyperlink_set.count());
            }
        }

        // Second row gets some hyperlinks
        {
            for x in 0..1 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 1)).unwrap();
                let cell = cl(list_cell.cell);
                assert!(!cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell);
                assert!(id.is_none());
            }
            for x in 1..4 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 1)).unwrap();
                let row = rw(list_cell.row);
                assert!(row.hyperlink());
                let cell = cl(list_cell.cell);
                assert!(cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
                assert_eq!(1, id);
                let page = nd(list_cell.node).page();
                assert_eq!(1, page.hyperlink_set.count());
            }
            for x in 4..6 {
                let list_cell = t.screens.active.pages.get_cell(vp(x, 1)).unwrap();
                let cell = cl(list_cell.cell);
                assert!(!cell.hyperlink());
                let id = nd(list_cell.node).page().lookup_hyperlink(cell);
                assert!(id.is_none());
            }
        }
    }

    #[test]
    fn terminal_scrolldown_outside_of_left_right_scroll_region() {
        let mut t = init(10, 10);

        t.print_string("ABC123").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("DEF456").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("GHI789").unwrap();
        t.scrolling_region.left = 1;
        t.scrolling_region.right = 3;
        t.set_cursor_pos(1, 1);

        let cursor = t.screens.active.cursor.clone();
        t.clear_dirty();
        t.scroll_down(1);
        assert_eq!(cursor.x, t.screens.active.cursor.x);
        assert_eq!(cursor.y, t.screens.active.cursor.y);

        for y in 0..4 { assert!(t.is_dirty(act(0, y))); }

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A   23\nDBC156\nGEF489\n HI7", str);
        }
    }

    #[test]
    fn terminal_scrolldown_preserves_pending_wrap() {
        let mut t = init(5, 10);

        t.set_cursor_pos(1, 5);
        t.print('A' as u32).unwrap();
        t.set_cursor_pos(2, 5);
        t.print('B' as u32).unwrap();
        t.set_cursor_pos(3, 5);
        t.print('C' as u32).unwrap();
        t.scroll_down(1);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n    A\n    B\nX   C", str);
        }
    }

    #[test]
    fn terminal_erasechars_simple_operation() {
        let mut t = init(5, 5);

        for c in "ABC".chars() { t.print(c as u32).unwrap(); }
        t.set_cursor_pos(1, 1);
        t.clear_dirty();
        t.erase_chars(2);
        t.print('X' as u32).unwrap();

        assert!(t.is_dirty(act(0, 0)));
        assert!(!t.is_dirty(act(0, 1)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X C", str);
        }
    }

    #[test]
    fn terminal_erasechars_minimum_one() {
        let mut t = init(5, 5);

        for c in "ABC".chars() { t.print(c as u32).unwrap(); }
        t.set_cursor_pos(1, 1);
        t.clear_dirty();
        t.erase_chars(0);
        t.print('X' as u32).unwrap();
        assert!(t.is_dirty(act(0, 0)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("XBC", str);
        }
    }

    #[test]
    fn terminal_erasechars_beyond_screen_edge() {
        let mut t = init(5, 5);

        for c in "  ABC".chars() { t.print(c as u32).unwrap(); }
        t.set_cursor_pos(1, 4);
        t.erase_chars(10);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("  A", str);
        }
    }

    #[test]
    fn terminal_erasechars_wide_character() {
        let mut t = init(5, 5);

        t.print('橋' as u32).unwrap();
        for c in "BC".chars() { t.print(c as u32).unwrap(); }
        t.set_cursor_pos(1, 1);
        t.erase_chars(1);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X BC", str);
        }
    }

    #[test]
    fn terminal_erasechars_resets_pending_wrap() {
        let mut t = init(5, 5);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.erase_chars(1);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDX", str);
        }
    }

    #[test]
    fn terminal_erasechars_resets_wrap() {
        let mut t = init(5, 5);

        for c in "ABCDE123".chars() { t.print(c as u32).unwrap(); }
        {
            let list_cell = t.screens.active.pages.get_cell(act(0, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(row.wrap());
        }

        t.set_cursor_pos(1, 1);
        t.erase_chars(1);

        {
            let list_cell = t.screens.active.pages.get_cell(act(0, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(!row.wrap());
        }

        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("XBCDE\n123", str);
        }
    }

    #[test]
    fn terminal_erasechars_preserves_background_sgr() {
        let mut t = init(10, 10);

        for c in "ABC".chars() { t.print(c as u32).unwrap(); }
        t.set_cursor_pos(1, 1);
        t.set_attribute(sgr::Attribute::DirectColorBg(color::Rgb {
            r: 0xFF,
            g: 0,
            b: 0,
        })).unwrap();
        t.erase_chars(2);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("  C", str);
            {
                let list_cell = t.screens.active.pages.get_cell(act(0, 0)).unwrap();
                assert!(cl(list_cell.cell).content_tag() == pagepkg::ContentTag::BgColorRgb);
                assert_eq!(color::Rgb {
                    r: 0xFF,
                    g: 0,
                    b: 0,
                }, cl(list_cell.cell).content_color_rgb());
            }
            {
                let list_cell = t.screens.active.pages.get_cell(act(1, 0)).unwrap();
                assert!(cl(list_cell.cell).content_tag() == pagepkg::ContentTag::BgColorRgb);
                assert_eq!(color::Rgb {
                    r: 0xFF,
                    g: 0,
                    b: 0,
                }, cl(list_cell.cell).content_color_rgb());
            }
        }
    }

    #[test]
    fn terminal_erasechars_handles_refcounted_styles() {
        let mut t = init(10, 10);

        t.set_attribute(sgr::Attribute::Bold).unwrap();
        t.print('A' as u32).unwrap();
        t.print('B' as u32).unwrap();
        t.set_attribute(sgr::Attribute::Unset).unwrap();
        t.print('C' as u32).unwrap();

        // verify we have styles in our style map
        let page = cursor_page(t.screens.active);
        assert_eq!(1, page.styles.count());

        t.set_cursor_pos(1, 1);
        t.erase_chars(2);

        // verify we have no styles in our style map
        assert_eq!(0, page.styles.count());
    }

    #[test]
    fn terminal_erasechars_protected_attributes_respected_with_iso() {
        let mut t = init(5, 5);

        t.set_protected_mode(ansi::ProtectedMode::Iso);
        for c in "ABC".chars() { t.print(c as u32).unwrap(); }
        t.set_cursor_pos(1, 1);
        t.erase_chars(2);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC", str);
        }
    }

    #[test]
    fn terminal_erasechars_protected_attributes_ignored_with_dec_most_recent() {
        let mut t = init(5, 5);

        t.set_protected_mode(ansi::ProtectedMode::Iso);
        for c in "ABC".chars() { t.print(c as u32).unwrap(); }
        t.set_protected_mode(ansi::ProtectedMode::Dec);
        t.set_protected_mode(ansi::ProtectedMode::Off);
        t.set_cursor_pos(1, 1);
        t.erase_chars(2);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("  C", str);
        }
    }

    #[test]
    fn terminal_erasechars_protected_attributes_ignored_with_dec_set() {
        let mut t = init(5, 5);

        t.set_protected_mode(ansi::ProtectedMode::Dec);
        for c in "ABC".chars() { t.print(c as u32).unwrap(); }
        t.set_cursor_pos(1, 1);
        t.erase_chars(2);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("  C", str);
        }
    }

    #[test]
    fn terminal_erasechars_wide_char_boundary_conditions() {
        let mut t = init(8, 1);

        t.print_string("😀a😀b😀").unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!("😀a😀b😀", str);
        }

        t.set_cursor_pos(1, 2);
        t.erase_chars(3);
        cursor_page(t.screens.active).assert_integrity();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("     b😀", str);
        }
    }

    #[test]
    fn terminal_erasechars_wide_char_splits_proper_cell_boundaries() {
        let mut t = init(30, 1);

        // This is a test for a bug: https://github.com/ghostty-org/ghostty/issues/2817
        // To explain the setup:
        // (1) We need our wide characters starting on an even (1-based) column.
        // (2) We need our cursor to be in the middle somewhere.
        // (3) We need our count to be less than our cursor X and on a split cell.
        // The bug was that we split the wrong cell boundaries.

        t.print_string("x食べて下さい").unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!("x食べて下さい", str);
        }

        t.set_cursor_pos(1, 6); // At: て
        t.erase_chars(4); // Delete: て下
        cursor_page(t.screens.active).assert_integrity();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("x食べ    さい", str);
        }
    }

    #[test]
    fn terminal_erasechars_wide_char_wrap_boundary_conditions() {
        let mut t = init(8, 3);

        t.print_string(".......😀abcde😀......").unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!(".......\n😀abcde\n😀......", str);

            let unwrapped = t.plain_string_unwrapped().unwrap();
            assert_eq!(".......😀abcde😀......", unwrapped);
        }

        t.set_cursor_pos(2, 2);
        t.erase_chars(3);
        cursor_page(t.screens.active).assert_integrity();

        {
            let str = t.plain_string().unwrap();
            assert_eq!(".......\n    cde\n😀......", str);

            let unwrapped = t.plain_string_unwrapped().unwrap();
            assert_eq!(".......     cde\n😀......", unwrapped);
        }
    }

    #[test]
    fn terminal_erasechars_clearing_wrapped_wide_char_marks_spacer_head_row_dirty() {
        let mut t = init(5, 3);

        // The wide char doesn't fit so it wraps, leaving a spacer head at
        // the end of the first row.
        t.print_string("ABCD字").unwrap();
        {
            let list_cell = t.screens.active.pages.get_cell(scr(4, 0)).unwrap();
            assert_eq!(Wide::SpacerHead, cl(list_cell.cell).wide());
            assert!(rw(list_cell.row).wrap());
        }

        t.set_cursor_pos(2, 1);
        t.clear_dirty();
        t.erase_chars(1);
        cursor_page(t.screens.active).assert_integrity();

        // Erasing the wide char also clears the spacer head on the previous
        // row, so that row must be dirty too.
        assert!(t.is_dirty(scr(0, 0)));
        assert!(t.is_dirty(scr(0, 1)));
        assert!(!t.is_dirty(scr(0, 2)));

        {
            let list_cell = t.screens.active.pages.get_cell(scr(4, 0)).unwrap();
            assert_eq!(Wide::Narrow, cl(list_cell.cell).wide());
        }
    }

    #[test]
    fn terminal_reverseindex() {
        let mut t = init(2, 5);

        // Initial value
        t.print('A' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('B' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('C' as u32).unwrap();
        t.reverse_index();
        t.print('D' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.carriage_return();
        t.linefeed().unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\nBD\nC", str);
        }
    }

    #[test]
    fn terminal_reverseindex_from_the_top() {
        let mut t = init(2, 5);

        t.print('A' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('B' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.carriage_return();
        t.linefeed().unwrap();

        t.set_cursor_pos(1, 1);
        t.reverse_index();
        t.print('D' as u32).unwrap();

        t.carriage_return();
        t.linefeed().unwrap();
        t.set_cursor_pos(1, 1);
        t.reverse_index();
        t.print('E' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("E\nD\nA\nB", str);
        }
    }

    #[test]
    fn terminal_reverseindex_top_of_scrolling_region() {
        let mut t = init(2, 10);

        // Initial value
        t.set_cursor_pos(2, 1);
        t.print('A' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('B' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('C' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('D' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();

        // Set our scroll region
        t.set_top_and_bottom_margin(2, 5);
        t.set_cursor_pos(2, 1);
        t.reverse_index();
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nX\nA\nB\nC", str);
        }
    }

    #[test]
    fn terminal_reverseindex_top_of_screen() {
        let mut t = init(5, 5);

        t.print('A' as u32).unwrap();
        t.set_cursor_pos(2, 1);
        t.print('B' as u32).unwrap();
        t.set_cursor_pos(3, 1);
        t.print('C' as u32).unwrap();
        t.set_cursor_pos(1, 1);
        t.reverse_index();
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X\nA\nB\nC", str);
        }
    }

    #[test]
    fn terminal_reverseindex_not_top_of_screen() {
        let mut t = init(5, 5);

        t.print('A' as u32).unwrap();
        t.set_cursor_pos(2, 1);
        t.print('B' as u32).unwrap();
        t.set_cursor_pos(3, 1);
        t.print('C' as u32).unwrap();
        t.set_cursor_pos(2, 1);
        t.reverse_index();
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X\nB\nC", str);
        }
    }

    #[test]
    fn terminal_reverseindex_top_bottom_margins() {
        let mut t = init(5, 5);

        t.print('A' as u32).unwrap();
        t.set_cursor_pos(2, 1);
        t.print('B' as u32).unwrap();
        t.set_cursor_pos(3, 1);
        t.print('C' as u32).unwrap();
        t.set_top_and_bottom_margin(2, 3);
        t.set_cursor_pos(2, 1);
        t.reverse_index();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\n\nB", str);
        }
    }

    #[test]
    fn terminal_reverseindex_outside_top_bottom_margins() {
        let mut t = init(5, 5);

        t.print('A' as u32).unwrap();
        t.set_cursor_pos(2, 1);
        t.print('B' as u32).unwrap();
        t.set_cursor_pos(3, 1);
        t.print('C' as u32).unwrap();
        t.set_top_and_bottom_margin(2, 3);
        t.set_cursor_pos(1, 1);
        t.reverse_index();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\nB\nC", str);
        }
    }

    #[test]
    fn terminal_reverseindex_left_right_margins() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.set_cursor_pos(2, 1);
        t.print_string("DEF").unwrap();
        t.set_cursor_pos(3, 1);
        t.print_string("GHI").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(2, 3);
        t.set_cursor_pos(1, 2);
        t.reverse_index();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\nDBC\nGEF\n HI", str);
        }
    }

    #[test]
    fn terminal_reverseindex_outside_left_right_margins() {
        let mut t = init(5, 5);

        t.print_string("ABC").unwrap();
        t.set_cursor_pos(2, 1);
        t.print_string("DEF").unwrap();
        t.set_cursor_pos(3, 1);
        t.print_string("GHI").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(2, 3);
        t.set_cursor_pos(1, 1);
        t.reverse_index();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABC\nDEF\nGHI", str);
        }
    }

    #[test]
    fn terminal_index() {
        let mut t = init(2, 5);

        t.index().unwrap();
        t.print('A' as u32).unwrap();

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nA", str);
        }
    }

    #[test]
    fn terminal_index_from_the_bottom() {
        let mut t = init(2, 5);

        t.set_cursor_pos(5, 1);
        t.print('A' as u32).unwrap();
        t.cursor_left(1); // undo moving right from 'A'

        t.clear_dirty();
        t.index().unwrap();
        t.print('B' as u32).unwrap();

        assert!(t.is_dirty(act(0, 3)));
        assert!(t.is_dirty(act(0, 4)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\n\nA\nB", str);
        }
    }

    #[test]
    fn terminal_index_scrolling_with_hyperlink() {
        let mut t = init(2, 5);

        t.set_cursor_pos(5, 1);
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print('A' as u32).unwrap();
        t.screens.active.end_hyperlink();
        t.cursor_left(1); // undo moving right from 'A'
        t.index().unwrap();
        t.print('B' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\n\nA\nB", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(vp(0, 3)).unwrap();
            let row = rw(list_cell.row);
            assert!(row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
            assert_eq!(1, id);
        }
        {
            let list_cell = t.screens.active.pages.get_cell(vp(0, 4)).unwrap();
            let row = rw(list_cell.row);
            assert!(!row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(!cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell);
            assert!(id.is_none());
        }
    }

    #[test]
    fn terminal_index_outside_of_scrolling_region() {
        let mut t = init(2, 5);

        assert_eq!(0, t.screens.active.cursor.y);
        t.set_top_and_bottom_margin(2, 5);
        t.index().unwrap();
        assert_eq!(1, t.screens.active.cursor.y);
    }

    #[test]
    fn terminal_index_from_the_bottom_outside_of_scroll_region() {
        let mut t = init(2, 5);

        t.set_top_and_bottom_margin(1, 2);
        t.set_cursor_pos(5, 1);
        t.print('A' as u32).unwrap();
        t.clear_dirty();
        t.index().unwrap();
        t.print('B' as u32).unwrap();
        assert!(t.is_dirty(act(0, 4)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\n\n\nAB", str);
        }
    }

    #[test]
    fn terminal_index_no_scroll_region_top_of_screen() {
        let mut t = init(5, 5);

        t.print('A' as u32).unwrap();
        t.clear_dirty();
        t.index().unwrap();
        t.print('X' as u32).unwrap();

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\n X", str);
        }
    }

    #[test]
    fn terminal_index_bottom_of_primary_screen() {
        let mut t = init(5, 5);

        t.set_cursor_pos(5, 1);
        t.print('A' as u32).unwrap();
        t.clear_dirty();
        t.index().unwrap();
        t.print('X' as u32).unwrap();

        assert!(t.is_dirty(act(0, 3)));
        assert!(t.is_dirty(act(0, 4)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\n\nA\n X", str);
        }
    }

    #[test]
    fn terminal_index_bottom_of_primary_screen_background_sgr() {
        let mut t = init(5, 5);

        t.set_cursor_pos(5, 1);
        t.print('A' as u32).unwrap();
        t.set_attribute(sgr::Attribute::DirectColorBg(color::Rgb {
            r: 0xFF,
            g: 0,
            b: 0,
        })).unwrap();
        t.index().unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\n\nA", str);
            for x in 0..5 {
                let list_cell = t.screens.active.pages.get_cell(act(x, 4)).unwrap();
                assert!(cl(list_cell.cell).content_tag() == pagepkg::ContentTag::BgColorRgb);
                assert_eq!(color::Rgb {
                    r: 0xFF,
                    g: 0,
                    b: 0,
                }, cl(list_cell.cell).content_color_rgb());
            }
        }
    }

    #[test]
    fn terminal_index_inside_scroll_region() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(1, 3);
        t.print('A' as u32).unwrap();
        t.clear_dirty();
        t.index().unwrap();
        t.print('X' as u32).unwrap();

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\n X", str);
        }
    }

    #[test]
    fn terminal_index_bottom_of_scroll_region_with_hyperlinks() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(1, 2);
        t.print('A' as u32).unwrap();
        t.index().unwrap();
        t.carriage_return();
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print('B' as u32).unwrap();
        t.screens.active.end_hyperlink();
        t.index().unwrap();
        t.carriage_return();
        t.print('C' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("B\nC", str);
        }

        {
            let list_cell = t.screens.active.pages.get_cell(vp(0, 0)).unwrap();
            let row = rw(list_cell.row);
            assert!(row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell).unwrap();
            assert_eq!(1, id);
        }
        {
            let list_cell = t.screens.active.pages.get_cell(vp(0, 1)).unwrap();
            let row = rw(list_cell.row);
            assert!(!row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(!cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell);
            assert!(id.is_none());
        }
    }

    #[test]
    fn terminal_index_bottom_of_scroll_region_clear_hyperlinks() {
        let mut t = Terminal::init(Options { cols: 5, rows: 5, max_scrollback_bytes: Some(0), ..Default::default() }).unwrap();

        t.set_top_and_bottom_margin(2, 3);
        t.set_cursor_pos(2, 1);
        t.screens.active.start_hyperlink(b"http://example.com", None).unwrap();
        t.print('A' as u32).unwrap();
        t.screens.active.end_hyperlink();
        t.index().unwrap();
        t.carriage_return();
        t.print('B' as u32).unwrap();
        t.index().unwrap();
        t.carriage_return();
        t.print('C' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nB\nC", str);
        }

        for y in 1..3 {
            let list_cell = t.screens.active.pages.get_cell(vp(0, y)).unwrap();
            let row = rw(list_cell.row);
            assert!(!row.hyperlink());
            let cell = cl(list_cell.cell);
            assert!(!cell.hyperlink());
            let id = nd(list_cell.node).page().lookup_hyperlink(cell);
            assert!(id.is_none());
            let page = nd(list_cell.node).page();
            assert_eq!(0, page.hyperlink_set.count());
        }
    }

    #[test]
    fn terminal_index_bottom_of_scroll_region_with_background_sgr() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(1, 3);
        t.set_cursor_pos(4, 1);
        t.print('B' as u32).unwrap();
        t.set_cursor_pos(3, 1);
        t.print('A' as u32).unwrap();
        t.set_attribute(sgr::Attribute::DirectColorBg(color::Rgb {
            r: 0xFF,
            g: 0,
            b: 0,
        })).unwrap();
        t.index().unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nA\n\nB", str);
        }

        for x in 0..t.cols {
            let list_cell = t.screens.active.pages.get_cell(act(x, 2)).unwrap();
            assert!(cl(list_cell.cell).content_tag() == pagepkg::ContentTag::BgColorRgb);
            assert_eq!(color::Rgb {
                r: 0xFF,
                g: 0,
                b: 0,
            }, cl(list_cell.cell).content_color_rgb());
        }
    }

    #[test]
    fn terminal_index_bottom_of_primary_screen_with_scroll_region() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(1, 3);
        t.set_cursor_pos(3, 1);
        t.print('A' as u32).unwrap();
        t.set_cursor_pos(5, 1);
        t.clear_dirty();
        t.index().unwrap();
        t.index().unwrap();
        t.index().unwrap();
        t.print('X' as u32).unwrap();

        for y in 0..4 { assert!(!t.is_dirty(act(0, y))); }
        assert!(t.is_dirty(act(0, 4)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\nA\n\nX", str);
        }
    }

    #[test]
    fn terminal_index_outside_left_right_margin() {
        let mut t = init(10, 5);

        t.set_top_and_bottom_margin(1, 3);
        t.scrolling_region.left = 3;
        t.scrolling_region.right = 5;
        t.set_cursor_pos(3, 3);
        t.print('A' as u32).unwrap();
        t.set_cursor_pos(3, 1);
        t.clear_dirty();
        t.index().unwrap();
        t.print('X' as u32).unwrap();

        assert!(t.is_dirty(act(0, 2)));

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\nX A", str);
        }
    }

    #[test]
    fn terminal_index_inside_left_right_margin() {
        let mut t = init(10, 5);

        t.print_string("AAAAAA").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("AAAAAA").unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print_string("AAAAAA").unwrap();
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_top_and_bottom_margin(1, 3);
        t.set_left_and_right_margin(1, 3);
        t.set_cursor_pos(3, 1);

        t.clear_dirty();
        t.index().unwrap();

        assert!(t.is_dirty(act(0, 0)));
        assert!(t.is_dirty(act(0, 1)));
        assert!(t.is_dirty(act(0, 2)));

        assert_eq!(2, t.screens.active.cursor.y);
        assert_eq!(0, t.screens.active.cursor.x);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("AAAAAA\nAAAAAA\n   AAA", str);
        }
    }

    #[test]
    fn terminal_index_bottom_of_scroll_region_creates_scrollback() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(1, 3);
        t.print_string("1\n2\n3").unwrap();
        t.set_cursor_pos(4, 1);
        t.print('X' as u32).unwrap();
        t.set_cursor_pos(3, 1);
        t.index().unwrap();
        t.print('Y' as u32).unwrap();

        {
            let str = t.screens.active.dump_string_alloc(vp(0, 0)).unwrap();
            assert_eq!("2\n3\nY\nX", str);
        }
        {
            let str = t.screens.active.dump_string_alloc(scr(0, 0)).unwrap();
            assert_eq!("1\n2\n3\nY\nX", str);
        }
    }

    #[test]
    fn terminal_index_bottom_of_scroll_region_no_scrollback() {
        let mut t = Terminal::init(Options { cols: 5, rows: 5, max_scrollback_bytes: Some(0), ..Default::default() }).unwrap();

        t.set_top_and_bottom_margin(1, 3);
        t.set_cursor_pos(4, 1);
        t.print('B' as u32).unwrap();
        t.set_cursor_pos(3, 1);
        t.print('A' as u32).unwrap();
        t.clear_dirty();
        t.index().unwrap();
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\nA\n X\nB", str);
        }
    }

    #[test]
    fn terminal_index_bottom_of_scroll_region_blank_line_preserves_sgr() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(1, 3);
        t.print_string("1\n2\n3").unwrap();
        t.set_cursor_pos(4, 1);
        t.print('X' as u32).unwrap();
        t.set_cursor_pos(3, 1);
        t.set_attribute(sgr::Attribute::DirectColorBg(color::Rgb {
            r: 0xFF,
            g: 0,
            b: 0,
        })).unwrap();
        t.index().unwrap();

        {
            let str = t.screens.active.dump_string_alloc(vp(0, 0)).unwrap();
            assert_eq!("2\n3\n\nX", str);
        }
        {
            let str = t.screens.active.dump_string_alloc(scr(0, 0)).unwrap();
            assert_eq!("1\n2\n3\n\nX", str);
        }
        for x in 0..t.cols {
            let list_cell = t.screens.active.pages.get_cell(act(x, 2)).unwrap();
            assert!(cl(list_cell.cell).content_tag() == pagepkg::ContentTag::BgColorRgb);
            assert_eq!(color::Rgb {
                r: 0xFF,
                g: 0,
                b: 0,
            }, cl(list_cell.cell).content_color_rgb());
        }
    }

    #[test]
    fn terminal_index_bottom_of_scroll_region_with_top_margin_and_background_sgr() {
        let mut t = init(5, 5);

        t.print_string("1\n2\n3\n4\n5").unwrap();
        t.set_top_and_bottom_margin(2, 4);
        t.set_cursor_pos(4, 1);
        t.set_attribute(sgr::Attribute::DirectColorBg(color::Rgb {
            r: 0xFF,
            g: 0,
            b: 0,
        })).unwrap();
        t.index().unwrap();

        // The region (rows 2-4) scrolled up, rows outside are unchanged.
        {
            let str = t.plain_string().unwrap();
            assert_eq!("1\n3\n4\n\n5", str);
        }

        // The cursor is on the new blank row.
        assert_eq!(3, t.screens.active.cursor.y);

        // The new blank row must be filled with our background color.
        for x in 0..t.cols {
            let list_cell = t.screens.active.pages.get_cell(act(x, 3)).unwrap();
            assert!(cl(list_cell.cell).content_tag() == pagepkg::ContentTag::BgColorRgb);
            assert_eq!(color::Rgb {
                r: 0xFF,
                g: 0,
                b: 0,
            }, cl(list_cell.cell).content_color_rgb());
        }
    }

    #[test]
    fn terminal_index_bottom_of_alt_screen_full_region() {
        let mut t = init(5, 3);

        t.switch_screen_mode(SwitchScreenMode::Mode1049, true).unwrap();
        t.print_string("A\nB\nC").unwrap();
        t.index().unwrap();
        t.carriage_return();
        t.print('D' as u32).unwrap();

        // Content scrolled up and the scrolled-out row is discarded, NOT
        // moved into scrollback (the alt screen has none).
        {
            let str = t.plain_string().unwrap();
            assert_eq!("B\nC\nD", str);
        }
        {
            let str = t.screens.active.dump_string_alloc(scr(0, 0)).unwrap();
            assert_eq!("B\nC\nD", str);
        }

        // Primary screen is untouched.
        t.switch_screen_mode(SwitchScreenMode::Mode1049, false).unwrap();
        {
            let str = t.plain_string().unwrap();
            assert_eq!("", str);
        }
    }

    #[test]
    fn terminal_index_bottom_of_alt_screen_top_region() {
        let mut t = init(5, 5);

        t.switch_screen_mode(SwitchScreenMode::Mode1049, true).unwrap();
        t.print_string("1\n2\n3\n4\n5").unwrap();

        // Region at the top of the screen, excluding the last row. On the
        // alt screen this must NOT create scrollback.
        t.set_top_and_bottom_margin(1, 4);
        t.set_cursor_pos(4, 1);
        t.index().unwrap();
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("2\n3\n4\nX\n5", str);
        }
        {
            let str = t.screens.active.dump_string_alloc(scr(0, 0)).unwrap();
            assert_eq!("2\n3\n4\nX\n5", str);
        }
    }

    #[test]
    fn terminal_scrollup_top_region_no_scrollback() {
        let mut t = Terminal::init(Options { cols: 5, rows: 5, max_scrollback_bytes: Some(0), ..Default::default() }).unwrap();

        t.print_string("A\nB\nC\nD\nE").unwrap();
        t.set_top_and_bottom_margin(1, 3);
        t.scroll_up(1).unwrap();

        // The region scrolled and the scrolled-out row is discarded.
        {
            let str = t.plain_string().unwrap();
            assert_eq!("B\nC\n\nD\nE", str);
        }
        {
            let str = t.screens.active.dump_string_alloc(scr(0, 0)).unwrap();
            assert_eq!("B\nC\n\nD\nE", str);
        }
    }

    #[test]
    fn terminal_cursorup_basic() {
        let mut t = init(5, 5);

        t.set_cursor_pos(3, 1);
        t.print('A' as u32).unwrap();
        t.cursor_up(10);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!(" X\n\nA", str);
        }
    }

    #[test]
    fn terminal_cursorup_below_top_scroll_margin() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(2, 4);
        t.set_cursor_pos(3, 1);
        t.print('A' as u32).unwrap();
        t.cursor_up(5);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n X\nA", str);
        }
    }

    #[test]
    fn terminal_cursorup_above_top_scroll_margin() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(3, 5);
        t.set_cursor_pos(3, 1);
        t.print('A' as u32).unwrap();
        t.set_cursor_pos(2, 1);
        t.cursor_up(10);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X\n\nA", str);
        }
    }

    #[test]
    fn terminal_cursorup_resets_wrap() {
        let mut t = init(5, 5);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.cursor_up(1);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDX", str);
        }
    }

    #[test]
    fn terminal_cursorleft_no_wrap() {
        let mut t = init(10, 5);

        t.print('A' as u32).unwrap();
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('B' as u32).unwrap();
        t.cursor_left(10);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\nB", str);
        }
    }

    #[test]
    fn terminal_cursorleft_stops_at_left_margin() {
        let mut t = init(5, 5);

        t.scrolling_region.left = 2;
        t.set_cursor_pos(1, 4);
        t.cursor_left(100);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("  X", str);
        }
    }

    #[test]
    fn terminal_cursorleft_left_of_left_margin() {
        let mut t = init(5, 5);

        t.scrolling_region.left = 3;
        t.set_cursor_pos(1, 3);
        t.cursor_left(100);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("X", str);
        }
    }

    #[test]
    fn terminal_backspace_at_left_margin() {
        let mut t = init(5, 5);

        t.scrolling_region.left = 2;
        t.set_cursor_pos(1, 3);
        t.backspace();
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("  X", str);
        }
    }

    #[test]
    fn terminal_cursorleft_unsets_pending_wrap_state() {
        let mut t = init(5, 5);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.cursor_left(1);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCXE", str);
        }
    }

    #[test]
    fn terminal_cursorleft_unsets_pending_wrap_state_with_longer_jump() {
        let mut t = init(5, 5);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.cursor_left(3);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("AXCDE", str);
        }
    }

    #[test]
    fn terminal_cursorleft_reverse_wrap_with_pending_wrap_state() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrap, true);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.cursor_left(1);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDX", str);
        }
    }

    #[test]
    fn terminal_cursorleft_reverse_wrap_with_pending_wrap_above_top_margin() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrap, true);
        t.modes.set(Mode::EnableLeftAndRightMargin, true);
        t.set_left_and_right_margin(1, 2);
        for c in "AB".chars() { t.print(c as u32).unwrap(); }
        t.save_cursor();

        // Restore pending wrap at the left margin, above the top margin.
        t.set_left_and_right_margin(2, 5);
        t.set_top_and_bottom_margin(3, 5);
        t.restore_cursor();
        assert!(t.screens.active.cursor.pending_wrap);

        t.cursor_left(1);
        assert!(!t.screens.active.cursor.pending_wrap);
        assert_eq!(1, t.screens.active.cursor.x);
        assert_eq!(0, t.screens.active.cursor.y);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("AX", str);
        }
    }

    #[test]
    fn terminal_cursorleft_reverse_wrap_extended_with_pending_wrap_state() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrapExtended, true);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.cursor_left(1);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDX", str);
        }
    }

    #[test]
    fn terminal_cursorleft_reverse_wrap() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrap, true);

        for c in "ABCDE1".chars() { t.print(c as u32).unwrap(); }
        t.cursor_left(2);
        t.print('X' as u32).unwrap();
        assert!(t.screens.active.cursor.pending_wrap);

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDX\n1", str);
        }
    }

    #[test]
    fn terminal_cursorleft_reverse_wrap_with_no_soft_wrap() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrap, true);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('1' as u32).unwrap();
        t.cursor_left(2);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDE\nX", str);
        }
    }

    #[test]
    fn terminal_cursorleft_reverse_wrap_before_left_margin() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrap, true);
        t.set_top_and_bottom_margin(3, 0);
        t.cursor_left(1);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("\n\nX", str);
        }
    }

    #[test]
    fn terminal_cursorleft_extended_reverse_wrap() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrapExtended, true);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('1' as u32).unwrap();
        t.cursor_left(2);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDX\n1", str);
        }
    }

    #[test]
    fn terminal_cursorleft_extended_reverse_wrap_bottom_wraparound() {
        let mut t = init(5, 3);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrapExtended, true);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('1' as u32).unwrap();
        t.cursor_left(1 + t.cols as usize + 1);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDE\n1\n    X", str);
        }
    }

    #[test]
    fn terminal_cursorleft_extended_reverse_wrap_is_priority_if_both_set() {
        let mut t = init(5, 3);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrap, true);
        t.modes.set(Mode::ReverseWrapExtended, true);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        t.carriage_return();
        t.linefeed().unwrap();
        t.print('1' as u32).unwrap();
        t.cursor_left(1 + t.cols as usize + 1);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDE\n1\n    X", str);
        }
    }

    #[test]
    fn terminal_cursorleft_extended_reverse_wrap_above_top_scroll_region() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrapExtended, true);

        t.set_top_and_bottom_margin(3, 0);
        t.set_cursor_pos(2, 1);
        t.cursor_left(1000);

        assert_eq!(0, t.screens.active.cursor.x);
        assert_eq!(0, t.screens.active.cursor.y);
    }

    #[test]
    fn terminal_cursorleft_reverse_wrap_on_first_row() {
        let mut t = init(5, 5);

        t.modes.set(Mode::Wraparound, true);
        t.modes.set(Mode::ReverseWrap, true);

        t.set_top_and_bottom_margin(3, 0);
        t.set_cursor_pos(1, 2);
        t.cursor_left(1000);

        assert_eq!(0, t.screens.active.cursor.x);
        assert_eq!(0, t.screens.active.cursor.y);
    }

    #[test]
    fn terminal_cursordown_basic() {
        let mut t = init(5, 5);

        t.print('A' as u32).unwrap();
        t.cursor_down(10);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\n\n\n\n X", str);
        }
    }

    #[test]
    fn terminal_cursordown_above_bottom_scroll_margin() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(1, 3);
        t.print('A' as u32).unwrap();
        t.cursor_down(10);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\n\n X", str);
        }
    }

    #[test]
    fn terminal_cursordown_below_bottom_scroll_margin() {
        let mut t = init(5, 5);

        t.set_top_and_bottom_margin(1, 3);
        t.print('A' as u32).unwrap();
        t.set_cursor_pos(4, 1);
        t.cursor_down(10);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("A\n\n\n\nX", str);
        }
    }

    #[test]
    fn terminal_cursordown_resets_wrap() {
        let mut t = init(5, 5);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.cursor_down(1);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDE\n    X", str);
        }
    }

    #[test]
    fn terminal_cursorright_resets_wrap() {
        let mut t = init(5, 5);

        for c in "ABCDE".chars() { t.print(c as u32).unwrap(); }
        assert!(t.screens.active.cursor.pending_wrap);
        t.cursor_right(1);
        assert!(!t.screens.active.cursor.pending_wrap);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("ABCDX", str);
        }
    }

    #[test]
    fn terminal_cursorright_to_the_edge_of_screen() {
        let mut t = init(5, 5);

        t.cursor_right(100);
        t.print('X' as u32).unwrap();

        {
            let str = t.plain_string().unwrap();
            assert_eq!("    X", str);
        }
    }

}
