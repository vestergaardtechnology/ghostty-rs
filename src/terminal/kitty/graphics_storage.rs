//! Port of `src/terminal/kitty/graphics_storage.zig`.
//!
//! STATUS: partial — only the `ImageStorage` state and the API that
//! `Screen` calls are ported (`nextGeneration`, `deinit`, `enabled`,
//! `markMutated`, `markDirty`, `setLimit`, `eraseHistory`). The image and
//! placement maps (`images`, `placements`, `loading`), `Placement`,
//! `PendingImage`, `addImage`/`addPlacement`, eviction, deletion, scroll
//! margins, animation and all tests are pending the `graphics_image` port.
//! Until then the storage never holds images or placements, so the
//! operations below that walk them are no-ops.
//!
//! Notes on the port:
//! - Functions that take a `*Screen` in Zig only touch `screen.pages`, so
//!   they take `&mut PageList`/`&PageList` here (the storage is a field of
//!   the screen, so borrowing the whole screen is impossible in Rust).
//! - `std.Io` parameters are dropped.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::terminal::kitty::graphics_image::Limits as LoadingImageLimits;
use crate::terminal::page_list::PageList;
use crate::terminal::point;

const LOG_TARGET: &str = "kitty_gfx";

/// Process-global counter backing all generation stamps (see
/// ImageStorage.generation and Image.generation). This is global rather
/// than per-storage so that stamps are unique across every storage in
/// the process: two mutation events never produce the same value, even
/// across separate screens (main vs. alt), storage resets, or separate
/// terminals. This lets consumers use a generation value alone as a
/// cache key without any ambiguity.
///
/// Thread-safe because separate terminals may mutate their storages
/// from different threads.
static GENERATION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns the next generation stamp. Stamps are unique and strictly
/// monotonically increasing process-wide, starting at 1 (0 is reserved
/// to mean "never stamped").
pub fn next_generation() -> u64 {
    GENERATION_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

/// An image storage is associated with a terminal screen (i.e. main
/// screen, alt screen) and contains all the transmitted images and
/// placements.
#[derive(Debug, Clone)]
pub struct ImageStorage {
    /// A change counter for anything that can affect how images are
    /// drawn. It changes when images or placements are added, removed,
    /// or modified, and also when placements move on screen without
    /// changing, such as during scrolling, resizing, or erasing history.
    ///
    /// The counter is never reset. Instead, each consumer keeps the last
    /// value it saw and compares it to the current value. Because nothing
    /// clears the counter, any number of consumers can watch the same
    /// storage independently.
    ///
    /// Only compare values for equality. The counter wraps around on
    /// overflow, so "greater than" has no meaning.
    ///
    /// The difference from `generation` is that `generation` only
    /// changes when the contents change. Every change to `generation`
    /// also changes `dirty`, so if `dirty` changed but `generation` did
    /// not, the images and placements are the same and only their
    /// positions on screen may have moved.
    ///
    /// Update this with `markDirty` or `markMutated` rather than writing
    /// it directly. There are two exceptions. `Terminal.switchScreen`
    /// sets the new screen's counter past the old screen's so that a
    /// consumer can never see the same value on both screens, and
    /// `Screen.reset` carries the counter over to the new storage.
    pub dirty: u64,

    /// Generation stamp of the last content mutation to this storage:
    /// any image transmit/replace, placement add, or delete of either.
    /// Zero means the storage has never been mutated (and is therefore
    /// empty).
    ///
    /// This field must only be written via markMutated.
    pub generation: u64,

    /// The storage generation at the end of the last animationTick
    /// that found nothing to play. Null if the last tick found
    /// something to play, or if no tick has run yet.
    pub animation_idle_generation: Option<u64>,

    /// This is the next automatically assigned image ID for images
    /// transmitted without an ID or number. We start mid-way through
    /// the u32 range to stay clear of the low IDs client programs
    /// typically pick. See nextImageId.
    pub next_image_id: u32,

    /// This is the next automatically assigned placement ID. This is never
    /// user-facing so we can start at 0. This is 32-bits because we use
    /// the same space for external placement IDs. We can start at zero
    /// because any number is valid.
    pub next_internal_placement_id: u32,

    // TODO(port): `images`, `placements` and `loading` are pending the
    // graphics_image port.
    /// The limits of what medium types are allowed for image loading.
    pub image_limits: LoadingImageLimits,

    /// The total bytes of image data that have been loaded and the limit.
    /// If the limit is reached, the oldest images will be evicted to make
    /// space. Unused images take priority.
    pub total_bytes: usize,
    pub total_limit: usize,
}

impl Default for ImageStorage {
    fn default() -> Self {
        ImageStorage {
            dirty: 0,
            generation: 0,
            animation_idle_generation: None,
            next_image_id: 2147483647,
            next_internal_placement_id: 0,
            image_limits: LoadingImageLimits::DIRECT,
            total_bytes: 0,
            total_limit: 320 * 1000 * 1000, // 320MB
        }
    }
}

impl ImageStorage {
    pub fn deinit(&mut self, pages: &mut PageList) {
        self.clear_placements(pages);
    }

    /// Kitty image protocol is enabled if we have a non-zero limit.
    pub fn enabled(&self) -> bool {
        self.total_limit != 0
    }

    /// Record a content mutation: marks the storage dirty and assigns a
    /// fresh generation stamp. Must be called by anything that changes
    /// the set of images or placements (or image contents).
    ///
    /// Do not call this when placements only move on screen (scrolling,
    /// resizing, screen switches). Use `markDirty` for those.
    pub fn mark_mutated(&mut self) {
        self.mark_dirty();
        self.generation = next_generation();
    }

    /// Record that placements may have moved on screen even though the
    /// images and placements themselves did not change. This advances
    /// `dirty` and leaves `generation` alone.
    pub fn mark_dirty(&mut self) {
        self.dirty = self.dirty.wrapping_add(1);
    }

    /// Sets the limit in bytes for the total amount of image data that
    /// can be loaded. If this limit is lower, this will do an eviction
    /// if necessary. If the value is zero, then Kitty image protocol will
    /// be disabled.
    pub fn set_limit(&mut self, pages: &mut PageList, limit: usize) {
        // Special case disabling by quickly deleting all
        if limit == 0 {
            let image_limits = self.image_limits.clone();
            self.deinit(pages);
            *self = ImageStorage {
                image_limits,
                ..Default::default()
            };
            self.mark_mutated();
        }

        // If we re lowering our limit, check if we need to evict.
        if limit < self.total_bytes {
            let req_bytes = self.total_bytes - limit;
            log::info!(target: LOG_TARGET, "evicting images to lower limit, evicting={req_bytes}");
            // TODO(port): evictImage is pending; no images are ever stored
            // yet, so total_bytes is always zero and this is unreachable.
            log::warn!(target: LOG_TARGET, "failed to evict enough images for required bytes");
        }

        self.total_limit = limit;
    }

    /// Remove pin-backed placements anchored in the history rows that
    /// `Screen.eraseHistory` is about to erase, from the top of history
    /// through `bl` (or all of history if null), along with any relative
    /// placements orphaned by that. This must be called before the rows
    /// are erased: afterward, the tracked pins have been moved to the
    /// top-left of the retained rows and are indistinguishable from
    /// placements that were already there.
    pub fn erase_history(&mut self, pages: &PageList, bl: Option<point::Point>) {
        let history_rows = pages.total_rows - pages.rows as usize;
        if history_rows == 0 {
            return;
        }
        let _limit: usize = match bl {
            None => history_rows - 1,
            Some(pt) => {
                let Some(bl_pin) = pages.pin(pt) else { return };
                let Some(bl_pt) = pages.point_from_pin(point::Tag::History, bl_pin) else {
                    return;
                };
                bl_pt.coord().y as usize
            }
        };

        // TODO(port): walk the placements map (pending the graphics_image
        // port) and remove pin placements at or above `limit`, then
        // orphans, then markMutated. There are no placements yet.
    }

    fn clear_placements(&mut self, _pages: &mut PageList) {
        // TODO(port): deinit (untrack the pins of) every placement once the
        // placements map is ported. There are no placements yet.
    }
}
