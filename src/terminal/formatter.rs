//! Port of `src/terminal/formatter.zig`.
//!
//! STATUS: partial — `Format`, `formatStyled`, `CodepointMap`, `Options`,
//! `PinMap`, `ScreenFormatter`, `PageListFormatter` and `PageFormatter`
//! (all emit formats) are ported. `TerminalFormatter` (needs the
//! `Terminal` port) and all of the formatter tests are pending.
//!
//! Notes on the port:
//! - Writers are `std::io::Write` trait objects; Zig's
//!   `std.Io.Writer.Error` is `std::io::Error`.
//! - The comptime `emit` specialization is a runtime match.
//! - Nested Zig types are flattened: `PinMap.Map` is [`PinMapMap`],
//!   `PinMap.NodeRun` is [`PinMapNodeRun`], `ScreenFormatter.Content` is
//!   [`ScreenFormatterContent`], `ScreenFormatter.Extra` is
//!   [`ScreenFormatterExtra`], `ScreenFormatter.CursorOrigin` is
//!   [`CursorOrigin`], `PageFormatter.PointMap` is [`PointMap`] and
//!   `PageFormatter.TrailingState` is [`TrailingState`].
//! - `CodepointMap` lists are a `Vec` (Zig: `std.MultiArrayList`).

use std::io::{self, Write};

use crate::terminal::charsets;
use crate::terminal::color;
use crate::terminal::hyperlink;
use crate::terminal::kitty;
use crate::terminal::page::{Cell, ContentTag, Page, Row, Wide};
use crate::terminal::page_list::{Direction, NodePtr, PageList, Pin};
use crate::terminal::point::{Coordinate, Tag};
use crate::terminal::screen::Screen;
use crate::terminal::selection::Selection;
use crate::terminal::size::{self, CellCountInt};
use crate::terminal::style::{self, Style};

/// Formats available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum Format {
    /// Plain text.
    Plain,

    /// Include VT sequences to preserve colors, styles, URLs, etc.
    /// This is predominantly SGR sequences but may contain others as needed.
    ///
    /// Note that for reference colors, like palette indices, this will
    /// vary based on the formatter and you should see the docs. For example,
    /// PageFormatter with VT will emit SGR sequences with palette indices,
    /// not the color itself.
    ///
    /// For VT, newlines will be emitted as `\r\n` so that the cursor properly
    /// moves back to the beginning prior emitting follow-up lines.
    Vt,

    /// HTML output.
    ///
    /// This will emit inline styles for as much styling as possible,
    /// in the interest of simplicity and ease of editing. This isn't meant
    /// to build the most beautiful or efficient HTML, but rather to be
    /// stylistically correct.
    ///
    /// For colors, RGB values are emitted as inline CSS (#RRGGBB) while palette
    /// indices use CSS variables (var(--vt-palette-N)). The palette colors are
    /// emitted by TerminalFormatter.Extra.palette as a <style> block if you
    /// want to also include that. But if you only format a screen or lower,
    /// the formatter doesn't have access to the current palette to render it.
    ///
    /// Newlines are emitted as actual '\n' characters. Consumers should use
    /// CSS white-space: pre or pre-wrap to preserve spacing and alignment.
    Html,
}

/// Returns true if the format emits styled output (not plaintext).
pub fn format_styled(fmt: Format) -> bool {
    match fmt {
        Format::Plain => false,
        Format::Html | Format::Vt => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodepointMap {
    /// Unicode codepoint range to replace.
    /// Asserts: range[0] <= range[1]
    pub range: [u32; 2],

    /// Replacement value for this range.
    pub replacement: Replacement,
}

/// (Zig: `CodepointMap.Replacement`.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replacement {
    /// A single replacement codepoint.
    Codepoint(u32),

    /// A UTF-8 encoded string to replace with.
    String(String),
}

/// Common encoding options regardless of what exact formatter is used.
#[derive(Debug, Clone)]
pub struct Options<'a> {
    /// The format to emit.
    pub emit: Format,

    /// Whether to unwrap soft-wrapped lines. If false, this will emit the
    /// screen contents as it is rendered on the page in the given size.
    pub unwrap: bool,

    /// Trim trailing whitespace on lines with other text. Trailing blank
    /// lines are always trimmed, except in VT output without trimming,
    /// where they are part of the layout. This only affects trailing
    /// whitespace on rows that have at least one other cell with text.
    /// Whitespace is currently only space characters (0x20).
    pub trim: bool,

    /// Replace matching Unicode codepoints with some other values.
    /// This will use the last matching range found in the list.
    pub codepoint_map: Option<Vec<CodepointMap>>,

    /// Set a background and foreground color to use for the "screen".
    /// For styled formats, this will emit the proper sequences or styles.
    pub background: Option<color::Rgb>,
    pub foreground: Option<color::Rgb>,

    /// If set, then styled formats in `emit` will use this palette to
    /// emit colors directly as RGB. If this is null, styled formats will
    /// still work but will use deferred palette styling (e.g. CSS variables
    /// for HTML or the actual palette indexes for VT).
    pub palette: Option<&'a color::Palette>,
}

impl Options<'static> {
    pub const PLAIN: Options<'static> = Options::new(Format::Plain);
    pub const VT: Options<'static> = Options::new(Format::Vt);
    pub const HTML: Options<'static> = Options::new(Format::Html);

    /// Options with the given format and all other fields defaulted.
    pub const fn new(emit: Format) -> Options<'static> {
        Options {
            emit,
            unwrap: false,
            trim: true,
            codepoint_map: Some(Vec::new()),
            background: None,
            foreground: None,
            palette: None,
        }
    }
}

/// Maps byte positions in formatted output to PageList pins.
///
/// Used by formatters that operate on PageLists to track the source position
/// of each byte written. The caller is responsible for freeing the map.
///
/// The mapping is stored in two parts: a per-byte x/y coordinate (8
/// bytes per output byte, half the size of a Pin) and a tiny table of
/// page nodes covering byte ranges (there are only ever a handful of
/// pages). This also lets page formatters write coordinates directly
/// into the map without a separate coordinate-to-pin conversion pass.
pub struct PinMap<'a> {
    pub map: &'a mut PinMapMap,
}

/// The type of the page node referenced by pins (Zig: `PinMap.Node`).
pub type PinMapNode = NodePtr;

/// A page node covering output bytes starting at `offset`
/// (inclusive) until the next entry's offset (or the end of the
/// output). (Zig: `PinMap.NodeRun`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinMapNodeRun {
    pub offset: usize,
    pub node: PinMapNode,
}

/// (Zig: `PinMap.Map`.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinMapMap {
    /// The x/y coordinate within its page for every output byte.
    pub points: Vec<Coordinate>,

    /// The page node for ranges of output bytes, ordered by offset.
    pub nodes: Vec<PinMapNodeRun>,
}

impl PinMapMap {
    pub const EMPTY: PinMapMap = PinMapMap {
        points: Vec::new(),
        nodes: Vec::new(),
    };

    pub fn deinit(&mut self) {
        self.points = Vec::new();
        self.nodes = Vec::new();
    }

    pub fn clear_retaining_capacity(&mut self) {
        self.points.clear();
        self.nodes.clear();
    }

    /// The total number of bytes mapped.
    pub fn count(&self) -> usize {
        self.points.len()
    }

    /// Set the page node for all bytes appended from here on,
    /// until the next call. No-op if the node is unchanged.
    pub fn set_node(&mut self, node: PinMapNode) {
        if let Some(last) = self.nodes.last() {
            if last.node == node {
                return;
            }
        }

        self.nodes.push(PinMapNodeRun {
            offset: self.points.len(),
            node,
        });
    }

    /// Append `n` bytes that map to `pin`.
    pub fn append(&mut self, pin: Pin, n: usize) {
        if n == 0 {
            return;
        }
        self.set_node(pin.node);
        let c = Coordinate {
            x: pin.x,
            y: pin.y as u32,
        };
        self.points.extend(std::iter::repeat_n(c, n));
    }

    /// Returns the pin that the byte at the given offset maps to,
    /// or null if the offset is out of range.
    pub fn get(&self, offset: usize) -> Option<Pin> {
        if offset >= self.points.len() {
            return None;
        }
        let coord = self.points[offset];
        let mut p = Pin::new(PinMap::find_node(&self.nodes, offset)?);
        p.x = coord.x;
        p.y = coord.y as CellCountInt;
        Some(p)
    }

    /// Returns the last pin in the map, if any.
    pub fn get_last_or_null(&self) -> Option<Pin> {
        let len = self.points.len();
        if len == 0 {
            return None;
        }
        self.get(len - 1)
    }
}

