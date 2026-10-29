//! Port of `src/terminal/Selection.zig`.
//!
//! STATUS: ported
//!
//! Represents a single selection within the terminal (i.e. a highlight region).
//!
//! Notes on the port:
//! - Tracked bounds hold raw tracked-pin pointers ([`PinPtr`]) owned by the
//!   screen's PageList, exactly like the Zig `*Pin`. A selection is `Copy`.
//! - `startPtr`/`endPtr` return `&mut Pin` (into `self` for untracked
//!   bounds, into the PageList pin pool for tracked bounds).

use crate::terminal::page;
use crate::terminal::page_list::{Direction, Pin, PinPtr};
use crate::terminal::point::{self, Coordinate, Tag};
use crate::terminal::screen::Screen;
use crate::terminal::size::AllocError;

// NOTE(mitchellh): I'm not very happy with how this is implemented, because
// the ordering operations which are used frequently require using
// pointFromPin which -- at the time of writing this -- is slow. The overall
// style of this struct is due to porting it from the previous implementation
// which had an efficient ordering operation.
//
// While reimplementing this, there were too many callers that already
// depended on this behavior so I kept it despite the inefficiency. In the
// future, we should take a look at this again!

/// A single selection within the terminal.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    /// The bounds of the selection.
    pub bounds: Bounds,

    /// Whether or not this selection refers to a rectangle, rather than whole
    /// lines of a buffer. In this mode, start and end refer to the top left and
    /// bottom right of the rectangle, or vice versa if the selection is backwards.
    pub rectangle: bool,
}

/// The bounds of the selection. A selection bounds can be either tracked
/// or untracked. Untracked bounds are unsafe beyond the point the terminal
/// screen may be modified, since they may point to invalid memory. Tracked
/// bounds are always valid and will be updated as the screen changes, but
/// are more expensive to exist.
///
/// In all cases, start and end can be in any order. There is no guarantee that
/// start is before end or vice versa. If a user selects backwards,
/// start will be after end, and vice versa. Use the struct functions
/// to not have to worry about this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bounds {
    Untracked { start: Pin, end: Pin },
    Tracked { start: PinPtr, end: PinPtr },
}

/// The order of the selection:
///
///  * forward: start(x, y) is before end(x, y) (top-left to bottom-right).
///  * reverse: end(x, y) is before start(x, y) (bottom-right to top-left).
///  * mirrored_[forward|reverse]: special, rectangle selections only (see below).
///
///  For regular selections, the above also holds for top-right to bottom-left
///  (forward) and bottom-left to top-right (reverse). However, for rectangle
///  selections, both of these selections are *mirrored* as orientation
///  operations only flip the x or y axis, not both. Depending on the y axis
///  direction, this is either mirrored_forward or mirrored_reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum Order {
    Forward,
    Reverse,
    MirroredForward,
    MirroredReverse,
}

/// Possible adjustments to the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub enum Adjustment {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    BeginningOfLine,
    EndOfLine,
}

/// Dereference a tracked pin.
#[inline(always)]
fn tp<'a>(p: PinPtr) -> &'a mut Pin {
    // SAFETY: tracked pins of a selection are live while the selection is
    // tracked by its screen (the selection contract).
    unsafe { &mut *p.as_ptr() }
}

/// The number of columns of the pin's node.
#[inline(always)]
fn pin_cols(p: &Pin) -> u16 {
    // SAFETY: valid pin (Pin contract).
    unsafe { p.node_mut().cols() }
}

impl Selection {
    /// Initialize a new selection with the given start and end pins on
    /// the screen. The screen will be used for pin tracking.
    pub fn init(start_pin: Pin, end_pin: Pin, rect: bool) -> Selection {
        Selection {
            bounds: Bounds::Untracked {
                start: start_pin,
                end: end_pin,
            },
            rectangle: rect,
        }
    }

    pub fn deinit(self, s: &mut Screen) {
        match self.bounds {
            Bounds::Tracked { start, end } => {
                s.pages.untrack_pin(start);
                s.pages.untrack_pin(end);
            }
            Bounds::Untracked { .. } => {}
        }
    }

    /// Returns true if this selection is equal to another selection.
    pub fn eql(&self, other: Selection) -> bool {
        self.start().eql(other.start()) && self.end().eql(other.end()) && self.rectangle == other.rectangle
    }

