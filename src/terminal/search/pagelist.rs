//! Port of `src/terminal/search/pagelist.zig`.
//!
//! STATUS: ported
//!
//! Notes on the port:
//! - The PageList and the tracked pin are raw pointers like in Zig: the
//!   caller guarantees the PageList outlives the search (or calls
//!   [`PageListSearch::deinit_list_invalid`]) and holds the necessary locks
//!   for every call that accesses it.
//! - `deinit` stays explicit (it untracks the pin) because the
//!   list-invalid variant must not touch the PageList.

use core::ptr::NonNull;

use crate::terminal::highlight::Flattened as FlattenedHighlight;
use crate::terminal::page_list::{Allocator, NodePtr, PageList, Pin, PinPtr};
use crate::terminal::search::sliding_window::{Direction, SlidingWindow};
use crate::terminal::size::AllocError;

/// Searches for a term in a PageList structure.
///
/// This searches in reverse order starting from the given node.
///
/// This assumes that nodes do not change contents. For nodes that change
/// contents, look at ActiveSearch, which is designed to re-search the active
/// area since it assumed to change. When integrating ActiveSearch with
/// PageListSearch, the caller should start the PageListSearch from the
/// returned node from ActiveSearch.update().
///
/// Concurrent access to a PageList or nodes in a PageList are not allowed,
/// so the caller should ensure that necessary locks are held. Each function
/// documents whether it accesses the PageList or not. For example, you can
/// safely call `next()` without holding a lock, but you must hold a lock
/// while calling `feed()`.
#[derive(Debug)]
pub struct PageListSearch {
    /// The list we're searching.
    pub list: NonNull<PageList>,

    /// The sliding window of page contents and nodes to search.
    pub window: SlidingWindow,

    /// The tracked pin for our current position in the pagelist. This
    /// will always point to the CURRENT node we're searching from so that
    /// we can track if we move.
    pub pin: PinPtr,
}

// SAFETY: the raw pointers are only dereferenced under the caller's
// synchronization (see the type docs), exactly like the Zig version.
unsafe impl Send for PageListSearch {}

impl PageListSearch {
    /// Initialize the page list search. The needle is copied so it can
    /// be freed immediately.
    ///
    /// Accesses the PageList/Node so the caller must ensure it is safe
    /// to do so if there is any concurrent access.
    pub fn init(
        alloc: Allocator,
        needle: &[u8],
        list: &mut PageList,
        start: NodePtr,
    ) -> Result<PageListSearch, AllocError> {
        // We put a tracked pin into the node that we're starting from.
        // By using a tracked pin, we can keep our pagelist references safe
        // because if the pagelist prunes pages, the tracked pin will
        // be moved somewhere safe.
        // SAFETY: start is a live node of `list`.
        let (rows, cols) = unsafe { (start.as_ref().rows(), start.as_ref().cols()) };
        let pin = list.track_pin(Pin {
            y: rows - 1,
            x: cols - 1,
            ..Pin::new(start)
        })?;

        // Create our sliding window we'll use for searching.
        let mut window = match SlidingWindow::init(alloc, Direction::Reverse, needle) {
            Ok(w) => w,
            Err(e) => {
                list.untrack_pin(pin);
                return Err(e);
            }
        };

        // We always feed our initial page data into the window, because
        // we have the lock anyways and this lets our `pin` point to our
        // current node and feed to work properly.
        if let Err(e) = window.append(start) {
            list.untrack_pin(pin);
            return Err(e);
        }

        Ok(PageListSearch {
            list: NonNull::from(list),
            window,
            pin,
        })
    }

    /// Modifies the PageList (to untrack a pin) so the caller must ensure
    /// that it is safe to do so.
    pub fn deinit(&mut self) {
        self.window.clear_and_retain_capacity();
        // SAFETY: the list outlives the search (caller contract).
        unsafe { self.list.as_mut().untrack_pin(self.pin) };
    }

    /// Release owned search buffers after the PageList itself has already
    /// been destroyed. Its pin belonged to the PageList pool and was freed
    /// with that list, so it must not be untracked here.
    pub fn deinit_list_invalid(&mut self) {
        self.window.clear_and_retain_capacity();
    }

    /// Return the next match in the loaded page nodes. If this returns
    /// null then the PageList search needs to be fed the next node(s).
    /// Call, `feed` to do this.
    ///
    /// Beware that the selection returned may point to a node that
    /// is freed if the caller does not hold necessary locks on the
    /// PageList while searching. The pins should be validated prior to
    /// final use.
    ///
    /// This does NOT access the PageList, so it can be called without
    /// a lock held.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<FlattenedHighlight> {
        self.window.next()
    }

    /// Feed more data to the sliding window from the pagelist. This will
    /// feed enough data to cover at least one match (needle length) if it
    /// exists; this doesn't perform the search, it only feeds data.
    ///
    /// This accesses nodes in the PageList, so the caller must ensure
    /// it is safe to do so (i.e. hold necessary locks).
    ///
    /// This returns false if there is no more data to feed. This essentially
    /// means we've searched the entire pagelist.
    pub fn feed(&mut self) -> Result<bool, AllocError> {
        // SAFETY: the pin is tracked by the live list (caller contract).
        let pin = unsafe { &mut *self.pin.as_ptr() };

        // If our pin becomes garbage it means wherever we were next
        // was reused and we can't make sense of our progress anymore.
        // It is effectively equivalent to reaching the end of the PageList.
        if pin.garbage {
            return Ok(false);
        }

        // Add at least enough data to find a single match.
        let mut rem = self.window.needle.len();

        // Start at our previous node and then continue adding until we
        // get our desired amount of data.
        // SAFETY: pin nodes are live.
        let mut node_ = unsafe { pin.node.as_ref().prev };
        while let Some(node) = node_ {
            rem = rem.saturating_sub(self.window.append(node)?.content_len);

            // Move our tracked pin to the new node.
            // SAFETY: node is live.
            let (rows, cols) = unsafe { (node.as_ref().rows(), node.as_ref().cols()) };
            pin.node = node;
            pin.y = rows - 1;
            pin.x = cols - 1;
            pin.garbage = false;

            if rem == 0 {
                break;
            }
            // SAFETY: node is live.
            node_ = unsafe { node.as_ref().prev };
        }

        // True if we fed any data.
        Ok(rem < self.window.needle.len())
    }
}