impl PinMap<'_> {
    /// Binary search for the node covering `offset` in a slice of node
    /// runs sorted by offset. Returns null only if the slice is empty
    /// or the offset precedes the first run.
    pub fn find_node(runs: &[PinMapNodeRun], offset: usize) -> Option<PinMapNode> {
        if runs.is_empty() || offset < runs[0].offset {
            return None;
        }
        let mut lo: usize = 0;
        let mut hi: usize = runs.len();
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if runs[mid].offset <= offset {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Some(runs[lo].node)
    }
}

// TODO(port): `TerminalFormatter` is pending the `Terminal` port.

/// A writer that only counts bytes (Zig: `std.Io.Writer.Discarding`).
#[derive(Debug, Default)]
struct Discarding {
    count: u64,
}

impl Write for Discarding {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.count += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_failed() -> io::Error {
    io::Error::other("WriteFailed")
}

/// Write a codepoint as UTF-8, replacing invalid codepoints with the
/// replacement character (Zig: `Writer.printUnicodeCodepoint`).
fn print_unicode_codepoint(writer: &mut dyn Write, cp: u32) -> io::Result<()> {
    let c = char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER);
    let mut buf = [0u8; 4];
    writer.write_all(c.encode_utf8(&mut buf).as_bytes())
}

/// Print a decimal into buf, returning the number of bytes
/// (Zig: `fastprint.printDecimal`).
fn print_decimal(buf: &mut [u8], v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut n = v;
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let len = tmp.len() - i;
    buf[..len].copy_from_slice(&tmp[i..]);
    len
}

pub struct ScreenFormatter<'a> {
    /// The screen to format.
    pub screen: &'a Screen,

    /// The common options
    pub opts: Options<'a>,

    /// The content to include.
    pub content: ScreenFormatterContent,

    /// Extra stuff to emit, such as cursor, style, hyperlinks, etc.
    /// This information is ONLY emitted when the format is "vt".
    pub extra: ScreenFormatterExtra,

    /// If non-null, then `map` will contain the Pin of every byte
    /// byte written to the writer offset by the byte index. It is the
    /// caller's responsibility to free the map.
    ///
    /// Note that some emitted bytes may not correspond to any Pin, such as
    /// the extra data around screen state. For these, we'll map it to the
    /// most previous pin so there is some continuity but its an arbitrary
    /// choice.
    ///
    /// Warning: there is a significant performance hit to track this
    pub pin_map: Option<PinMap<'a>>,

    /// The point the emitted cursor position (CUP) counts from. This is the
    /// top-left of the screen unless the output is replayed in origin mode
    /// (DECOM), where CUP counts from the top-left of the scrolling region.
    pub cursor_origin: CursorOrigin,
}

/// (Zig: `ScreenFormatter.Content`.)
#[derive(Debug, Clone, Copy)]
pub enum ScreenFormatterContent {
    /// Emit no content, only terminal state such as modes, palette, etc.
    /// via extra.
    None,

    /// Emit the content specified by the selection. Null for all.
    /// The selection is inclusive on both ends.
    Selection(Option<Selection>),
}

/// (Zig: `ScreenFormatter.CursorOrigin`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorOrigin {
    pub x: CellCountInt,
    pub y: CellCountInt,
}

/// (Zig: `ScreenFormatter.Extra`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScreenFormatterExtra {
    /// Emit cursor position using CUP (CSI H).
    pub cursor: bool,

    /// Emit current SGR style state based on the cursor's active style_id.
    /// This reconstructs the SGR attributes (bold, italic, colors, etc.) at
    /// the cursor position.
    pub style: bool,

    /// Emit current hyperlink state using OSC 8 sequences.
    /// This sets the active hyperlink based on cursor.hyperlink_id.
    pub hyperlink: bool,

    /// Emit character protection mode using DECSCA.
    pub protection: bool,

    /// Emit Kitty keyboard protocol state using CSI > u and CSI = sequences.
    pub kitty_keyboard: bool,

    /// Emit character set designations and invocations.
    /// This includes G0-G3 designations (ESC ( ) * +) and GL/GR invocations.
    pub charsets: bool,
}

impl ScreenFormatterExtra {
    /// Emit nothing.
    pub const NONE: ScreenFormatterExtra = ScreenFormatterExtra {
        cursor: false,
        style: false,
        hyperlink: false,
        protection: false,
        kitty_keyboard: false,
        charsets: false,
    };

    /// Emit style-relevant information only.
    pub const STYLES: ScreenFormatterExtra = ScreenFormatterExtra {
        cursor: false,
        style: true,
        hyperlink: true,
        protection: false,
        kitty_keyboard: false,
        charsets: false,
    };

    /// Emit everything. This reconstructs the screen state as closely
    /// as possible.
    pub const ALL: ScreenFormatterExtra = ScreenFormatterExtra {
        cursor: true,
        style: true,
        hyperlink: true,
        protection: true,
        kitty_keyboard: true,
        charsets: true,
    };

    fn is_set(&self) -> bool {
        self.cursor || self.style || self.hyperlink || self.protection || self.kitty_keyboard || self.charsets
    }
}