    /// The starting pin of the selection. This is NOT ordered.
    pub fn start_ptr(&mut self) -> &mut Pin {
        match &mut self.bounds {
            Bounds::Untracked { start, .. } => start,
            Bounds::Tracked { start, .. } => tp(*start),
        }
    }

    /// The ending pin of the selection. This is NOT ordered.
    pub fn end_ptr(&mut self) -> &mut Pin {
        match &mut self.bounds {
            Bounds::Untracked { end, .. } => end,
            Bounds::Tracked { end, .. } => tp(*end),
        }
    }

    pub fn start(&self) -> Pin {
        match self.bounds {
            Bounds::Untracked { start, .. } => start,
            Bounds::Tracked { start, .. } => *tp(start),
        }
    }

    pub fn end(&self) -> Pin {
        match self.bounds {
            Bounds::Untracked { end, .. } => end,
            Bounds::Tracked { end, .. } => *tp(end),
        }
    }

    /// Returns true if this is a tracked selection.
    pub fn tracked(&self) -> bool {
        match self.bounds {
            Bounds::Untracked { .. } => false,
            Bounds::Tracked { .. } => true,
        }
    }

    /// Convert this selection a tracked selection. It is asserted this is
    /// an untracked selection. The tracked selection is returned.
    pub fn track(&self, s: &mut Screen) -> Result<Selection, AllocError> {
        debug_assert!(!self.tracked());

        // Track our pins
        let Bounds::Untracked { start: start_pin, end: end_pin } = self.bounds else {
            unreachable!()
        };
        let tracked_start = s.pages.track_pin(start_pin)?;
        let tracked_end = match s.pages.track_pin(end_pin) {
            Ok(v) => v,
            Err(e) => {
                s.pages.untrack_pin(tracked_start);
                return Err(e);
            }
        };

        Ok(Selection {
            bounds: Bounds::Tracked {
                start: tracked_start,
                end: tracked_end,
            },
            rectangle: self.rectangle,
        })
    }

    /// Returns the top left point of the selection.
    pub fn top_left(&self, s: &Screen) -> Pin {
        match self.order(s) {
            Order::Forward => self.start(),
            Order::Reverse => self.end(),
            Order::MirroredForward => {
                let mut p = self.start();
                p.x = self.end().x.min(pin_cols(&p) - 1);
                p
            }
            Order::MirroredReverse => {
                let mut p = self.end();
                p.x = self.start().x.min(pin_cols(&p) - 1);
                p
            }
        }
    }

    /// Returns the bottom right point of the selection.
    pub fn bottom_right(&self, s: &Screen) -> Pin {
        match self.order(s) {
            Order::Forward => self.end(),
            Order::Reverse => self.start(),
            Order::MirroredForward => {
                let mut p = self.end();
                p.x = self.start().x.min(pin_cols(&p) - 1);
                p
            }
            Order::MirroredReverse => {
                let mut p = self.start();
                p.x = self.end().x.min(pin_cols(&p) - 1);
                p
            }
        }
    }

    pub fn order(&self, s: &Screen) -> Order {
        let start_pt = s.pages.point_from_pin(Tag::Screen, self.start()).unwrap().coord();
        let end_pt = s.pages.point_from_pin(Tag::Screen, self.end()).unwrap().coord();

        if self.rectangle {
            // Reverse (also handles single-column)
            if start_pt.y > end_pt.y && start_pt.x >= end_pt.x {
                return Order::Reverse;
            }
            if start_pt.y >= end_pt.y && start_pt.x > end_pt.x {
                return Order::Reverse;
            }

            // Mirror, bottom-left to top-right
            if start_pt.y > end_pt.y && start_pt.x < end_pt.x {
                return Order::MirroredReverse;
            }

            // Mirror, top-right to bottom-left
            if start_pt.y < end_pt.y && start_pt.x > end_pt.x {
                return Order::MirroredForward;
            }

            // Forward
            return Order::Forward;
        }

        if start_pt.y < end_pt.y {
            return Order::Forward;
        }
        if start_pt.y > end_pt.y {
            return Order::Reverse;
        }
        if start_pt.x <= end_pt.x {
            return Order::Forward;
        }
        Order::Reverse
    }