impl<'a> ScreenFormatter<'a> {
    pub fn init(screen: &'a Screen, opts: Options<'a>) -> ScreenFormatter<'a> {
        ScreenFormatter {
            screen,
            opts,
            content: ScreenFormatterContent::Selection(None),
            extra: ScreenFormatterExtra::NONE,
            pin_map: None,
            cursor_origin: CursorOrigin::default(),
        }
    }

    pub fn format(&mut self, writer: &mut dyn Write) -> io::Result<()> {
        match self.content {
            ScreenFormatterContent::None => {}

            ScreenFormatterContent::Selection(selection_) => {
                // Emit our pagelist contents according to our selection.
                let mut list_formatter = PageListFormatter::init(&self.screen.pages, self.opts.clone());
                list_formatter.pin_map = self.pin_map.as_mut().map(|m| PinMap { map: &mut *m.map });
                if let Some(sel) = selection_ {
                    list_formatter.top_left = Some(sel.top_left(self.screen));
                    list_formatter.bottom_right = Some(sel.bottom_right(self.screen));
                    list_formatter.rectangle = sel.rectangle;
                }
                list_formatter.format(writer)?;
            }
        }

        // Emit extra screen state after content if we care. The state has
        // to be emitted after since some state such as cursor position and
        // style are impacted by content rendering.
        match self.opts.emit {
            Format::Plain => return Ok(()),
            Format::Vt => {
                if !self.extra.is_set() {
                    return Ok(());
                }
            }

            // HTML doesn't preserve any screen state because it has
            // nothing to do with rendering.
            Format::Html => return Ok(()),
        }

        self.format_extra(writer)?;

        // If we have a pin_map, we need to count how many bytes the extras
        // will emit so we can map them all to the same pin. We do this by
        // formatting to a discarding writer with content=none.
        if let Some(m) = self.pin_map.as_mut() {
            let mut discarding = Discarding::default();
            let mut extra_formatter = ScreenFormatter {
                screen: self.screen,
                opts: self.opts.clone(),
                content: ScreenFormatterContent::None,
                extra: self.extra,
                pin_map: None,
                cursor_origin: self.cursor_origin,
            };
            extra_formatter.format(&mut discarding)?;

            // Map all those bytes to the same pin. Use the first page node
            // to ensure the node pointer is always properly initialized.
            let pin = m
                .map
                .get_last_or_null()
                .unwrap_or_else(|| self.screen.pages.get_top_left(Tag::Screen));
            let n = usize::try_from(discarding.count).map_err(|_| write_failed())?;
            m.map.append(pin, n);
        }

        Ok(())
    }

    fn format_extra(&self, writer: &mut dyn Write) -> io::Result<()> {
        let screen = self.screen;

        // Emit cursor position before the other extras because restoring a
        // pending wrap requires reprinting the cell at the right edge. That
        // print uses and changes active screen state, so the requested style,
        // hyperlink, protection, and charset must be restored afterwards.
        if self.extra.cursor {
            'cursor: {
                let cursor = &screen.cursor;
                let origin = self.cursor_origin;

                // If we don't have pending wrap, then we can just use CUP.
                if !cursor.pending_wrap || cursor.x != screen.pages.cols - 1 {
                    write!(
                        writer,
                        "\x1b[{};{}H",
                        cursor.y.saturating_sub(origin.y) as u32 + 1,
                        cursor.x.saturating_sub(origin.x) as u32 + 1,
                    )?;
                    break 'cursor;
                }

                // SAFETY: the cursor cell pointer is live.
                let page_cell = unsafe { *cursor.page_cell };

                // Pending wrap, we can't use CUP because it resets pending wrap.
                let start_x = match page_cell.wide() {
                    Wide::SpacerTail => cursor.x - 1,
                    Wide::Narrow | Wide::Wide | Wide::SpacerHead => cursor.x,
                };

                // Move cursor to the edge.
                write!(
                    writer,
                    "\x1b[{};{}H",
                    cursor.y.saturating_sub(origin.y) as u32 + 1,
                    start_x.saturating_sub(origin.x) as u32 + 1,
                )?;

                // A blank cell formats to nothing, which would leave the wrap
                // unset. Print a space to set it, save the cursor with the wrap
                // (DECSC), erase the space (ECH, which clears the wrap) and
                // restore the cursor (DECRC), which sets the wrap again over a
                // blank cell. This replaces the saved cursor, which the
                // formatter does not emit.
                if page_cell.wide() != Wide::SpacerTail && !page_cell.has_text() {
                    writer.write_all(b" \x1b7\x1b[X\x1b8")?;
                    break 'cursor;
                }

                // Reformat the cell which sets the proper pending wrap state.
                // SAFETY: the cursor pin is a live tracked pin.
                let pin = unsafe { *cursor.page_pin.as_ptr() };
                // SAFETY: valid pin.
                let page = unsafe { pin.node_mut().page() };
                let mut cell_formatter = PageFormatter::init(page, self.opts.clone());
                cell_formatter.start_x = cursor.x;
                cell_formatter.end_x = Some(cursor.x);
                cell_formatter.start_y = pin.y;
                cell_formatter.end_y = Some(pin.y);
                cell_formatter.format(writer)?;
            }
        }

        // Emit current SGR style state
        if self.extra.style {
            let cursor = &screen.cursor;
            write!(writer, "{}", cursor.style.formatter_vt())?;
        }

        // Emit current hyperlink state using OSC 8
        if self.extra.hyperlink {
            let cursor = &screen.cursor;
            if let Some(link) = &cursor.hyperlink {
                // Start hyperlink with uri (and explicit id if present)
                match &link.id {
                    hyperlink::HyperlinkId::Explicit(id) => {
                        writer.write_all(b"\x1b]8;id=")?;
                        writer.write_all(id)?;
                        writer.write_all(b";")?;
                        writer.write_all(&link.uri)?;
                        writer.write_all(b"\x1b\\")?;
                    }
                    hyperlink::HyperlinkId::Implicit(_) => {
                        writer.write_all(b"\x1b]8;;")?;
                        writer.write_all(&link.uri)?;
                        writer.write_all(b"\x1b\\")?;
                    }
                }
            }
        }

        // Emit character protection mode using DECSCA
        if self.extra.protection {
            let cursor = &screen.cursor;
            if cursor.protected {
                // DEC protected mode
                writer.write_all(b"\x1b[1\"q")?;
            }
        }

        // Emit Kitty keyboard protocol state using CSI = u
        if self.extra.kitty_keyboard {
            let current_flags = screen.kitty_keyboard.current();
            if current_flags.int() != kitty::KeyFlags::DISABLED.int() {
                let flags = current_flags.int();
                write!(writer, "\x1b[={flags};1u")?;
            }
        }

        // Emit character set designations and invocations
        if self.extra.charsets {
            let charset = &screen.charset;

            // Emit G0-G3 designations
            for slot in charsets::Slots::ALL {
                let cs = charset.charsets.get(slot);
                if cs != charsets::Charset::Utf8 {
                    // Only emit non-default charsets
                    let intermediate: u8 = match slot {
                        charsets::Slots::G0 => b'(',
                        charsets::Slots::G1 => b')',
                        charsets::Slots::G2 => b'*',
                        charsets::Slots::G3 => b'+',
                    };
                    let final_: u8 = match cs {
                        charsets::Charset::Ascii => b'B',
                        charsets::Charset::British => b'A',
                        charsets::Charset::DecSpecial => b'0',
                        _ => continue,
                    };
                    writer.write_all(&[0x1b, intermediate, final_])?;
                }
            }

            // Emit GL invocation if not G0
            if charset.gl != charsets::Slots::G0 {
                let seq: &[u8] = match charset.gl {
                    charsets::Slots::G0 => unreachable!(),
                    charsets::Slots::G1 => b"\x0e", // SO - Shift Out
                    charsets::Slots::G2 => b"\x1bn", // LS2
                    charsets::Slots::G3 => b"\x1bo", // LS3
                };
                writer.write_all(seq)?;
            }

            // Emit GR invocation if not G2
            if charset.gr != charsets::Slots::G2 {
                let seq: &[u8] = match charset.gr {
                    charsets::Slots::G0 => unreachable!(), // GR can't be G0
                    charsets::Slots::G1 => b"\x1b~",      // LS1R
                    charsets::Slots::G2 => unreachable!(),
                    charsets::Slots::G3 => b"\x1b|", // LS3R
                };
                writer.write_all(seq)?;
            }
        }

        Ok(())
    }
}

/// PageList formatter formats multiple pages as represented by a PageList.
pub struct PageListFormatter<'a> {
    /// The pagelist to format.
    pub list: &'a PageList,

    /// The common options
    pub opts: Options<'a>,

    /// The bounds of the PageList to format. The top left and bottom right
    /// MUST be ordered properly.
    pub top_left: Option<Pin>,
    pub bottom_right: Option<Pin>,

    /// If true, the boundaries define a rectangle selection where start_x
    /// and end_x apply to every row, not just the first and last.
    pub rectangle: bool,

    /// If non-null, then `map` will contain the Pin of every byte
    /// byte written to the writer offset by the byte index. It is the
    /// caller's responsibility to free the map.
    ///
    /// Warning: there is a significant performance hit to track this
    pub pin_map: Option<PinMap<'a>>,
}

impl<'a> PageListFormatter<'a> {
    pub fn init(list: &'a PageList, opts: Options<'a>) -> PageListFormatter<'a> {
        PageListFormatter {
            list,
            opts,
            top_left: None,
            bottom_right: None,
            rectangle: false,
            pin_map: None,
        }
    }

    pub fn format(&mut self, writer: &mut dyn Write) -> io::Result<()> {
        let tl: Pin = self.top_left.unwrap_or_else(|| self.list.get_top_left(Tag::Screen));
        let mut br: Pin = self
            .bottom_right
            .unwrap_or_else(|| self.list.get_bottom_right(Tag::Screen).unwrap());

        // If the end is a spacer head, move it onto the wrapped wide char,
        // which may be on another page.
        // SAFETY: valid pin.
        if self.opts.unwrap && !self.rectangle && unsafe { (*br.row_and_cell().cell).wide() } == Wide::SpacerHead {
            br = br.right_wrap(1).unwrap_or(br);
        }

        let mut page_state: Option<TrailingState> = None;
        let iter = tl.page_iterator(Direction::RightDown, Some(br));
        for chunk in iter {
            debug_assert!(chunk.start < chunk.end);
            debug_assert!(chunk.end > 0);

            // SAFETY: chunk nodes are live nodes of the list.
            let page = unsafe { (*chunk.node.as_ptr()).page() };
            let mut formatter = PageFormatter::init(page, self.opts.clone());
            formatter.start_y = chunk.start;
            formatter.end_y = Some(chunk.end - 1);
            formatter.trailing_state = page_state;
            formatter.rectangle = self.rectangle;

            // For rectangle selection, apply start_x and end_x to all chunks
            if self.rectangle {
                formatter.start_x = tl.x;
                formatter.end_x = Some(br.x);
            } else {
                // Otherwise only on the first/last, respectively.
                if chunk.node == tl.node {
                    formatter.start_x = tl.x;
                }
                if chunk.node == br.node {
                    formatter.end_x = Some(br.x);
                }
            }

            // If we're tracking pins, the page formatter writes its
            // per-byte coordinates directly into our map's point list
            // and we record which page node covers those bytes.
            if let Some(m) = self.pin_map.as_mut() {
                m.map.set_node(chunk.node);
                let base = m.map.points.len();
                formatter.point_map = Some(PointMap {
                    map: &mut m.map.points,
                    base,
                });
            }

            page_state = Some(formatter.format_with_state(writer)?);
        }

        // VT output without trimming keeps the blank rows at the end:
        // they are part of the layout, and a replay without them scrolls
        // less, so the rows above them end up too low on the screen.
        if self.opts.emit == Format::Vt && !self.opts.trim {
            let Some(state) = page_state else { return Ok(()) };
            if state.rows <= 1 {
                return Ok(());
            }

            // The last row needs no newline after it.
            let count = state.rows - 1;
            for _ in 0..count {
                writer.write_all(b"\r\n")?;
            }

            // Each newline maps to the row it ends, as between pages:
            // the first one to the end of the last row with text.
            if let Some(m) = self.pin_map.as_mut() {
                for i in 0..count {
                    let row = br.up(count - i).unwrap_or(tl);
                    let pin = if i == 0 {
                        m.map.get(m.map.count().saturating_sub(1)).unwrap_or(row)
                    } else {
                        row
                    };
                    let mut p = Pin::new(pin.node);
                    p.x = if i == 0 { pin.x } else { 0 };
                    p.y = pin.y;
                    m.map.append(p, 2);
                }
            }
        }

        Ok(())
    }
}

/// (Zig: `PageFormatter.PointMap`.)
pub struct PointMap<'a> {
    pub map: &'a mut Vec<Coordinate>,

    /// The index in `map` at which this formatter's output begins.
    /// Entries before this index belong to a caller (e.g. previous
    /// pages of a PageListFormatter) and aren't inspected. This
    /// exists so that callers can share one list across pages.
    pub base: usize,
}

impl PointMap<'_> {
    fn append_n(&mut self, c: Coordinate, n: usize) {
        self.map.extend(std::iter::repeat_n(c, n));
    }
}

/// Trailing state. This is used to ensure that rows wrapped across
/// multiple pages are unwrapped properly, as well as other accounting
/// we may do in the future. (Zig: `PageFormatter.TrailingState`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrailingState {
    pub rows: usize,
    pub cells: usize,
}

impl TrailingState {
    pub const EMPTY: TrailingState = TrailingState { rows: 0, cells: 0 };
}

/// Page formatter.
///
/// For styled formatting such as VT, this will emit references for palette
/// colors. If you want to capture the palette as-is at the type of formatting,
/// you'll have to emit the sequences for setting up the palette prior to
/// this formatting. (TODO: A function to do this)
pub struct PageFormatter<'a> {
    /// The page to format.
    pub page: &'a Page,

    /// The common options
    pub opts: Options<'a>,

    /// Start and end points within the page to format. If end x is not given
    /// then it will be the full width. If end y is not given then it will be
    /// the full height.
    ///
    /// The start and end are both inclusive, so equal values will still
    /// return a non-empty result (i.e. a single cell or row).
    ///
    /// The start x is considered the X in the first row and end X is
    /// X in the final row. This isn't a rectangle selection by default.
    ///
    /// If start X falls on the second column of a wide character, then
    /// the entire character will be included (as if you specified the
    /// previous column).
    pub start_x: CellCountInt,
    pub start_y: CellCountInt,
    pub end_x: Option<CellCountInt>,
    pub end_y: Option<CellCountInt>,

    /// If true, the start x/y and end x/y define a rectangle selection.
    /// In this case, the boundaries will apply to every row, not just
    /// the first and last.
    pub rectangle: bool,

    /// If non-null, then `map` will contain the x/y coordinate of every
    /// byte written to the writer offset by the byte index. It is the
    /// caller's responsibility to free the map.
    ///
    /// The x/y coordinate will be the coordinates within the page.
    ///
    /// Warning: there is a significant performance hit to track this
    pub point_map: Option<PointMap<'a>>,

    /// The previous trailing state from the prior page. If you're iterating
    /// over multiple pages this helps ensure that unwrapping and other
    /// accounting works properly.
    pub trailing_state: Option<TrailingState>,
}

/// The largest single-cell encoding must fit after a flush: the
/// HTML entity for the maximum codepoint ("&#2097151;") is 10
/// bytes, escapes are up to 6.
const MAX_ENCODING_LEN: usize = 16;

/// Style id marking a style that didn't come from an interned id.
const INVALID_STYLE_ID: u32 = u32::MAX;