    /// Returns the selection in the given order.
    ///
    /// The returned selection is always a new untracked selection.
    ///
    /// Note that only forward and reverse are useful desired orders for this
    /// function. All other orders act as if forward order was desired.
    pub fn ordered(&self, s: &Screen, desired: Order) -> Selection {
        if self.order(s) == desired {
            return Selection::init(self.start(), self.end(), self.rectangle);
        }

        let tl = self.top_left(s);
        let br = self.bottom_right(s);
        match desired {
            Order::Forward => Selection::init(tl, br, self.rectangle),
            Order::Reverse => Selection::init(br, tl, self.rectangle),
            _ => Selection::init(tl, br, self.rectangle),
        }
    }

    /// Returns true if the selection contains the given point.
    ///
    /// This recalculates top left and bottom right each call. If you have
    /// many points to check, it is cheaper to do the containment logic
    /// yourself and cache the topleft/bottomright.
    pub fn contains(&self, s: &Screen, pin: Pin) -> bool {
        let tl_pin = self.top_left(s);
        let br_pin = self.bottom_right(s);

        // This is definitely not very efficient. Low-hanging fruit to
        // improve this.
        let tl = s.pages.point_from_pin(Tag::Screen, tl_pin).unwrap().coord();
        let br = s.pages.point_from_pin(Tag::Screen, br_pin).unwrap().coord();
        let p = s.pages.point_from_pin(Tag::Screen, pin).unwrap().coord();

        // If we're in rectangle select, we can short-circuit with an easy check
        // here
        if self.rectangle {
            return p.y >= tl.y && p.y <= br.y && p.x >= tl.x && p.x <= br.x;
        }

        // If tl/br are same line
        if tl.y == br.y {
            return p.y == tl.y && p.x >= tl.x && p.x <= br.x;
        }

        // If on top line, just has to be left of X
        if p.y == tl.y {
            return p.x >= tl.x;
        }

        // If on bottom line, just has to be right of X
        if p.y == br.y {
            return p.x <= br.x;
        }

        // If between the top/bottom, always good.
        p.y > tl.y && p.y < br.y
    }

    /// Get a selection for a single row in the screen. This will return null
    /// if the row is not included in the selection.
    ///
    /// This is a very expensive operation. It has to traverse the linked list
    /// of pages for the top-left, bottom-right, and the given pin to find
    /// the coordinates. If you are calling this repeatedly, prefer
    /// `containedRowCached`.
    pub fn contained_row(&self, s: &Screen, pin: Pin) -> Option<Selection> {
        let tl_pin = self.top_left(s);
        let br_pin = self.bottom_right(s);

        // This is definitely not very efficient. Low-hanging fruit to
        // improve this. Callers should prefer containedRowCached if they
        // can swing it.
        let tl = s.pages.point_from_pin(Tag::Screen, tl_pin).unwrap().coord();
        let br = s.pages.point_from_pin(Tag::Screen, br_pin).unwrap().coord();
        let p = s.pages.point_from_pin(Tag::Screen, pin).unwrap().coord();

        self.contained_row_cached(s, tl_pin, br_pin, pin, tl, br, p)
    }

    /// Same as containedRow but useful if you're calling it repeatedly
    /// so that the pins can be cached across calls. Advanced.
    #[allow(clippy::too_many_arguments)]
    pub fn contained_row_cached(
        &self,
        _s: &Screen,
        tl_pin: Pin,
        br_pin: Pin,
        pin: Pin,
        tl: Coordinate,
        br: Coordinate,
        p: Coordinate,
    ) -> Option<Selection> {
        if p.y < tl.y || p.y > br.y {
            return None;
        }

        // Rectangle case: we can return early as the x range will always be the
        // same. We've already validated that the row is in the selection.
        if self.rectangle {
            let mut start = pin;
            start.x = tl.x.min(pin_cols(&start) - 1);
            let mut end = pin;
            end.x = br.x.min(pin_cols(&end) - 1);
            return Some(Selection::init(start, end, true));
        }

        if p.y == tl.y {
            // If the selection is JUST this line, return it as-is.
            if p.y == br.y {
                return Some(Selection::init(tl_pin, br_pin, false));
            }

            // Selection top-left line matches only.
            let mut end = pin;
            end.x = pin_cols(&end) - 1;
            return Some(Selection::init(tl_pin, end, false));
        }

        // Row is our bottom selection, so we return the selection from the
        // beginning of the line to the br. We know our selection is more than
        // one line (due to conditionals above)
        if p.y == br.y {
            debug_assert!(p.y != tl.y);
            let mut start = pin;
            start.x = 0;
            return Some(Selection::init(start, br_pin, false));
        }

        // Row is somewhere between our selection lines so we return the full line.
        let mut start = pin;
        start.x = 0;
        let mut end = pin;
        end.x = pin_cols(&end) - 1;
        Some(Selection::init(start, end, false))
    }