impl<'a> PageFormatter<'a> {
    /// Initializes a page formatter. Other options can be set directly on the
    /// struct after initialization and before calling `format()`.
    pub fn init(page: &'a Page, opts: Options<'a>) -> PageFormatter<'a> {
        PageFormatter {
            page,
            opts,
            start_x: 0,
            start_y: 0,
            end_x: None,
            end_y: None,
            rectangle: false,
            point_map: None,
            trailing_state: None,
        }
    }

    pub fn format(&mut self, writer: &mut dyn Write) -> io::Result<()> {
        let _ = self.format_with_state(writer)?;
        Ok(())
    }

    pub fn format_with_state(&mut self, writer: &mut dyn Write) -> io::Result<TrailingState> {
        // Specialize the hot path on the emitted format so that the
        // per-cell loop contains no per-cell format dispatch.
        let emit = self.opts.emit;
        self.format_with_state_emit(writer, emit)
    }

    fn cells_of(&self, row: &Row) -> &'a [Cell] {
        // SAFETY: the row belongs to this page.
        unsafe { &*self.page.get_cells(row) }
    }

    fn format_with_state_emit(&mut self, writer: &mut dyn Write, emit: Format) -> io::Result<TrailingState> {
        let mut blank_rows: usize = 0;
        let mut blank_cells: usize = 0;

        // Continue our prior trailing state if we have it, but only if we're
        // starting from the beginning (start_y and start_x are both 0).
        // If a non-zero start position is specified, ignore trailing state.
        if let Some(state) = self.trailing_state {
            if self.start_y == 0 && self.start_x == 0 {
                blank_rows = state.rows;
                blank_cells = state.cells;
            }
        }

        let early = |blank_rows, blank_cells| TrailingState {
            rows: blank_rows,
            cells: blank_cells,
        };

        // Setup our starting column and perform some validation for overflows.
        // Note: start_x only applies to the first row, end_x only applies to the last row.
        let start_x: CellCountInt = self.start_x;
        if start_x >= self.page.size.cols {
            return Ok(early(blank_rows, blank_cells));
        }
        let end_x_unclamped: CellCountInt = self.end_x.unwrap_or(self.page.size.cols - 1);
        let end_x = end_x_unclamped.min(self.page.size.cols - 1);

        // Setup our starting row and perform some validation for overflows.
        let start_y: CellCountInt = self.start_y;
        if start_y >= self.page.size.rows {
            return Ok(early(blank_rows, blank_cells));
        }
        let end_y_unclamped: CellCountInt = self.end_y.unwrap_or(self.page.size.rows - 1);
        if start_y > end_y_unclamped {
            return Ok(early(blank_rows, blank_cells));
        }
        let end_y = end_y_unclamped.min(self.page.size.rows - 1);

        // If we only have a single row, validate that start_x <= end_x
        if start_y == end_y && start_x > end_x {
            return Ok(early(blank_rows, blank_cells));
        }

        // Wrap HTML output in monospace font styling
        match emit {
            Format::Plain => {}

            Format::Html => {
                // Setup our div. We use a buffer here that should always
                // fit the stuff we need, in order to make counting bytes easier.
                let mut buf: Vec<u8> = Vec::with_capacity(1024);

                // Monospace and whitespace preserving
                buf.extend_from_slice(b"<div style=\"font-family: monospace; white-space: pre;");

                // Background/foreground colors
                if let Some(bg) = self.opts.background {
                    write!(buf, "background-color: #{:02x}{:02x}{:02x};", bg.r, bg.g, bg.b)?;
                }
                if let Some(fg) = self.opts.foreground {
                    write!(buf, "color: #{:02x}{:02x}{:02x};", fg.r, fg.g, fg.b)?;
                }

                buf.extend_from_slice(b"\">");

                writer.write_all(&buf)?;
                if let Some(map) = self.point_map.as_mut() {
                    map.append_n(Coordinate { x: 0, y: 0 }, buf.len());
                }
            }

            Format::Vt => {
                // OSC 10 sets foreground color, OSC 11 sets background color
                let mut buf: Vec<u8> = Vec::with_capacity(512);
                if let Some(fg) = self.opts.foreground {
                    write!(buf, "\x1b]10;rgb:{:02x}/{:02x}/{:02x}\x1b\\", fg.r, fg.g, fg.b)?;
                }
                if let Some(bg) = self.opts.background {
                    write!(buf, "\x1b]11;rgb:{:02x}/{:02x}/{:02x}\x1b\\", bg.r, bg.g, bg.b)?;
                }

                writer.write_all(&buf)?;
                if let Some(map) = self.point_map.as_mut() {
                    map.append_n(Coordinate { x: 0, y: 0 }, buf.len());
                }
            }
        }

        // Our style for non-plain formats. Alongside the style itself we
        // track the page-local interned style id it corresponds to (styles
        // are interned per-page so id equality implies style equality).
        // The id is only a fast-path hint: it is set to `invalid_style_id`
        // whenever the current style didn't come from an interned id
        // (e.g. bg-color-only cells which synthesize styles).
        let mut style: Style = Style::default();
        let mut style_id: u32 = 0;

        // Whether the codepoint map has any entries. Hoisted out of the
        // per-codepoint path so that the common no-map case can use the
        // fast cell run path below.
        let cp_map_empty: bool = match &self.opts.codepoint_map {
            Some(m) => m.is_empty(),
            None => true,
        };

        // Track hyperlink state for HTML and VT output. We need to close
        // </a> tags or OSC 8 when the hyperlink changes or ends.
        let mut current_hyperlink_id: Option<hyperlink::Id> = None;

        // Most pages hold no hyperlinks, so check once instead of per cell.
        let page_hyperlinks = self.page.hyperlink_count() > 0;

        for y_usize in start_y as usize..end_y as usize + 1 {
            let y: CellCountInt = y_usize as CellCountInt;
            // SAFETY: y is within the page rows.
            let row: &Row = unsafe { &*self.page.get_row(y_usize) };
            let cells: &[Cell] = self.cells_of(row);

            // Determine the x range for this row
            // - First row: start_x to end of row (or end_x if single row)
            // - Last row: start of row to end_x
            // - Middle rows: full width
            // The end is always straightforward
            let row_end_x: CellCountInt = if self.rectangle || y == end_y {
                end_x + 1
            } else {
                self.page.size.cols
            };

            // The first we have to check if our start X falls on the
            // tail of a wide character.
            let row_start_x: CellCountInt = if start_x > 0 && (self.rectangle || y == start_y) {
                match cells[start_x as usize].wide() {
                    // Include the prior cell to get the full wide char
                    Wide::SpacerTail => start_x - 1,

                    // If we're a spacer head on our first row then we
                    // skip this whole row.
                    Wide::SpacerHead => continue,

                    Wide::Narrow | Wide::Wide => start_x,
                }
            } else {
                0
            };

            let cells_subset = &cells[row_start_x as usize..row_end_x as usize];

            // If this row is blank, accumulate to avoid a bunch of extra
            // work later. If it isn't blank, make sure we dump all our
            // blanks. In VT a row of cells that only carry a background
            // (erased while a background color was set) isn't blank,
            // since dropping it would lose the color.
            if !Cell::has_text_any(cells_subset) && !(emit == Format::Vt && has_background_any(cells_subset)) {
                blank_rows += 1;
                continue;
            }

            if blank_rows > 0 {
                // Reset style before emitting newlines to prevent background
                // colors from bleeding into the next line's leading cells.
                if !style.is_default() {
                    self.format_style_close(emit, writer)?;
                    style = Style::default();
                    style_id = 0;
                }

                let sequence: &[u8] = match emit {
                    // Plaintext just uses standard newlines because newlines
                    // on their own usually move the cursor back in anywhere
                    // you type plaintext.
                    Format::Plain => b"\n",

                    // VT uses \r\n because in a raw pty, \n alone doesn't
                    // guarantee moving the cursor back to column 0. \r
                    // makes it work for sure.
                    Format::Vt => b"\r\n",

                    // HTML uses just \n because HTML rendering will move
                    // the cursor back.
                    Format::Html => b"\n",
                };

                for _ in 0..blank_rows {
                    writer.write_all(sequence)?;
                }

                // \r and \n map to the row that ends with this newline.
                // If we're continuing (trailing state) then this will be
                // in a prior page, so we just map to the first row of this
                // page.
                if let Some(map) = self.point_map.as_mut() {
                    let start: Coordinate = if map.map.len() > map.base {
                        map.map[map.map.len() - 1]
                    } else {
                        Coordinate { x: 0, y: 0 }
                    };

                    // The first one inherits the x value.
                    map.append_n(
                        Coordinate {
                            x: start.x,
                            y: start.y,
                        },
                        sequence.len(),
                    );

                    // All others have x = 0 since they reference their prior
                    // blank line.
                    for y_offset in 1..blank_rows {
                        map.append_n(
                            Coordinate {
                                x: 0,
                                y: start.y + y_offset as u32,
                            },
                            sequence.len(),
                        );
                    }
                }

                blank_rows = 0;
            }

            // If we're not wrapped, we always add a newline so after
            // the row is printed we can add a newline.
            if !row.wrap() || !self.opts.unwrap {
                blank_rows += 1;
            }

            // If the row doesn't continue a wrap then we need to reset
            // our blank cell count.
            if !row.wrap_continuation() || !self.opts.unwrap {
                blank_cells = 0;
            }

            // Go through each cell and print it
            let mut cell_i: usize = 0;
            'cells: while cell_i < cells_subset.len() {
                let cell: &Cell = &cells_subset[cell_i];
                let x: CellCountInt = (row_start_x as usize + cell_i) as CellCountInt;

                // Fast path: runs of simple cells (single codepoint, no
                // style/hyperlink transitions) are encoded in batches,
                // avoiding all of the per-cell bookkeeping below. This is
                // only valid when we have no codepoint map and when our
                // current style/hyperlink state is known-stable.
                'fast: {
                    if !cp_map_empty {
                        break 'fast;
                    }
                    if format_styled(emit) && style_id == INVALID_STYLE_ID {
                        break 'fast;
                    }

                    // VT blanks carried over from a wrapped row (when
                    // unwrapping) can't be stepped over, so the run only
                    // starts with blanks on this row.
                    if emit == Format::Vt && blank_cells > cell_i {
                        break 'fast;
                    }

                    // Pages without hyperlinks take a copy of the run
                    // loop that never checks for them.
                    let hyperlinks = format_styled(emit) && page_hyperlinks;
                    let track_points = self.point_map.is_some();
                    let consumed = self.write_cell_run(
                        emit,
                        hyperlinks,
                        track_points,
                        writer,
                        &cells_subset[cell_i..],
                        x,
                        y,
                        style_id,
                        current_hyperlink_id,
                        &mut blank_cells,
                    )?;

                    // Zero cells consumed means the first cell isn't
                    // eligible for the fast path; handle it below.
                    if consumed == 0 {
                        break 'fast;
                    }

                    cell_i += consumed;
                    continue 'cells;
                }

                // Skip spacers. These happen naturally when wide characters
                // are printed again on the screen (for well-behaved terminals!)
                match cell.wide() {
                    Wide::Narrow | Wide::Wide => {}
                    Wide::SpacerHead | Wide::SpacerTail => {
                        cell_i += 1;
                        continue 'cells;
                    }
                }

                // If we have a zero value, then we accumulate a counter. We
                // only want to turn zero values into spaces if we have a non-zero
                // char sometime later.
                'blank: {
                    // If we're emitting styled output (not plaintext) and
                    // the cell has some kind of styling or is not empty
                    // then this isn't blank.
                    if format_styled(emit) && (!cell.is_empty() || cell.has_styling()) {
                        break 'blank;
                    }

                    // Cells with no text are blank
                    if !cell.has_text() {
                        blank_cells += 1;
                        cell_i += 1;
                        continue 'cells;
                    }

                    // Trailing spaces are blank. We know it is trailing
                    // because if we get a non-empty cell later we'll
                    // fill the blanks.
                    if cell.codepoint() == ' ' as u32 && self.opts.trim {
                        blank_cells += 1;
                        cell_i += 1;
                        continue 'cells;
                    }
                }

                // This cell is not blank. If we have accumulated blank cells
                // then we want to emit them now.
                if emit == Format::Vt && blank_cells > 0 {
                    if blank_cells <= cell_i {
                        // Blanks on this row are stepped over (CUF).
                        // Spaces would turn them into text, in the pen
                        // of the text before them.
                        self.write_blank_step(writer, blank_cells, x, y)?;
                        blank_cells = 0;
                    } else if !style.is_default() {
                        // Blanks carried over from a wrapped row can't
                        // be: CUF stops at the end of the row. They are
                        // written as spaces, without the pen of the text
                        // before them.
                        self.format_style_close(emit, writer)?;
                        style = Style::default();
                        style_id = 0;
                    }
                }

                if blank_cells > 0 {
                    // Blank cells have no hyperlink. In VT output an open
                    // hyperlink would apply to the spaces written for them,
                    // so close it first.
                    if emit == Format::Vt && current_hyperlink_id.is_some() {
                        self.format_hyperlink_close(emit, writer)?;
                        current_hyperlink_id = None;
                    }

                    for _ in 0..blank_cells {
                        writer.write_all(b" ")?;
                    }

                    if self.point_map.is_some() {
                        self.append_blank_points(blank_cells, x, y);
                    }

                    blank_cells = 0;
                }

                'style: {
                    // If we aren't emitting styled output then we don't
                    // have to worry about styles.
                    if !format_styled(emit) {
                        break 'style;
                    }

                    // Fast path: styles are interned per-page, so if this
                    // cell's style id matches the id of our current style
                    // then the style is unchanged.
                    let cell_style_id: u32 = match cell.content_tag() {
                        ContentTag::Codepoint | ContentTag::CodepointGrapheme => cell.style_id() as u32,
                        ContentTag::BgColorPalette | ContentTag::BgColorRgb => INVALID_STYLE_ID,
                    };
                    if cell_style_id == style_id && cell_style_id != INVALID_STYLE_ID {
                        break 'style;
                    }

                    // Get our cell style.
                    let cell_style = self.cell_style(cell);

                    // If the style hasn't changed, don't bloat output.
                    // When both ids are interned (and thus different, since
                    // equal ids broke out above), interning guarantees the
                    // styles differ so we can skip the comparison entirely.
                    if (cell_style_id == INVALID_STYLE_ID || style_id == INVALID_STYLE_ID) && cell_style.eql(&style) {
                        style_id = cell_style_id;
                        break 'style;
                    }

                    // If we had a previous style, we need to close it,
                    // because we've confirmed we have some new style
                    // (which is maybe default).
                    if !style.is_default() {
                        match emit {
                            Format::Html => self.format_style_close(emit, writer)?,

                            // For VT, we only close if we're switching to a default
                            // style because any non-default style will emit
                            // a \x1b[0m as the start of a VT coloring sequence.
                            Format::Vt => {
                                if cell_style.is_default() {
                                    self.format_style_close(emit, writer)?;
                                }
                            }

                            // Unreachable because of the styled() check at the
                            // top of this block.
                            Format::Plain => unreachable!(),
                        }
                    }

                    // At this point, we can copy our style over
                    style = cell_style;
                    style_id = cell_style_id;

                    // If we're just the default style now, we're done.
                    if cell_style.is_default() {
                        break 'style;
                    }

                    // New style, emit it.
                    self.format_style_open(emit, writer, &style)?;

                    // If we have a point map, we map the style to
                    // this cell.
                    if self.point_map.is_some() {
                        let mut discarding = Discarding::default();
                        self.format_style_open(emit, &mut discarding, &style)?;
                        let n = usize::try_from(discarding.count).map_err(|_| write_failed())?;
                        if let Some(map) = self.point_map.as_mut() {
                            map.append_n(Coordinate { x, y: y as u32 }, n);
                        }
                    }
                }

                // Hyperlink state
                'hyperlink: {
                    // HTML emits hyperlinks as anchors and VT as OSC 8.
                    if !format_styled(emit) {
                        break 'hyperlink;
                    }
                    if !page_hyperlinks {
                        break 'hyperlink;
                    }

                    // Get the hyperlink ID. This ID is our internal ID,
                    // not necessarily the OSC8 ID.
                    let link_id_: Option<hyperlink::Id> = if cell.hyperlink() {
                        self.page.lookup_hyperlink(cell)
                    } else {
                        None
                    };

                    // If our hyperlink IDs match (even null) then we have
                    // identical hyperlink state and we do nothing.
                    if current_hyperlink_id == link_id_ {
                        break 'hyperlink;
                    }

                    // If our prior hyperlink ID was non-null, we need to
                    // close it because the ID has changed. In VT, opening
                    // a hyperlink replaces the open one, so it is only
                    // closed when no hyperlink follows.
                    if current_hyperlink_id.is_some() && (emit == Format::Html || link_id_.is_none()) {
                        self.format_hyperlink_close(emit, writer)?;
                    }
                    current_hyperlink_id = None;

                    // Set our current hyperlink ID
                    let Some(link_id) = link_id_ else { break 'hyperlink };
                    current_hyperlink_id = Some(link_id);

                    // Emit the opening hyperlink tag
                    // SAFETY: page memory is the base of its hyperlink set.
                    let link = unsafe { *self.page.hyperlink_set.get(self.page.memory, link_id) };
                    self.format_hyperlink_open(emit, writer, &link)?;

                    // If we have a point map, we map the hyperlink to
                    // this cell.
                    if self.point_map.is_some() {
                        let n = self.hyperlink_open_len(emit, &link)?;
                        if let Some(map) = self.point_map.as_mut() {
                            map.append_n(Coordinate { x, y: y as u32 }, n);
                        }
                    }
                }

                match cell.content_tag() {
                    // We combine codepoint and graphemes because both have
                    // shared style handling. We use comptime to dup it.
                    tag @ (ContentTag::Codepoint | ContentTag::CodepointGrapheme) => {
                        self.write_cell(tag, emit, writer, cell)?;

                        // If we have a point map, all codepoints map to this
                        // cell.
                        if self.point_map.is_some() {
                            let mut discarding = Discarding::default();
                            self.write_cell(tag, emit, &mut discarding, cell)?;
                            let n = usize::try_from(discarding.count).map_err(|_| write_failed())?;
                            if let Some(map) = self.point_map.as_mut() {
                                map.append_n(Coordinate { x, y: y as u32 }, n);
                            }
                        }
                    }

                    // Cells with only background color (no text).
                    ContentTag::BgColorPalette | ContentTag::BgColorRgb => {
                        // VT restores them as they are made: erasing with
                        // the background set (ECH) leaves empty cells
                        // carrying it, where a space would be text. The
                        // pen is already the background, from the style
                        // block above. A run of the same background is
                        // erased at once, then stepped over (CUF).
                        if emit == Format::Vt {
                            let mut run: usize = 1;
                            while cell_i + run < cells_subset.len() && same_background(cell, &cells_subset[cell_i + run])
                            {
                                run += 1;
                            }

                            let seq = format!("\x1b[{run}X\x1b[{run}C");
                            writer.write_all(seq.as_bytes())?;
                            if let Some(map) = self.point_map.as_mut() {
                                map.append_n(Coordinate { x, y: y as u32 }, seq.len());
                            }

                            cell_i += run;
                            continue 'cells;
                        }

                        // HTML emits a space, in the background's style.
                        writer.write_all(b" ")?;
                        if let Some(map) = self.point_map.as_mut() {
                            map.map.push(Coordinate { x, y: y as u32 });
                        }
                    }
                }

                cell_i += 1;
            }
        }

        // If the style is non-default, we need to close our style tag.
        if !style.is_default() {
            self.format_style_close(emit, writer)?;
        }

        // Close any open hyperlink
        if current_hyperlink_id.is_some() {
            self.format_hyperlink_close(emit, writer)?;
        }

        // Close the monospace wrapper for HTML output
        if emit == Format::Html {
            let closing = b"</div>";
            writer.write_all(closing)?;
            if let Some(map) = self.point_map.as_mut() {
                let last = map.map[map.map.len() - 1];
                map.append_n(last, closing.len());
            }
            // Closing the div creates a newline in the output
            // so make sure to create one less newline
            if blank_rows >= 1 {
                blank_rows -= 1;
            }
        }

        Ok(TrailingState {
            rows: blank_rows,
            cells: blank_cells,
        })
    }

    /// Fast path for writing runs of simple cells: single-codepoint cells
    /// that require no style or hyperlink handling. Output bytes are
    /// batched into a stack buffer to avoid per-cell writer dispatch.
    /// Returns the number of cells consumed, which may be zero if the
    /// first cell isn't eligible for the fast path (in which case the
    /// caller must handle it via the slow path).
    ///
    /// Requirements (asserted by the caller, not here):
    ///
    ///   - The codepoint map is empty.
    ///   - For styled formats, `run_style_id` is the valid interned
    ///     page-local id of the currently active style.
    ///   - `run_hyperlink_id` is the open hyperlink (or none).
    ///
    /// `hyperlinks` is whether the page holds any hyperlinks. Without
    /// them the run never checks a cell's hyperlink state, which keeps
    /// the common case free of the per-cell check.
    ///
    /// `run_x`/`run_y` are the page coordinates of `cells[0]`, used for
    /// point map tracking.
    ///
    /// Blank cell accounting matches the slow path: accumulated blanks
    /// are only materialized once a non-blank cell is found, and any
    /// remainder is written back to `blank_cells`.
    #[allow(clippy::too_many_arguments)]
    fn write_cell_run(
        &mut self,
        emit: Format,
        hyperlinks: bool,
        track_points: bool,
        writer: &mut dyn Write,
        cells: &[Cell],
        run_x: CellCountInt,
        run_y: CellCountInt,
        run_style_id: u32,
        run_hyperlink_id: Option<hyperlink::Id>,
        blank_cells: &mut usize,
    ) -> io::Result<usize> {
        debug_assert!(track_points == self.point_map.is_some());

        let mut buf = [0u8; 512];
        let mut len: usize = 0;
        let mut pending: usize = *blank_cells;

        let mut i: usize = 0;
        while i < cells.len() {
            let cell = &cells[i];

            // Spacers produce no output, matching the slow path which
            // skips them before any blank/style handling.
            match cell.wide() {
                Wide::Narrow | Wide::Wide => {}
                Wide::SpacerHead | Wide::SpacerTail => {
                    i += 1;
                    continue;
                }
            }

            // Only text cells: bg-color cells synthesize styles and take
            // the slow path.
            match cell.content_tag() {
                ContentTag::Codepoint | ContentTag::CodepointGrapheme => {}
                ContentTag::BgColorPalette | ContentTag::BgColorRgb => break,
            }

            // Style transition, take the slow path.
            if format_styled(emit) && cell.style_id() as u32 != run_style_id {
                break;
            }

            let cp: u32 = cell.content_codepoint();

            // Blank cell accounting, matching the slow path blank block.
            if format_styled(emit) {
                // Styled formats only treat unstyled empty cells as
                // blank; anything else (including spaces) is written
                // so that styling is preserved.
                if cp == 0 && cell.wide() == Wide::Narrow && run_style_id == 0 {
                    pending += 1;
                    i += 1;
                    continue;
                }
            } else {
                // Cells with no text are blank.
                if cp == 0 {
                    pending += 1;
                    i += 1;
                    continue;
                }

                // Trailing spaces are blank.
                if cp == ' ' as u32 && self.opts.trim {
                    pending += 1;
                    i += 1;
                    continue;
                }
            }

            // Hyperlink state must be stable within a run: any non-blank
            // cell must belong to the currently open hyperlink (or none).
            // Transitions take the slow path. This is checked after blank
            // accounting because blank cells never touch hyperlink state.
            if hyperlinks {
                if cell.hyperlink() {
                    let Some(run_id) = run_hyperlink_id else { break };
                    let Some(cell_id) = self.page.lookup_hyperlink(cell) else { break };
                    if cell_id != run_id {
                        break;
                    }
                } else if run_hyperlink_id.is_some() {
                    break;
                }
            }

            // The page coordinate of this cell, for point tracking.
            let x: CellCountInt = (run_x as usize + i) as CellCountInt;

            // This cell produces output: materialize accumulated blanks.
            // VT steps over them (CUF), which the caller guarantees stays
            // on this row.
            if emit == Format::Vt && pending > 0 {
                writer.write_all(&buf[..len])?;
                len = 0;
                self.write_blank_step(writer, pending, x, run_y)?;
                pending = 0;
            }

            if pending > 0 {
                if track_points {
                    self.append_blank_points(pending, x, run_y);
                }

                while pending > 0 {
                    if len == buf.len() {
                        writer.write_all(&buf[..len])?;
                        len = 0;
                    }
                    let n = pending.min(buf.len() - len);
                    buf[len..len + n].fill(b' ');
                    len += n;
                    pending -= n;
                }
            }

            // Flush if the largest possible encoding may not fit.
            if len + MAX_ENCODING_LEN > buf.len() {
                writer.write_all(&buf[..len])?;
                len = 0;
            }

            let mut cell_bytes: usize;

            // Empty (but styled or wide) cells emit a space, matching
            // writeCell.
            if cp == 0 {
                buf[len] = b' ';
                len += 1;
                cell_bytes = 1;
            } else {
                cell_bytes = encode_codepoint(emit, &mut buf, &mut len, cp);

                // Multi-codepoint graphemes emit their extra codepoints,
                // matching writeCell. This is out-of-line to keep the
                // hot loop for the common single-codepoint case small.
                if cell.content_tag() == ContentTag::CodepointGrapheme {
                    cell_bytes += self.write_grapheme_cps(emit, writer, cell, &mut buf, &mut len)?;
                }
            }

            // All of the cell's bytes map to the cell's coordinate.
            if track_points {
                if let Some(map) = self.point_map.as_mut() {
                    map.append_n(Coordinate { x, y: run_y as u32 }, cell_bytes);
                }
            }

            i += 1;
        }

        if len > 0 {
            writer.write_all(&buf[..len])?;
        }
        *blank_cells = pending;
        Ok(i)
    }

    /// Encode the extra codepoints of a multi-codepoint grapheme into
    /// buf, flushing to the writer as needed. Returns the number of
    /// bytes written. This is deliberately not inlined so that the
    /// (rare) grapheme case doesn't bloat the writeCellRun hot loop.
    #[inline(never)]
    fn write_grapheme_cps(
        &self,
        emit: Format,
        writer: &mut dyn Write,
        cell: &Cell,
        buf: &mut [u8; 512],
        len: &mut usize,
    ) -> io::Result<usize> {
        let mut bytes: usize = 0;
        for &gcp in self.page.lookup_grapheme(cell).unwrap() {
            if *len + MAX_ENCODING_LEN > buf.len() {
                writer.write_all(&buf[..*len])?;
                *len = 0;
            }
            bytes += encode_codepoint(emit, buf, len, gcp);
        }
        Ok(bytes)
    }

    /// Step over `count` blank cells (CUF) just before the cell at
    /// (x, y), all on that row. The point map entries map to the first
    /// blank cell.
    fn write_blank_step(&mut self, writer: &mut dyn Write, count: usize, x: CellCountInt, y: CellCountInt) -> io::Result<()> {
        let seq = format!("\x1b[{count}C");
        writer.write_all(seq.as_bytes())?;
        if let Some(map) = self.point_map.as_mut() {
            map.append_n(
                Coordinate {
                    x: x - count as CellCountInt,
                    y: y as u32,
                },
                seq.len(),
            );
        }
        Ok(())
    }

    /// Append the point map entries for a run of `count` blank cells
    /// that are materialized as spaces just before the cell at (x, y).
    /// Blank cells can span multiple rows if they carry over from wrap
    /// continuation, so this walks backwards from (x, y).
    fn append_blank_points(&mut self, count: usize, x: CellCountInt, y: CellCountInt) {
        let cols = self.page.size.cols;
        let Some(map) = self.point_map.as_mut() else { return };
        map.map.reserve(count);

        let mut remaining = count;
        let mut blank_x = x;
        let mut blank_y = y;
        while remaining > 0 {
            if blank_x > 0 {
                // We have space in this row
                blank_x -= 1;
            } else if blank_y > 0 {
                // Wrap to previous row
                blank_y -= 1;
                blank_x = cols - 1;
            } else {
                // Can't go back further, just use (0, 0)
                blank_x = 0;
                blank_y = 0;
            }

            map.map.push(Coordinate {
                x: blank_x,
                y: blank_y as u32,
            });
            remaining -= 1;
        }
    }

    fn write_cell(&self, tag: ContentTag, emit: Format, writer: &mut dyn Write, cell: &Cell) -> io::Result<()> {
        // Blank cells get an empty space that isn't replaced by anything
        // because it isn't really a space. We do this so that formatting
        // is preserved if we're emitting styles.
        if !cell.has_text() {
            writer.write_all(b" ")?;
            return Ok(());
        }

        self.write_codepoint_with_replacement(emit, writer, cell.content_codepoint())?;
        if tag == ContentTag::CodepointGrapheme {
            for &cp in self.page.lookup_grapheme(cell).unwrap() {
                self.write_codepoint_with_replacement(emit, writer, cp)?;
            }
        }
        Ok(())
    }

    fn write_codepoint_with_replacement(&self, emit: Format, writer: &mut dyn Write, codepoint: u32) -> io::Result<()> {
        // Search for our replacement
        let r_: Option<&Replacement> = 'replacement: {
            let Some(map) = &self.opts.codepoint_map else { break 'replacement None };
            for entry in map.iter().rev() {
                let range = entry.range;
                if range[0] <= codepoint && codepoint <= range[1] {
                    break 'replacement Some(&entry.replacement);
                }
            }

            None
        };

        // If no replacement, write it directly.
        let Some(r) = r_ else {
            return write_codepoint(emit, writer, codepoint);
        };

        match r {
            Replacement::Codepoint(v) => write_codepoint(emit, writer, *v),

            Replacement::String(s) => {
                for cp in s.chars() {
                    write_codepoint(emit, writer, cp as u32)?;
                }
                Ok(())
            }
        }
    }

    /// Returns the style for the given cell. If there is no styling this
    /// will return the default style.
    fn cell_style(&self, cell: &Cell) -> Style {
        match cell.content_tag() {
            ContentTag::Codepoint | ContentTag::CodepointGrapheme => {
                if !cell.has_styling() {
                    Style::default()
                } else {
                    // SAFETY: page memory is the base of its style set.
                    unsafe { *self.page.styles.get(self.page.memory, cell.style_id()) }
                }
            }

            ContentTag::BgColorPalette => Style {
                bg_color: style::Color::Palette(cell.content_color_palette()),
                ..Style::default()
            },

            ContentTag::BgColorRgb => {
                let rgb = cell.content_color_rgb();
                Style {
                    bg_color: style::Color::Rgb(color::Rgb {
                        r: rgb.r,
                        g: rgb.g,
                        b: rgb.b,
                    }),
                    ..Style::default()
                }
            }
        }
    }

    /// Write a string with HTML escaping. Used for escaping href attributes
    /// and other HTML attribute values.
    fn format_style_open(&self, emit: Format, writer: &mut dyn Write, style: &Style) -> io::Result<()> {
        match emit {
            Format::Plain => unreachable!(),

            Format::Vt => {
                let mut formatter = style.formatter_vt();
                formatter.palette = self.opts.palette;
                write!(writer, "{formatter}")
            }

            // We use `display: inline` so that the div doesn't impact
            // layout since we're primarily using it as a CSS wrapper.
            Format::Html => {
                let mut formatter = style.formatter_html();
                formatter.palette = self.opts.palette;
                writer.write_all(b"<div style=\"display: inline;")?;
                write!(writer, "{formatter}")?;
                writer.write_all(b"\">")
            }
        }
    }

    fn format_style_close(&mut self, emit: Format, writer: &mut dyn Write) -> io::Result<()> {
        let str_: &[u8] = match emit {
            Format::Plain => return Ok(()),
            Format::Vt => b"\x1b[0m",
            Format::Html => b"</div>",
        };

        writer.write_all(str_)?;
        if let Some(m) = self.point_map.as_mut() {
            debug_assert!(!m.map.is_empty());
            let last = m.map[m.map.len() - 1];
            m.append_n(last, str_.len());
        }
        Ok(())
    }

    fn format_hyperlink_open(&self, emit: Format, writer: &mut dyn Write, link: &hyperlink::PageEntry) -> io::Result<()> {
        // SAFETY: the slices are in this page's memory.
        let uri = unsafe { link.uri.slice(self.page.memory) };
        match emit {
            Format::Plain => unreachable!(),

            // OSC 8, with the explicit id if there is one. Keep
            // hyperlinkOpenLen in sync.
            Format::Vt => match link.id {
                hyperlink::PageEntryId::Explicit(id) => {
                    writer.write_all(b"\x1b]8;id=")?;
                    // SAFETY: as above.
                    writer.write_all(unsafe { id.slice(self.page.memory) })?;
                    writer.write_all(b";")?;
                    writer.write_all(uri)?;
                    writer.write_all(b"\x1b\\")
                }
                hyperlink::PageEntryId::Implicit(_) => {
                    writer.write_all(b"\x1b]8;;")?;
                    writer.write_all(uri)?;
                    writer.write_all(b"\x1b\\")
                }
            },

            // layout since we're primarily using it as a CSS wrapper.
            Format::Html => {
                writer.write_all(b"<a href=\"")?;
                for &byte in uri {
                    write_codepoint(emit, writer, byte as u32)?;
                }
                writer.write_all(b"\">")
            }
        }
    }

    /// The number of bytes formatHyperlinkOpen writes for `link`.
    fn hyperlink_open_len(&self, emit: Format, link: &hyperlink::PageEntry) -> io::Result<usize> {
        match emit {
            Format::Plain => unreachable!(),

            // "\x1b]8;id=" ID ";" URI "\x1b\\" or "\x1b]8;;" URI "\x1b\\"
            Format::Vt => Ok(link.uri.len
                + match link.id {
                    hyperlink::PageEntryId::Explicit(id) => id.len + 10,
                    hyperlink::PageEntryId::Implicit(_) => 7,
                }),

            // The URI is escaped, so count the bytes.
            Format::Html => {
                let mut discarding = Discarding::default();
                self.format_hyperlink_open(emit, &mut discarding, link)?;
                usize::try_from(discarding.count).map_err(|_| write_failed())
            }
        }
    }

    fn format_hyperlink_close(&mut self, emit: Format, writer: &mut dyn Write) -> io::Result<()> {
        let str_: &[u8] = match emit {
            Format::Html => b"</a>",
            Format::Vt => b"\x1b]8;;\x1b\\",
            Format::Plain => return Ok(()),
        };

        writer.write_all(str_)?;
        if let Some(m) = self.point_map.as_mut() {
            debug_assert!(!m.map.is_empty());
            let last = m.map[m.map.len() - 1];
            m.append_n(last, str_.len());
        }
        Ok(())
    }
}

/// Encode a single codepoint into buf at len, advancing len and
/// returning the number of bytes written. The caller must guarantee
/// enough remaining buffer space for the largest possible encoding.
#[inline]
fn encode_codepoint(emit: Format, buf: &mut [u8; 512], len: &mut usize, cp: u32) -> usize {
    let start = *len;
    match emit {
        Format::Plain | Format::Vt => {
            if cp < 0x80 {
                buf[start] = cp as u8;
                *len += 1;
            } else {
                // Matches Writer.printUnicodeCodepoint: invalid
                // codepoints become the replacement character.
                let c = char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER);
                *len += c.encode_utf8(&mut buf[start..start + 4]).len();
            }
        }

        Format::Html => {
            let esc: Option<&[u8]> = match cp {
                0x3C => Some(b"&lt;"),
                0x3E => Some(b"&gt;"),
                0x26 => Some(b"&amp;"),
                0x22 => Some(b"&quot;"),
                0x27 => Some(b"&#39;"),
                _ => None,
            };
            if let Some(s) = esc {
                buf[start..start + s.len()].copy_from_slice(s);
                *len += s.len();
            } else if cp < 0x80 {
                // ASCII is emitted directly, everything else as a
                // numeric entity. See writeCodepoint.
                buf[start] = cp as u8;
                *len += 1;
            } else {
                buf[start..start + 2].copy_from_slice(b"&#");
                *len += 2;
                *len += print_decimal(&mut buf[*len..], cp);
                buf[*len] = b';';
                *len += 1;
            }
        }
    }

    *len - start
}