    /// Adjust the selection by some given adjustment. An adjustment allows
    /// a selection to be expanded slightly left, right, up, down, etc.
    pub fn adjust(&mut self, s: &Screen, adjustment: Adjustment) {
        // Note that we always adjust "end" because end always represents
        // the last point of the selection by mouse, not necessarily the
        // top/bottom visually. So this results in the correct behavior
        // whether the user drags up or down.
        match adjustment {
            Adjustment::Up => {
                let end_pin = self.end_ptr();
                if let Some(new_end) = end_pin.up(1) {
                    *end_pin = new_end;
                } else {
                    self.adjust(s, Adjustment::BeginningOfLine);
                }
            }

            Adjustment::Down => {
                // Find the next non-blank row
                let end_pin = self.end_ptr();
                let mut current = *end_pin;
                let mut found = false;
                while let Some(next) = current.down(1) {
                    let rac = next.row_and_cell();
                    // SAFETY: valid pin.
                    let cells = unsafe { &*next.node_mut().page().get_cells(&*rac.row) };
                    if page::Cell::has_text_any(cells) {
                        *end_pin = next;
                        found = true;
                        break;
                    }
                    current = next;
                }
                if !found {
                    // If we're at the bottom, just go to the end of the line
                    self.adjust(s, Adjustment::EndOfLine);
                }
            }

            Adjustment::Left => {
                let end_pin = self.end_ptr();
                let mut it = end_pin.cell_iterator(Direction::LeftUp, None);
                let _ = it.next();
                for next in it {
                    // SAFETY: valid pin.
                    let rac = next.row_and_cell();
                    if unsafe { (*rac.cell).has_text() } {
                        *end_pin = next;
                        break;
                    }
                }
            }

            Adjustment::Right => {
                // Step right, wrapping to the next row down at the start of each new line,
                // until we find a non-empty cell.
                let end_pin = self.end_ptr();
                let mut it = end_pin.cell_iterator(Direction::RightDown, None);
                let _ = it.next();
                for next in it {
                    let rac = next.row_and_cell();
                    // SAFETY: valid pin.
                    if unsafe { (*rac.cell).has_text() } {
                        *end_pin = next;
                        break;
                    }
                }
            }

            Adjustment::PageUp => {
                let end_pin = self.end_ptr();
                if let Some(new_end) = end_pin.up(s.pages.rows as usize) {
                    *end_pin = new_end;
                } else {
                    self.adjust(s, Adjustment::Home);
                }
            }

            Adjustment::PageDown => {
                let end_pin = self.end_ptr();
                if let Some(new_end) = end_pin.down(s.pages.rows as usize) {
                    *end_pin = new_end;
                } else {
                    self.adjust(s, Adjustment::End);
                }
            }

            Adjustment::Home => {
                *self.end_ptr() = s.pages.pin(point::Point::Screen(Coordinate { x: 0, y: 0 })).unwrap();
            }

            Adjustment::End => {
                let end_pin = self.end_ptr();
                let it = s.pages.row_iterator(Direction::LeftUp, point::Point::Screen(Coordinate::default()), None);
                for next in it {
                    let rac = next.row_and_cell();
                    // SAFETY: valid pin.
                    let cells = unsafe { &*next.node_mut().page().get_cells(&*rac.row) };
                    if page::Cell::has_text_any(cells) {
                        *end_pin = next;
                        end_pin.x = (cells.len() - 1) as u16;
                        break;
                    }
                }
            }

            Adjustment::BeginningOfLine => self.end_ptr().x = 0,

            Adjustment::EndOfLine => {
                let end_pin = self.end_ptr();
                end_pin.x = pin_cols(end_pin) - 1;
            }
        }
    }
}