/// Whether any of the cells only carries a background color.
fn has_background_any(cells: &[Cell]) -> bool {
    for cell in cells {
        match cell.content_tag() {
            ContentTag::BgColorPalette | ContentTag::BgColorRgb => return true,
            ContentTag::Codepoint | ContentTag::CodepointGrapheme => {}
        }
    }
    false
}

/// Whether two cells both only carry the same background color.
fn same_background(a: &Cell, b: &Cell) -> bool {
    if a.content_tag() != b.content_tag() {
        return false;
    }
    match a.content_tag() {
        ContentTag::BgColorPalette => a.content_color_palette() == b.content_color_palette(),
        ContentTag::BgColorRgb => a.content_color_rgb() == b.content_color_rgb(),
        ContentTag::Codepoint | ContentTag::CodepointGrapheme => false,
    }
}

fn write_codepoint(emit: Format, writer: &mut dyn Write, codepoint: u32) -> io::Result<()> {
    match emit {
        Format::Plain | Format::Vt => print_unicode_codepoint(writer, codepoint),
        Format::Html => match codepoint {
            0x3C => writer.write_all(b"&lt;"),
            0x3E => writer.write_all(b"&gt;"),
            0x26 => writer.write_all(b"&amp;"),
            0x22 => writer.write_all(b"&quot;"),
            0x27 => writer.write_all(b"&#39;"),
            _ => {
                // For HTML, emit ASCII (< 0x80) directly, but encode
                // all non-ASCII as numeric entities to avoid encoding
                // detection issues (fixes #9426). We can't set the
                // meta tag because we emit partial HTML so this ensures
                // proper unicode handling.
                if codepoint < 0x80 {
                    writer.write_all(&[codepoint as u8])
                } else {
                    let mut buf = [0u8; 16];
                    buf[0..2].copy_from_slice(b"&#");
                    let mut len: usize = 2 + print_decimal(&mut buf[2..], codepoint);
                    buf[len] = b';';
                    len += 1;
                    writer.write_all(&buf[..len])
                }
            }
        },
    }
}

#[allow(dead_code)]
fn _size_check(_: size::CellCountInt) {}
