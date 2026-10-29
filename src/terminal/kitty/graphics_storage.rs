//! Port of `src/terminal/kitty/graphics_storage.zig`.
//!
//! STATUS: ported
//!
//! Notes on the port:
//! - Functions that take a `*Screen` in Zig only touch `screen.pages`, so
//!   they take `&mut PageList`/`&PageList` here (the storage is a field of
//!   the screen, so borrowing the whole screen is impossible in Rust).
//! - Functions that take a `*Terminal` in Zig take the active screen's
//!   `&mut PageList` plus a [`TerminalInfo`] holding the terminal values
//!   they read (geometry, scrolling region and the active screen's cursor),
//!   for the same reason. `Placement`'s geometry methods take only the
//!   `TerminalInfo`.
//! - `std.Io`/allocator parameters are dropped.
//! - The image and placement maps are `HashMap`s. Zig removes entries while
//!   iterating; the port collects the matching keys first and then removes
//!   them in the same order, which is equivalent because no removal changes
//!   whether a later entry matches.
//! - `imageById`/`imageByNumber` return `Option<&Image>` instead of an
//!   `Image` copy (Rust images own their pixels).
//! - `PendingImage::complete`/`complete_frame` return the data back in
//!   `Err` instead of returning `false` (the caller keeps ownership).
//! - `ScrollMargins::end` takes the page list (Zig keeps a `*Screen`).

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::terminal::kitty::graphics_animation as animation;
use crate::terminal::kitty::graphics_command as command;
use crate::terminal::kitty::graphics_image::{Data, Image, Limits as LoadingImageLimits, LoadingImage, Rect};
use crate::terminal::kitty::graphics_pixel as pixel;
use crate::terminal::page_list::{PageList, Pin, PinOverflow, PinPtr};
use crate::terminal::point;
use crate::terminal::size::{AllocError, CellCountInt};

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
/// from different threads. (Zig falls back to a mutex on targets without
/// 64-bit atomics; every Rust target we build supports `AtomicU64`.)
static GENERATION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns the next generation stamp. Stamps are unique and strictly
/// monotonically increasing process-wide, starting at 1 (0 is reserved
/// to mean "never stamped").
pub fn next_generation() -> u64 {
    GENERATION_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

/// `Terminal.ScrollingRegion`.
///
/// TODO(port-dedupe): replace with `crate::terminal::terminal::ScrollingRegion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

/// The terminal values the storage reads (Zig: the `*Terminal` parameter;
/// the active screen's page list is passed separately because the storage
/// lives inside the active screen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalInfo {
    /// `t.cols`, `t.rows`
    pub cols: CellCountInt,
    pub rows: CellCountInt,

    /// `t.width_px`, `t.height_px`
    pub width_px: u32,
    pub height_px: u32,

    /// `t.scrolling_region`
    pub scrolling_region: ScrollingRegion,

    /// `t.screens.active.cursor.{x,y}`
    pub cursor_x: CellCountInt,
    pub cursor_y: CellCountInt,
}

impl TerminalInfo {
    /// A terminal of the given size with a full-screen scrolling region,
    /// no pixel geometry and the cursor at the top-left.
    pub fn new(cols: CellCountInt, rows: CellCountInt) -> TerminalInfo {
        TerminalInfo {
            cols,
            rows,
            width_px: 0,
            height_px: 0,
            scrolling_region: ScrollingRegion {
                top: 0,
                bottom: rows - 1,
                left: 0,
                right: cols - 1,
            },
            cursor_x: 0,
            cursor_y: 0,
        }
    }
}

/// Mode for [`ImageStorage::next_image_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextImageIdMode {
    Explicit,
    Implicit,
}

pub type ImageMap = HashMap<u32, Image>;
pub type PlacementMap = HashMap<PlacementKey, Placement>;

/// An image storage is associated with a terminal screen (i.e. main
/// screen, alt screen) and contains all the transmitted images and
/// placements.
#[derive(Debug)]
pub struct ImageStorage {
    /// A change counter for anything that can affect how images are
    /// drawn. It changes when images or placements are added, removed,
    /// or modified, and also when placements move on screen without
    /// changing, such as during scrolling, resizing, or erasing history.
    ///
    /// The counter is never reset. Instead, each consumer keeps the last
    /// value it saw and compares it to the current value. Because nothing
    /// clears the counter, any number of consumers can watch the same
    /// storage independently. Within Ghostty, only `RenderState` reads
    /// it, to know when to position its placements again. A renderer
    /// should watch `RenderState.imagePlacementsGeneration` instead. For
    /// example, a consumer of the storage would do:
    ///
    /// ```ignore
    /// if storage.dirty != self.last_dirty {
    ///     self.last_dirty = storage.dirty;
    ///     self.rebuild_placements(storage);
    /// }
    /// ```
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
    /// Unlike dirty, this is NOT updated by scrolling/resizing, so an
    /// unchanged generation means the placement set and all image data
    /// are identical; only placement geometry (pins) may have moved.
    /// Values come from a process-global monotonic counter, so a value
    /// observed from any storage never recurs for different content,
    /// even across screen switches or storage resets.
    ///
    /// This field must only be written via markMutated.
    pub generation: u64,

    /// The storage generation at the end of the last animationTick
    /// that found nothing to play. Null if the last tick found
    /// something to play, or if no tick has run yet.
    ///
    /// This lets animationTick skip walking every image when nothing
    /// changed since it last found nothing to play. That shortcut is
    /// safe for two reasons. First, every change that can start an
    /// animation updates the generation through markMutated. That
    /// includes animation commands, new frames, new placements,
    /// replaced images and finished image loads. Second, none of the
    /// reasons an image doesn't animate go away just because time
    /// passes.
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

    /// The set of images that are currently known.
    pub images: ImageMap,

    /// The set of placements for loaded images.
    pub placements: PlacementMap,

    /// The chunked image or animation frame load that is in progress,
    /// or null if there isn't one.
    ///
    /// A load starts when a direct (t=d) transmission arrives with m=1
    /// and ends when its final chunk (m=0) arrives. It is abandoned early
    /// by a delete command or by a transmission that uses another medium.
    pub loading: Option<Box<LoadingImage>>,

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
            images: HashMap::new(),
            placements: HashMap::new(),
            loading: None,
            image_limits: LoadingImageLimits::DIRECT,
            total_bytes: 0,
            total_limit: 320 * 1000 * 1000, // 320MB
        }
    }
}

/// A ticket for attaching pixels to an image that was added before its
/// pixels arrived. `addPendingImage` returns one. (Zig:
/// `ImageStorage.PendingImage`.)
///
/// A pending image takes part in storage like any other image. It can
/// be placed, queried and evicted, and its full size is reserved
/// against the storage limit. It just has nothing to draw yet. When
/// the pixels arrive, `complete` attaches them, and `completeFrame`
/// does the same for the image's pending animation frames.
///
/// The ticket only works for the exact image it was issued for. If
/// the image was deleted, evicted, or replaced by a new transmission
/// with the same ID in the meantime, completing it does nothing and
/// returns false. That way, pixels that arrive late can never end up
/// on the wrong image.
///
/// ```ignore
/// let ticket = storage.add_pending_image(pages, Image {
///     id: 1,
///     width: 2,
///     height: 2,
///     format: Format::Rgba,
///     data: Data::Pending(16),
///     ..Default::default()
/// })?;
///
/// // ... later, once the 16 bytes of pixels have arrived ...
///
/// if let Err(pixels) = ticket.complete(&mut storage, pixels) {
///     // The image is gone or was replaced, so the pixels are still ours.
///     drop(pixels);
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingImage {
    /// The ID of the pending image.
    pub id: u32,

    /// The `Image.transmission` of the pending image, which tells it
    /// apart from any later image with the same ID.
    pub transmission: u64,
}

impl PendingImage {
    /// Attach the image's decoded pixels.
    ///
    /// Returns Ok if the image is still the one this ticket was
    /// issued for, its pixels are still pending, and `data` has
    /// exactly the expected length. The storage then owns `data`.
    /// Otherwise this changes nothing and returns `data` back in `Err`.
    ///
    /// Completing an image keeps its `generation`, so the image keeps
    /// its age for eviction purposes.
    pub fn complete(self, storage: &mut ImageStorage, data: Vec<u8>) -> Result<(), Vec<u8>> {
        let Some(img) = self.image(storage) else {
            return Err(data);
        };
        let expected_len = match img.data {
            Data::Complete(_) => return Err(data),
            Data::Pending(len) => len,
        };
        if data.len() != expected_len {
            return Err(data);
        }

        img.data = Data::Complete(data);
        storage.mark_mutated();
        Ok(())
    }

    /// Attach the RGBA pixels of one pending animation frame.
    ///
    /// `number` is the frame number as the Kitty graphics protocol
    /// counts frames, the same numbering `Image.frameData` uses.
    /// Frame 1 is the image itself, so animation frames start at 2.
    /// Use `complete` for frame 1.
    ///
    /// This follows the same rules as `complete`: it succeeds and
    /// takes ownership of `data` only if the image is still the one
    /// this ticket was issued for, the frame is still pending, and
    /// `data` has exactly the frame's length. The storage keeps
    /// `data` mutable because later frame edits draw into it.
    ///
    /// If the completed frame is the one on screen, the image gets a
    /// new `generation` so that renderers pick up the new pixels.
    pub fn complete_frame(
        self,
        storage: &mut ImageStorage,
        number: u32,
        data: Vec<u8>,
    ) -> Result<(), Vec<u8>> {
        if number < 2 {
            return Err(data);
        }
        let Some(img) = self.image(storage) else {
            return Err(data);
        };
        let Some(anim) = img.animation.as_mut() else {
            return Err(data);
        };
        let index = (number - 2) as usize;
        if index >= anim.frames.len() {
            return Err(data);
        }
        let frame = &mut anim.frames[index];
        if !frame.is_pending() {
            return Err(data);
        }
        if data.len() != frame.pending_len {
            return Err(data);
        }

        *frame = animation::Frame {
            data,
            gap_ms: frame.gap_ms,
            pending_len: 0,
        };
        let current = anim.current_index == number - 1;
        if current {
            storage.mark_mutated();
            let generation = storage.generation;
            storage.images.get_mut(&self.id).unwrap().generation = generation;
        } else {
            storage.mark_mutated();
        }
        Ok(())
    }

    /// The image this ticket was issued for, or null if it is no
    /// longer in storage.
    fn image(self, storage: &mut ImageStorage) -> Option<&mut Image> {
        let img = storage.images.get_mut(&self.id)?;
        if img.transmission != self.transmission {
            return None;
        }
        Some(img)
    }
}

/// How the row operation behind a scrollMarginsBegin/end pair moves
/// tracked pins, which determines how much repositioning work the
/// placements need. (Zig: `ImageStorage.ScrollOp`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollOp {
    /// The operation shifts the entire active window down (it
    /// creates scrollback), so every placement anchored in the
    /// active area changes active position and must be restored.
    WindowShift,

    /// The operation moves rows in place within the scrolling
    /// region: pins anchored outside the region rows keep their
    /// active position without help, so only placements anchored
    /// inside the region rows need handling. This is the common
    /// case (e.g. a status bar image beside a scrolling pane
    /// costs almost nothing per scroll).
    InPlace,
}

/// Maximum number of parent links in a relative placement chain,
/// matching the minimum specified in the spec and the actual
/// limit defined by Kitty at the time of authoring.
pub const PARENT_CHAIN_LIMIT: usize = 8;

/// `ImageStorage.ParentError`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentError {
    /// The parent image (P=) does not exist.
    ParentImageNotFound,
    /// The parent image exists but the requested placement (Q=, or
    /// any placement when Q is omitted) does not.
    ParentPlacementNotFound,
    /// The placement refers to itself as its own parent.
    SelfParent,
    /// The parent chain loops back to the placement being created.
    Cycle,
    /// The parent chain exceeds parent_chain_limit links.
    TooDeep,
    /// An ancestor in the chain no longer exists. This should not
    /// happen since removeOrphans keeps chains intact, but we
    /// check anyway rather than trusting the invariant.
    AncestorNotFound,
}

impl fmt::Display for ParentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ParentError {}

/// The result of resolving a relative placement's parent chain:
/// the chain's root placement (always pin or virtual, never
/// relative) and the total cell offset from the root's origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedChain {
    pub root_key: PlacementKey,
    pub root: Placement,
    pub horizontal_offset: i32,
    pub vertical_offset: i32,
}

/// The result of [`ImageStorage::placeholder_target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceholderTarget {
    pub key: PlacementKey,
    pub placement: Placement,
}

impl ImageStorage {
    pub fn deinit(&mut self, pages: &mut PageList) {
        self.loading = None;

        self.clear_placements(pages);
        self.placements = HashMap::new();
        self.images = HashMap::new();
    }

    /// Kitty image protocol is enabled if we have a non-zero limit.
    pub fn enabled(&self) -> bool {
        self.total_limit != 0
    }

    /// Record a content mutation: marks the storage dirty and assigns a
    /// fresh generation stamp. Must be called by anything that changes
    /// the set of images or placements (or image contents), including
    /// the animation command handlers in graphics_exec.zig.
    ///
    /// Do not call this when placements only move on screen (scrolling,
    /// resizing, screen switches). Use `markDirty` for those. Changing
    /// the generation there would break the promise that an unchanged
    /// generation means unchanged contents.
    pub fn mark_mutated(&mut self) {
        self.mark_dirty();
        self.generation = next_generation();
    }

    /// Record that placements may have moved on screen even though the
    /// images and placements themselves did not change. This advances
    /// `dirty` and leaves `generation` alone.
    ///
    /// Call this after anything that moves or removes the screen rows
    /// placements are attached to, such as scrolling, resizing, erasing
    /// history, or lowering the scrollback limit. If the images or
    /// placements themselves changed, call `markMutated` instead.
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
            if !self.evict_image(pages, req_bytes) {
                log::warn!(target: LOG_TARGET, "failed to evict enough images for required bytes");
            }
        }

        self.total_limit = limit;
    }

    /// Returns the next ID to automatically assign to an image that
    /// was transmitted without an explicit ID (i=). The result is
    /// never zero (zero means "no ID") and never an ID currently in
    /// use, so an automatic ID never replaces an existing image.
    pub fn next_image_id(&mut self, mode: NextImageIdMode) -> u32 {
        // Starting ID depends on mode.
        let mut id: u32 = match mode {
            // Yeah, starting from 1 is wasteful. This matches what Kitty
            // does. In the future we can probably cache a low-water-mark
            // based on delete behavior.
            NextImageIdMode::Explicit => 1,
            NextImageIdMode::Implicit => self.next_image_id,
        };

        // Go through all possible images. We have +2 because we can
        // exceed the total by 1 (new ID).
        let count = self.images.len();
        for _ in 0..count + 2 {
            if id != 0 && !self.images.contains_key(&id) {
                break;
            }
            id = id.wrapping_add(1);
        }

        // Zero is never allowed.
        if id == 0 {
            id = 1;
        }

        // Keep track of our next image id for implicit to speed that up.
        if mode == NextImageIdMode::Implicit {
            self.next_image_id = id.wrapping_add(1);
        }

        id
    }

    /// Add an image to the storage. This will automatically free any existing
    /// image with the same ID. Prefer addPendingImage for pending data so the
    /// caller receives a completion token.
    pub fn add_image(&mut self, pages: &mut PageList, img: Image) -> Result<(), AllocError> {
        let new_len = img.data.len();

        // If the image itself is over the limit, then error immediately
        if new_len > self.total_limit {
            return Err(AllocError::OutOfMemory);
        }

        // Credit an existing image's reservation before calculating the
        // replacement size. In particular, a completed live transmission
        // replacing pending snapshot metadata must be able to reuse the
        // reservation without evicting its own ID.
        let old_len = self.images.get(&img.id).map_or(0, |old| old.storage_size());
        debug_assert!(old_len <= self.total_bytes);

        // Reserve map capacity before evicting anything so allocation failure
        // cannot leave storage partially evicted. Existing keys need no room.
        if !self.images.contains_key(&img.id) {
            self.images
                .try_reserve(1)
                .map_err(|_| AllocError::OutOfMemory)?;
        }

        // If this would put us over the limit, then evict.
        let total_bytes = self.total_bytes - old_len + new_len;
        if total_bytes > self.total_limit {
            let req_bytes = total_bytes - self.total_limit;
            log::info!(target: LOG_TARGET, "evicting images to make space for {req_bytes} bytes");
            if !self.evict_image_except(pages, req_bytes, Some(img.id)) {
                log::warn!(target: LOG_TARGET, "failed to evict enough images for required bytes");
                return Err(AllocError::OutOfMemory);
            }
        }

        log::debug!(target: LOG_TARGET, "addImage image={:?}", img.without_data());

        if let Some(existing) = self.images.get(&img.id) {
            let existing_size = existing.storage_size();

            // Retransmitting a specific image ID replaces the old image and all
            // of its placements, as required by the Kitty graphics protocol.
            self.remove_placements_by_image_id(pages, img.id);

            // Relative placements parented to the removed placements go too.
            let _ = self.remove_orphans(pages, false);

            // Replacing an image drops its animation frames with it,
            // implementing the protocol rule that retransmitting the
            // base image resets the animation.
            self.total_bytes -= existing_size;
            self.images.remove(&img.id);
        }

        let id = img.id;
        let mut img = img;
        img.metadata.placement_count = 0;
        self.images.insert(id, img);
        self.total_bytes += new_len;

        // Stamp the stored image with a fresh generation. This gives
        // every add/replace a unique stamp even when the same image ID
        // is retransmitted with identical dimensions, so consumers
        // (e.g. renderer texture caches) can detect content changes.
        // The transmission starts out with the same stamp and keeps it
        // for the life of this image.
        self.mark_mutated();
        let generation = self.generation;
        let stored = self.images.get_mut(&id).unwrap();
        stored.generation = generation;
        stored.transmission = generation;
        Ok(())
    }

    /// Add an image whose pixels have not arrived yet. `img.data` must be
    /// `.pending` with the length of the decoded pixels. Returns the
    /// ticket to attach those pixels later. See `PendingImage`.
    pub fn add_pending_image(
        &mut self,
        pages: &mut PageList,
        img: Image,
    ) -> Result<PendingImage, AllocError> {
        debug_assert!(img.data.is_pending());
        let id = img.id;
        self.add_image(pages, img)?;
        let stored = self.images.get(&id).unwrap();
        Ok(PendingImage {
            id: stored.id,
            transmission: stored.transmission,
        })
    }

    /// Add a placement for a given image. The caller must verify in advance
    /// the image exists to prevent memory corruption. On success, storage
    /// owns `p`; on error, the caller retains ownership. The screen must own
    /// any tracked pin in `p` and in the placement it replaces.
    pub fn add_placement(
        &mut self,
        pages: &mut PageList,
        image_id: u32,
        placement_id: u32,
        p: Placement,
    ) -> Result<(), AllocError> {
        debug_assert!(self.images.contains_key(&image_id));
        log::debug!(
            target: LOG_TARGET,
            "placement image_id={image_id} placement_id={placement_id} placement={p:?}"
        );

        // When we add a placement, take this opportunity to reap garbage.
        // Garbage are placements that disappeared off scrollback (were
        // pruned). We don't clear Kitty state in the hot path so we do
        // this opportunistically here.
        self.reap_garbage_placements(pages);

        // The important piece here is that the placement ID needs to
        // be marked internal if it is zero. This allows multiple placements
        // to be added for the same image. If it is non-zero, then it is
        // an external placement ID and we can only have one placement
        // per (image id, placement id) pair.
        let key = PlacementKey {
            image_id,
            placement_id: if placement_id == 0 {
                let id = self.next_internal_placement_id;
                self.next_internal_placement_id = self.next_internal_placement_id.wrapping_add(1);
                PlacementId {
                    tag: PlacementTag::Internal,
                    id,
                }
            } else {
                PlacementId {
                    tag: PlacementTag::External,
                    id: placement_id,
                }
            },
        };

        self.placements
            .try_reserve(1)
            .map_err(|_| AllocError::OutOfMemory)?;
        if let Some(existing) = self.placements.get(&key) {
            existing.deinit(pages);
        } else {
            let img = self.images.get_mut(&image_id).unwrap();
            if img.metadata.placement_count >= crate::terminal::kitty::graphics_image::Metadata::MAX_PLACEMENT_COUNT {
                return Err(AllocError::OutOfMemory);
            }
            img.metadata.placement_count += 1;
        }
        self.placements.insert(key, p);

        // This always mutates
        self.mark_mutated();
        Ok(())
    }

    /// Adjust placements for a scroll of the terminal's scrolling region
    /// by `delta` rows (negative moves content up). This implements the
    /// Kitty graphics protocol behavior for scrolls bounded by margins:
    /// placements entirely inside the scrolling region move with the content
    /// and are clipped when the move would extend them past a margin (deleted
    /// when fully clipped), while every other placement (straddling a margin,
    /// outside the region, or in the scrollback) must not move at all.
    ///
    /// This is split into two phases around the row operation because
    /// the various scroll implementations move tracked pins in
    /// inconsistent ways (some shift the active area so every pin moves,
    /// some rotate rows in place and adjust only pins inside the
    /// region). We record the desired final position of every placement
    /// up front in the returned state, whose end() repositions the pins
    /// once the rows have settled, making the result independent of the
    /// path taken. Every call must be paired with exactly one end().
    ///
    /// Callers must skip this entirely when the scrolling region is the
    /// full screen: a full-screen scroll moves placements with their
    /// anchored rows (possibly into the scrollback) via pin tracking,
    /// which already matches kitty's marginless scroll behavior.
    ///
    /// `pages` must be the page list of the screen that owns this storage
    /// (Zig asserts `self == &t.screens.active.kitty_images`).
    pub fn scroll_margins_begin(
        &mut self,
        pages: &mut PageList,
        t: &TerminalInfo,
        delta: isize,
        op: ScrollOp,
    ) -> ScrollMargins {
        let mut result = ScrollMargins::default();

        let top: i64 = t.scrolling_region.top as i64;
        let bottom: i64 = t.scrolling_region.bottom as i64;

        let mut mutated = false;
        let keys: Vec<PlacementKey> = self.placements.keys().copied().collect();
        'placements: for key in keys {
            let mut p = self.placements[&key];

            // Virtual placements follow their placeholder cells and
            // relative placements follow their parents, so neither is
            // ever adjusted by scrolls, matching kitty.
            let pin_ptr = match p.location {
                Location::Pin(pin) => pin,
                Location::Virtual | Location::Relative(_) => continue,
            };
            // SAFETY: tracked pins of stored placements are owned by
            // `pages` and live until the placement is removed.
            let pin = unsafe { *pin_ptr.as_ptr() };

            // Pruned placements are reaped by reapGarbagePlacements.
            if pin.garbage {
                continue;
            }

            // Placements anchored outside the active area (scrollback)
            // are never touched by a scroll within the margins.
            let Some(pt) = pages.point_from_pin(point::Tag::Active, pin) else {
                continue;
            };
            let coord = pt.coord();
            let y: i64 = coord.y as i64;

            // An in-place operation doesn't touch pins anchored outside
            // the region rows, and a placement anchored there can never
            // be entirely inside the region, so it needs no work at all.
            if op == ScrollOp::InPlace && (y < top || y > bottom) {
                continue;
            }

            // The final position defaults to unmoved; restoring the pin
            // to this exact spot afterwards is what stops the row
            // operations from dragging the placement around.
            let mut final_y: i64 = y;

            'inside: {
                let Some(image) = self.images.get(&key.image_id) else {
                    break 'inside;
                };
                let grid = p.grid_size(image, t);

                // Unknown geometry (e.g. pixel sizes not yet reported)
                // means we can't know the extent, so we can't consider
                // the placement contained.
                if grid.rows == 0 || grid.cols == 0 {
                    break 'inside;
                }

                // Only placements entirely within the scrolling region
                // move. Kitty only has top/bottom margins so it only
                // checks rows; the column check is our generalization
                // for left/right margins and is trivially true for
                // full-width regions.
                if y < top || y + grid.rows as i64 - 1 > bottom {
                    break 'inside;
                }
                let x: i64 = coord.x as i64;
                if x < t.scrolling_region.left as i64
                    || (x + grid.cols as i64 - 1).min(t.cols as i64 - 1)
                        > t.scrolling_region.right as i64
                {
                    break 'inside;
                }

                let new_y: i64 = y + delta as i64;
                let rows: i64 = grid.rows as i64;
                let top_clip: i64 = 0.max(top - new_y);
                let bottom_clip: i64 = 0.max(new_y + rows - 1 - bottom);
                let visible: bool = 'clip: {
                    if top_clip >= rows || bottom_clip >= rows {
                        break 'clip false;
                    }

                    if top_clip > 0 {
                        if !p.clip_top(image, t, top_clip as u32, grid.rows) {
                            break 'clip false;
                        }
                        mutated = true;
                        final_y = top;
                        break 'clip true;
                    }

                    if bottom_clip > 0 {
                        if !p.clip_bottom(image, t, bottom_clip as u32, grid.rows) {
                            break 'clip false;
                        }
                        mutated = true;
                    }

                    final_y = new_y;
                    true
                };

                // Scrolled or clipped entirely out of the region: the
                // placement is deleted, like kitty. The image itself is
                // retained for future placements.
                if !visible {
                    self.remove_placement(pages, key);
                    mutated = true;
                    continue 'placements;
                }

                // Store the clipped source rectangle.
                self.placements.insert(key, p);
            }

            if result.restores.try_reserve(1).is_err() {
                // OOM: this placement is left to the whims of the
                // generic pin tracking. Degraded, but safe.
                log::warn!(target: LOG_TARGET, "OOM adjusting image placement for scroll");
                continue;
            }
            result.restores.push(Restore {
                pin: pin_ptr,
                x: pin.x,
                y: final_y as u32,
            });
        }

        if mutated {
            // Placements deleted by clipping may orphan relative placements.
            // Orphans have no pins so this never touches the restores.
            let _ = self.remove_orphans(pages, false);
            self.mark_mutated();
        }

        result
    }

    /// Reap pin-backed placements whose tracked content has been pruned
    /// from history, along with any relative placements orphaned by
    /// that, marking the content mutation.
    ///
    /// Virtual and relative placements have no tracked screen location and
    /// are never pruned themselves. Kitty removes placements once they
    /// scroll out of retained history.
    fn reap_garbage_placements(&mut self, pages: &mut PageList) {
        let keys: Vec<PlacementKey> = self
            .placements
            .iter()
            .filter(|(_, p)| match p.location {
                // SAFETY: tracked pin owned by `pages` (see Placement).
                Location::Pin(pin) => unsafe { pin.as_ref().garbage },
                Location::Virtual | Location::Relative(_) => false,
            })
            .map(|(k, _)| *k)
            .collect();
        if keys.is_empty() {
            return;
        }
        for key in keys {
            self.remove_placement(pages, key);
        }

        // If we removed a placement, then also remove any orphan
        // children if this was a parent.
        let _ = self.remove_orphans(pages, false);

        self.mark_mutated();
    }

    /// Remove pin-backed placements anchored in the history rows that
    /// `Screen.eraseHistory` is about to erase, from the top of history
    /// through `bl` (or all of history if null), along with any relative
    /// placements orphaned by that. This must be called before the rows
    /// are erased: afterward, the tracked pins have been moved to the
    /// top-left of the retained rows and are indistinguishable from
    /// placements that were already there.
    pub fn erase_history(&mut self, pages: &mut PageList, bl: Option<point::Point>) {
        let history_rows = pages.total_rows - pages.rows as usize;
        if history_rows == 0 {
            return;
        }
        let limit: usize = match bl {
            None => history_rows - 1,
            Some(pt) => {
                let Some(bl_pin) = pages.pin(pt) else { return };
                let Some(bl_pt) = pages.point_from_pin(point::Tag::History, bl_pin) else {
                    return;
                };
                bl_pt.coord().y as usize
            }
        };

        let keys: Vec<PlacementKey> = self
            .placements
            .iter()
            .filter(|(_, p)| {
                let pin = match p.location {
                    Location::Pin(pin) => pin,
                    Location::Virtual | Location::Relative(_) => return false,
                };
                // SAFETY: tracked pin owned by `pages`.
                let Some(pt) = pages.point_from_pin(point::Tag::History, unsafe { *pin.as_ptr() })
                else {
                    return false;
                };
                pt.coord().y as usize <= limit
            })
            .map(|(k, _)| *k)
            .collect();
        if keys.is_empty() {
            return;
        }
        for key in keys {
            self.remove_placement(pages, key);
        }

        let _ = self.remove_orphans(pages, false);
        self.mark_mutated();
    }

    fn clear_placements(&mut self, pages: &mut PageList) {
        for p in self.placements.values() {
            p.deinit(pages);
        }
        self.placements.clear();
    }

    fn remove_placements_by_image_id(&mut self, pages: &mut PageList, image_id: u32) {
        let keys: Vec<PlacementKey> = self
            .placements
            .keys()
            .filter(|k| k.image_id == image_id)
            .copied()
            .collect();
        for key in keys {
            self.remove_placement(pages, key);
        }
    }

    /// Remove a placement by its key, releasing any tracked pin
    /// and keeping the image's placement count in sync. The key must
    /// be in the placements map.
    fn remove_placement(&mut self, pages: &mut PageList, key: PlacementKey) {
        let p = self.placements.remove(&key).unwrap();
        p.deinit(pages);
        let img = self.images.get_mut(&key.image_id).unwrap();
        debug_assert!(img.metadata.placement_count > 0);
        img.metadata.placement_count -= 1;
    }

    /// Remove relative placements whose parent placement no longer
    /// exists, keeping a relative placement's lifetime tied to its
    /// parent chain. Chains can be several links deep, so this loops
    /// until a pass removes nothing to take out entire orphaned
    /// subtrees. Returns true if anything was removed.
    ///
    /// If `delete_unused` is set, an image left without placements
    /// by the reap is freed. This matches uppercase delete
    /// semantics; every other removal path retains image data and
    /// passes false. (Zig passes an optional allocator.)
    ///
    /// This must be called, outside of any placements iteration, after
    /// every operation that removes placements. Kitty gets the same
    /// effect lazily by dropping unresolvable placements while drawing;
    /// we do it eagerly because our renderer never mutates terminal
    /// state.
    fn remove_orphans(&mut self, pages: &mut PageList, delete_unused: bool) -> bool {
        let mut removed_any = false;
        let mut removed = true;
        while removed {
            removed = false;
            let keys: Vec<PlacementKey> = self
                .placements
                .iter()
                .filter_map(|(k, p)| match p.location {
                    Location::Relative(rel) => Some((*k, rel.parent)),
                    Location::Pin(_) | Location::Virtual => None,
                })
                .filter(|(_, parent)| !self.placements.contains_key(parent))
                .map(|(k, _)| k)
                .collect();
            for key in keys {
                // Parent is gone, remove this placement.
                let image_id = key.image_id;
                self.remove_placement(pages, key);
                removed = true;
                removed_any = true;

                if delete_unused {
                    self.delete_if_unused(image_id);
                }
            }
        }

        removed_any
    }

    /// Resolve and validate the parent reference (P=/Q=) of a relative
    /// placement, returning the concrete key of the parent placement.
    /// `child` is the key of the placement being created when it is
    /// addressable (an explicit placement ID); null for placements that
    /// will receive a fresh internal ID, which nothing can refer to yet.
    ///
    /// Checks: the parent must exist, the placement must not
    /// parent itself, and the resulting ancestor chain must be acyclic
    /// and within parent_chain_limit.
    pub fn resolve_parent(
        &mut self,
        pages: &mut PageList,
        child: Option<PlacementKey>,
        parent_image_id: u32,
        parent_placement_id: u32,
    ) -> Result<PlacementKey, ParentError> {
        // Reap placements whose content has been pruned from history
        // before resolving: a pruned parent must be reported as missing,
        // not accepted and then immediately reaped by addPlacement's own
        // sweep, which would store an orphan.
        self.reap_garbage_placements(pages);

        // If the parent doesn't exist we're already failed.
        if !self.images.contains_key(&parent_image_id) {
            return Err(ParentError::ParentImageNotFound);
        }

        // Find the parent
        let parent: PlacementKey = 'parent: {
            // An explicit parent placement ID (Q=) selects exactly that
            // placement.
            if parent_placement_id > 0 {
                let key = PlacementKey {
                    image_id: parent_image_id,
                    placement_id: PlacementId {
                        tag: PlacementTag::External,
                        id: parent_placement_id,
                    },
                };
                if !self.placements.contains_key(&key) {
                    return Err(ParentError::ParentPlacementNotFound);
                }
                break 'parent key;
            }

            // No Q: pick a placement of the parent image. Kitty picks
            // the oldest surviving placement; our placement map doesn't
            // track creation order so we pick by PlacementId.preferredOver
            // instead. This is unspecified so any behavior here is fine.
            // In practice the parent image has a single placement, and
            // clients use Q when it doesn't.
            let mut best: Option<PlacementId> = None;
            for key in self.placements.keys() {
                if key.image_id != parent_image_id {
                    continue;
                }
                let id = key.placement_id;
                match best {
                    None => best = Some(id),
                    Some(b) => {
                        if id.preferred_over(b) {
                            best = Some(id);
                        }
                    }
                }
            }
            let Some(best) = best else {
                return Err(ParentError::ParentPlacementNotFound);
            };

            PlacementKey {
                image_id: parent_image_id,
                placement_id: best,
            }
        };

        // A placement cannot be its own parent. This must be checked
        // before the chain walk so it reports EINVAL, not ECYCLE.
        if let Some(c) = child {
            if parent.eql(c) {
                return Err(ParentError::SelfParent);
            }
        }

        // Walk the would-be ancestor chain. `depth` counts parent links,
        // starting at one for the link we are about to create.
        let mut depth: usize = 1;
        let mut key = parent;
        loop {
            if let Some(c) = child {
                if key.eql(c) {
                    return Err(ParentError::Cycle);
                }
            }
            let Some(p) = self.placements.get(&key) else {
                return Err(ParentError::AncestorNotFound);
            };
            let rel = match p.location {
                Location::Relative(rel) => rel,
                // A pin or virtual placement is the chain root.
                Location::Pin(_) | Location::Virtual => break,
            };
            if depth >= PARENT_CHAIN_LIMIT {
                return Err(ParentError::TooDeep);
            }
            depth += 1;
            key = rel.parent;
        }

        Ok(parent)
    }

    /// Resolve a relative placement's parent chain to its root,
    /// accumulating the cell offsets (H=/V=) of every link on the way.
    /// The placement's position is the root's origin plus the accumulated
    /// offsets.
    ///
    /// Returns null when the chain is broken or too deep. Neither can
    /// normally happen for stored placements (resolveParent validates
    /// chains and removeOrphans removes broken ones), with one kitty
    /// quirk: replacing an ancestor placement can deepen the chains
    /// below it beyond parent_chain_limit after the fact.
    pub fn resolve_chain(&self, rel: Relative) -> Option<ResolvedChain> {
        let mut horizontal = rel.horizontal_offset;
        let mut vertical = rel.vertical_offset;
        let mut key = rel.parent;
        let mut depth: usize = 1;
        loop {
            let p = self.placements.get(&key)?;
            match p.location {
                Location::Relative(parent_rel) => {
                    if depth >= PARENT_CHAIN_LIMIT {
                        return None;
                    }
                    depth += 1;
                    horizontal = horizontal.saturating_add(parent_rel.horizontal_offset);
                    vertical = vertical.saturating_add(parent_rel.vertical_offset);
                    key = parent_rel.parent;
                }

                Location::Pin(_) | Location::Virtual => {
                    return Some(ResolvedChain {
                        root_key: key,
                        root: *p,
                        horizontal_offset: horizontal,
                        vertical_offset: vertical,
                    })
                }
            }
        }
    }

    /// Returns the placement that a unicode placeholder cell referencing
    /// this image/placement ID pair targets, or null if there is none. A
    /// zero placement ID targets one of the image's virtual placements,
    /// chosen by PlacementId.preferredOver so the choice is stable (an
    /// image rarely has more than one). This is the shared lookup used
    /// both for sizing placeholder runs and for positioning relative
    /// placements whose chain roots at a virtual placement.
    pub fn placeholder_target(&self, image_id: u32, placement_id: u32) -> Option<PlaceholderTarget> {
        if placement_id > 0 {
            let key = PlacementKey {
                image_id,
                placement_id: PlacementId {
                    tag: PlacementTag::External,
                    id: placement_id,
                },
            };
            let p = self.placements.get(&key)?;
            return Some(PlaceholderTarget { key, placement: *p });
        }

        let mut best: Option<PlacementKey> = None;
        for (key, p) in &self.placements {
            if key.image_id != image_id {
                continue;
            }
            if p.location != Location::Virtual {
                continue;
            }
            match best {
                None => best = Some(*key),
                Some(b) => {
                    if key.placement_id.preferred_over(b.placement_id) {
                        best = Some(*key);
                    }
                }
            }
        }

        let key = best?;
        Some(PlaceholderTarget {
            key,
            placement: self.placements[&key],
        })
    }

    /// Get an image by its ID. If the image doesn't exist, null is returned.
    pub fn image_by_id(&self, image_id: u32) -> Option<&Image> {
        self.images.get(&image_id)
    }

    /// Get an image by its number. If the image doesn't exist, return null.
    pub fn image_by_number(&self, image_number: u32) -> Option<&Image> {
        let mut newest: Option<&Image> = None;

        for img in self.images.values() {
            if img.number == image_number
                && (newest.is_none() || img.generation > newest.unwrap().generation)
            {
                newest = Some(img);
            }
        }

        newest
    }

    /// Get a mutable pointer to a stored image, by ID or (newest by)
    /// number, following the protocol's id/number addressing. Used by
    /// the animation commands, which mutate images in place. The
    /// pointer is invalidated by any operation that adds or removes
    /// images.
    pub fn image_ptr_by_id_or_number(
        &mut self,
        image_id: u32,
        image_number: u32,
    ) -> Option<&mut Image> {
        if image_id != 0 {
            return self.images.get_mut(&image_id);
        }

        let mut newest: Option<&mut Image> = None;
        for img in self.images.values_mut() {
            if img.number != image_number {
                continue;
            }
            if newest.is_none() || img.generation > newest.as_ref().unwrap().generation {
                newest = Some(img);
            }
        }

        newest
    }

    /// Record that the displayed content of an image changed without
    /// the image being re-added: marks the storage mutated and stamps
    /// the image with the fresh generation so consumers (e.g. the
    /// renderer's texture cache) replace what they hold. Used when an
    /// animation changes which frame is current or edits the current
    /// frame's pixels. (Zig takes an `*Image`; the Rust port takes the
    /// stored image's ID.)
    pub fn mark_image_content_changed(&mut self, image_id: u32) {
        self.mark_mutated();
        let generation = self.generation;
        if let Some(img) = self.images.get_mut(&image_id) {
            img.generation = generation;
        }
    }

    /// Convert a stored image's base data to RGBA in place, adjusting
    /// byte accounting. All animation composition happens in RGBA;
    /// this is called before the first composition into an image.
    ///
    /// The pixels are unchanged visually but the stored representation
    /// changed, so the image is stamped with a fresh generation.
    pub fn convert_image_to_rgba(&mut self, image_id: u32) -> Result<(), AllocError> {
        let Some(img) = self.images.get_mut(&image_id) else {
            return Ok(());
        };
        if img.format == command::Format::Rgba {
            return Ok(());
        }
        let Some(old) = img.data.bytes() else {
            return Ok(());
        };

        let rgba = pixel::rgba_from_format(img.format, old);
        self.total_bytes -= old.len();
        self.total_bytes += rgba.len();
        img.data = Data::Complete(rgba);
        img.format = command::Format::Rgba;
        self.mark_image_content_changed(image_id);
        Ok(())
    }

    /// Reserve `bytes` of storage for animation frame data belonging
    /// to `image_id`, evicting other images if needed, mirroring how
    /// image transmission reserves space.
    ///
    /// Errors if the space cannot be made available. On success the caller
    /// owns the reservation and must either attach the frame data to the
    /// image or call releaseAnimationBytes.
    pub fn reserve_animation_bytes(
        &mut self,
        pages: &mut PageList,
        image_id: u32,
        bytes: usize,
    ) -> Result<(), AllocError> {
        if bytes > self.total_limit {
            return Err(AllocError::OutOfMemory);
        }

        let total_bytes = self.total_bytes + bytes;
        if total_bytes > self.total_limit {
            let req_bytes = total_bytes - self.total_limit;
            // Excess this large cannot be recovered by evicting other
            // images (evictImageExcept also requires it).
            if req_bytes > self.total_limit {
                return Err(AllocError::OutOfMemory);
            }
            log::info!(target: LOG_TARGET, "evicting images for animation frame, evicting={req_bytes}");
            if !self.evict_image_except(pages, req_bytes, Some(image_id)) {
                log::warn!(target: LOG_TARGET, "failed to evict enough images for animation frame");
                return Err(AllocError::OutOfMemory);
            }
        }

        self.total_bytes += bytes;
        Ok(())
    }

    /// Release a reservation made by reserveAnimationBytes, or credit
    /// bytes freed by deleting animation frame data.
    pub fn release_animation_bytes(&mut self, bytes: usize) {
        debug_assert!(bytes <= self.total_bytes);
        self.total_bytes -= bytes;
    }

    /// Advance every running animation to the frame that should be
    /// displayed at `now_ms` and report when the next frame change is
    /// due, as a delay in milliseconds relative to `now_ms`. Null
    /// means no running animation needs a future tick.
    ///
    /// `now_ms` is a monotonic timestamp on a clock of the caller's
    /// choosing. The same clock must be used for every call. The
    /// caller is expected to be the renderer, ticking once per frame
    /// build and scheduling a wakeup for the returned delay.
    ///
    /// When nothing is playing and the storage hasn't changed since
    /// the last tick, this returns null right away instead of looking
    /// at every image.
    pub fn animation_tick(&mut self, now_ms: u64) -> Option<u64> {
        // Nothing changed since the last tick found nothing to play,
        // so there is still nothing to play. See
        // animation_idle_generation.
        if self.animation_idle_generation == Some(self.generation) {
            return None;
        }

        let mut min_delay: Option<u64> = None;

        let ids: Vec<u32> = self.images.keys().copied().collect();
        for id in ids {
            let img = self.images.get_mut(&id).unwrap();

            // The gates below mirror Kitty's image_is_animatable.

            // No animation state was ever attached (plain image).
            let placement_count = img.metadata.placement_count;
            let data_pending = img.data.is_pending();
            let Some(anim) = img.animation.as_mut() else {
                continue;
            };

            // Stopped is the initial state of every animation: frames
            // then only change client-driven (a=a c=N), never by time.
            if anim.state == animation::State::Stopped {
                continue;
            }

            // Only the root frame exists; there is nothing to advance
            // to yet even in the running state.
            if anim.frames.is_empty() {
                continue;
            }

            // Unplaced images don't animate. This is our simpler
            // approximation of Kitty's "is actually drawn" visibility
            // gate; it is what stops an image that was transmitted but
            // never placed from waking the renderer forever.
            if placement_count == 0 {
                continue;
            }

            // The base pixel data hasn't arrived yet (e.g. an image
            // restored from a snapshot); nothing can be displayed.
            if data_pending {
                continue;
            }

            // For the same reason, playback waits until every frame's
            // pixels have arrived.
            if anim.has_pending_frames() {
                continue;
            }

            // A zero total duration means every frame is gapless and
            // no frame can ever be displayed, so the animation can
            // never advance. This also guards the gapless-skip loop
            // below from never terminating.
            if anim.duration_ms() == 0 {
                continue;
            }

            // A finite loop budget (a=a v=N) that ran out on an
            // earlier tick froze playback on the last frame for good.
            if anim.max_loops > 0 && anim.current_loop >= anim.max_loops {
                continue;
            }

            let shown_at: u64 = match anim.frame_shown_at_ms {
                // First tick since playback started (or since the
                // current frame changed through another path, e.g.
                // a=a c=N): the frame is considered shown as of now,
                // and its gap starts counting from here.
                None => now_ms,

                // A timestamp from the future means the caller's clock
                // restarted; re-anchor rather than stalling until the
                // old timestamp comes around again.
                Some(at) if at > now_ms => now_ms,

                Some(at) => at,
            };
            anim.frame_shown_at_ms = Some(shown_at);

            // The current frame is replaced once its gap has elapsed.
            // We advance at most one displayed frame per tick with no
            // catch-up, exactly like Kitty: if ticks lag behind the
            // gaps, the animation slows down rather than skipping.
            let mut next_at: u64 = shown_at.saturating_add(anim.gap_at(anim.current_index) as u64);
            let mut advanced = false;
            if now_ms >= next_at {
                'advance: {
                    // Walk forward to the next displayable frame. This is
                    // a loop only because gapless (gap=0) frames are never
                    // displayed and are stepped over; the durationMs gate
                    // above guarantees a displayable frame exists.
                    let count: u32 = anim.frame_count();
                    let mut idx = anim.current_index;
                    loop {
                        let next = (idx + 1) % count;
                        if next == 0 {
                            // Wrapping past the last frame back to the
                            // root. A loading-state (a=a s=2) animation
                            // refuses the wrap: it parks on the last frame
                            // awaiting more frames from the client.
                            if anim.state == animation::State::Loading {
                                break 'advance;
                            }

                            // Each wrap completes a loop; a finite budget
                            // that just ran out parks on the last frame.
                            anim.current_loop += 1;
                            if anim.max_loops > 0 && anim.current_loop >= anim.max_loops {
                                break 'advance;
                            }
                        }
                        idx = next;
                        if anim.gap_at(idx) != 0 {
                            break;
                        }
                    }

                    // Show the chosen frame: restart its gap timer and
                    // stamp a fresh generation so consumers (the renderer
                    // texture cache, the C API) pick up the new pixels.
                    anim.current_index = idx;
                    anim.frame_shown_at_ms = Some(now_ms);
                    next_at = now_ms.saturating_add(anim.gap_at(idx) as u64);
                    advanced = true;
                }
            }
            if advanced {
                self.mark_image_content_changed(id);
            }

            // Schedule the next tick. A parked animation left next_at
            // in the past and so never schedules one; it is woken by
            // its trigger instead (a new frame arriving, or an a=a
            // command changing the state).
            if next_at > now_ms {
                let delay = next_at - now_ms;
                min_delay = Some(match min_delay {
                    Some(m) => m.min(delay),
                    None => delay,
                });
            }
        }

        // Remember the generation only when nothing is playing. We
        // read it after the loop because showing a new frame above
        // changes it. That case always schedules another tick, so it
        // never records a stale value here.
        self.animation_idle_generation = if min_delay.is_none() {
            Some(self.generation)
        } else {
            None
        };

        min_delay
    }

    /// Clear placements intersecting the active screen, then reclaim every
    /// image with no remaining placement. Unlike protocol d=A, a terminal
    /// clear also reclaims images that were already unplaced.
    pub fn clear_screen(&mut self, pages: &mut PageList, t: &TerminalInfo) {
        let placements_before = self.placements.len();
        let images_before = self.images.len();

        // Delete unused placements and images. Orphaned relative
        // placements must be reaped before the image sweep so that
        // their images are reclaimable too.
        self.delete_visible_placements(pages, t, true);
        let _ = self.remove_orphans(pages, false);
        let ids: Vec<u32> = self.images.keys().copied().collect();
        for id in ids {
            self.delete_if_unused(id);
        }

        // Mark mutated only if things changed.
        if self.placements.len() != placements_before || self.images.len() != images_before {
            self.mark_mutated();
        }
    }

    /// Delete placements, images.
    pub fn delete(&mut self, pages: &mut PageList, t: &TerminalInfo, cmd: command::DeleteAction) {
        use command::DeleteAction;

        // Deletes only ever remove placements/images, so comparing counts
        // before and after tells us whether anything actually changed.
        // Only then do we mark a mutation, so a delete that matches nothing
        // doesn't dirty the image state or bump the generation.
        let placements_before = self.placements.len();
        let images_before = self.images.len();

        match cmd {
            DeleteAction::All(delete_images) => {
                self.delete_visible_placements(pages, t, delete_images)
            }

            DeleteAction::Id(v) => self.delete_by_id(pages, v.image_id, v.placement_id, v.delete),

            DeleteAction::Newest(v) => {
                if let Some(img) = self.image_by_number(v.image_number) {
                    let id = img.id;
                    self.delete_by_id(pages, id, v.placement_id, v.delete);
                }
            }

            DeleteAction::IntersectCursor(delete_images) => {
                self.delete_intersecting(
                    pages,
                    t,
                    point::Point::Active(point::Coordinate {
                        x: t.cursor_x,
                        y: t.cursor_y as u32,
                    }),
                    delete_images,
                    None,
                );
            }

            DeleteAction::IntersectCell(v) => 'intersect_cell: {
                if v.x == 0 || v.y == 0 {
                    log::warn!(target: LOG_TARGET, "delete intersect cell coords must be at least 1");
                    break 'intersect_cell;
                }

                let Ok(x) = CellCountInt::try_from(v.x - 1) else {
                    break 'intersect_cell;
                };
                let Ok(y) = CellCountInt::try_from(v.y - 1) else {
                    break 'intersect_cell;
                };
                self.delete_intersecting(
                    pages,
                    t,
                    point::Point::Active(point::Coordinate { x, y: y as u32 }),
                    v.delete,
                    None,
                );
            }

            DeleteAction::IntersectCellZ(v) => 'intersect_cell_z: {
                if v.x == 0 || v.y == 0 {
                    log::warn!(target: LOG_TARGET, "delete intersect cell coords must be at least 1");
                    break 'intersect_cell_z;
                }

                let Ok(x) = CellCountInt::try_from(v.x - 1) else {
                    break 'intersect_cell_z;
                };
                let Ok(y) = CellCountInt::try_from(v.y - 1) else {
                    break 'intersect_cell_z;
                };
                self.delete_intersecting(
                    pages,
                    t,
                    point::Point::Active(point::Coordinate { x, y: y as u32 }),
                    v.delete,
                    Some(v.z),
                );
            }

            DeleteAction::Column(v) => 'column: {
                if v.x == 0 {
                    log::warn!(target: LOG_TARGET, "delete column must be greater than zero");
                    break 'column;
                }

                let x = v.x - 1;
                let matches: Vec<(PlacementKey, u32)> = self
                    .placements
                    .iter()
                    .filter_map(|(k, p)| {
                        let img = self.image_by_id(k.image_id)?;
                        let rect = p.rect(img, t)?;
                        if rect.top_left.x as u32 <= x && rect.bottom_right.x as u32 >= x {
                            Some((*k, img.id))
                        } else {
                            None
                        }
                    })
                    .collect();
                for (key, image_id) in matches {
                    self.remove_placement(pages, key);
                    if v.delete {
                        self.delete_if_unused(image_id);
                    }
                }
            }

            DeleteAction::Row(v) => 'row: {
                if v.y == 0 {
                    log::warn!(target: LOG_TARGET, "delete row must be greater than zero");
                    break 'row;
                }

                // v.y is in active coords so we want to convert it to a pin
                // so we can compare by page offsets.
                let Ok(y) = CellCountInt::try_from(v.y - 1) else {
                    break 'row;
                };
                let Some(target_pin) = pages.pin(point::Point::Active(point::Coordinate {
                    x: 0,
                    y: y as u32,
                })) else {
                    break 'row;
                };

                let matches: Vec<(PlacementKey, u32)> = self
                    .placements
                    .iter()
                    .filter_map(|(k, p)| {
                        let img = self.image_by_id(k.image_id)?;
                        let rect = p.rect(img, t)?;

                        // We need to copy our pin to ensure we are at least at
                        // the top-left x.
                        let mut target_pin_copy = target_pin;
                        target_pin_copy.x = rect.top_left.x;
                        if target_pin_copy.is_between(rect.top_left, rect.bottom_right) {
                            Some((*k, img.id))
                        } else {
                            None
                        }
                    })
                    .collect();
                for (key, image_id) in matches {
                    self.remove_placement(pages, key);
                    if v.delete {
                        self.delete_if_unused(image_id);
                    }
                }
            }

            DeleteAction::Z(v) => {
                let matches: Vec<PlacementKey> = self
                    .placements
                    .iter()
                    .filter(|(_, p)| match p.location {
                        // Relative placements carry their own z value and
                        // are matched by z deletes just like kitty does.
                        Location::Pin(_) | Location::Relative(_) => p.z == v.z,

                        // Virtual placeholders cannot delete by z according
                        // to the spec.
                        Location::Virtual => false,
                    })
                    .map(|(k, _)| *k)
                    .collect();
                for key in matches {
                    let image_id = key.image_id;
                    self.remove_placement(pages, key);
                    if v.delete {
                        self.delete_if_unused(image_id);
                    }
                }
            }

            DeleteAction::Range(v) => 'range: {
                // Both bounds default to zero when omitted. An inverted range
                // or a zero upper bound is not an error, it just selects
                // nothing: image IDs are always non-zero.
                if v.last == 0 || v.first > v.last {
                    break 'range;
                }

                // Remove matching placements in one pass.
                let matches: Vec<PlacementKey> = self
                    .placements
                    .keys()
                    .filter(|k| k.image_id >= v.first && k.image_id <= v.last)
                    .copied()
                    .collect();
                for key in matches {
                    self.remove_placement(pages, key);
                }

                // Uppercase deletion also frees matching images that are now
                // unused, including images that had no placements initially.
                if !v.delete {
                    break 'range;
                }
                let ids: Vec<u32> = self
                    .images
                    .keys()
                    .filter(|&&id| id >= v.first && id <= v.last)
                    .copied()
                    .collect();
                for image_id in ids {
                    self.delete_if_unused(image_id);
                }
            }

            DeleteAction::AnimationFrames(v) => self.delete_animation_frame(pages, v),
        }

        // Deleting placements orphans any relative placements parented
        // to them (transitively). Their lifetime is tied to the parent
        // so they are removed as well, and an uppercase delete also
        // frees any image the cascade leaves without placements (the
        // per-branch deleteIfUnused calls above ran while the orphans
        // still counted as placements).
        let delete_unused: bool = match cmd {
            DeleteAction::All(v) | DeleteAction::IntersectCursor(v) => v,
            DeleteAction::Id(v) => v.delete,
            DeleteAction::Newest(v) => v.delete,
            DeleteAction::AnimationFrames(v) => v.delete,
            DeleteAction::IntersectCell(v) => v.delete,
            DeleteAction::IntersectCellZ(v) => v.delete,
            DeleteAction::Range(v) => v.delete,
            DeleteAction::Column(v) => v.delete,
            DeleteAction::Row(v) => v.delete,
            DeleteAction::Z(v) => v.delete,
        };
        let _ = self.remove_orphans(pages, delete_unused);

        if self.placements.len() != placements_before || self.images.len() != images_before {
            self.mark_mutated();
        }
    }

    /// Delete only non-virtual placements that intersect the active screen.
    /// The protocol defines d=a/A in terms of visible placements, and an
    /// uppercase delete only frees data for images selected by those
    /// placements.
    fn delete_visible_placements(&mut self, pages: &mut PageList, t: &TerminalInfo, delete_unused: bool) {
        let matches: Vec<PlacementKey> = self
            .placements
            .iter()
            .filter(|(k, p)| {
                let pin_ptr = match p.location {
                    Location::Pin(pin) => pin,
                    // Virtual placements are never selected by visible
                    // deletes per the protocol. Relative placements are
                    // removed with their parents instead (removeOrphans).
                    Location::Virtual | Location::Relative(_) => return false,
                };
                // SAFETY: tracked pin owned by `pages`.
                let pin = unsafe { *pin_ptr.as_ptr() };
                if pin.garbage {
                    return false;
                }

                // Placements anchored in the active area are necessarily visible.
                // Only compute their extent when the anchor is already in history
                // and the bottom edge may still intersect the active area.
                if pages.point_from_pin(point::Tag::Active, pin).is_none() {
                    let Some(image) = self.image_by_id(k.image_id) else {
                        return false;
                    };
                    let Some(rect) = p.rect(image, t) else {
                        return false;
                    };
                    if pages
                        .point_from_pin(point::Tag::Active, rect.bottom_right)
                        .is_none()
                    {
                        return false;
                    }
                }

                true
            })
            .map(|(k, _)| *k)
            .collect();
        for key in matches {
            let image_id = key.image_id;
            self.remove_placement(pages, key);
            if delete_unused {
                self.delete_if_unused(image_id);
            }
        }
    }

    fn delete_by_id(
        &mut self,
        pages: &mut PageList,
        image_id: u32,
        placement_id: u32,
        delete_unused: bool,
    ) {
        let mut matched = placement_id == 0;

        // If no placement, we delete all placements with the ID
        if placement_id == 0 {
            self.remove_placements_by_image_id(pages, image_id);
        } else {
            let key = PlacementKey {
                image_id,
                placement_id: PlacementId {
                    tag: PlacementTag::External,
                    id: placement_id,
                },
            };
            if self.placements.contains_key(&key) {
                self.remove_placement(pages, key);
                matched = true;
            }
        }

        // A placement ID narrows the selection to that exact placement, so an
        // unmatched selector must not free otherwise-unreferenced image data.
        // https://sw.kovidgoyal.net/kitty/graphics-protocol/#deleting-images
        if delete_unused && matched {
            self.delete_if_unused(image_id);
        }
    }

    /// Delete an animation frame (d=f/F). Deletes never produce
    /// responses, so all failures are only logged. Kitty behaviors
    /// implemented here: on an image without extra frames a lowercase
    /// delete is a no-op while an uppercase delete removes the entire
    /// image, placements included; the frame number is clamped to the
    /// last frame and zero selects the root frame; deleting the root
    /// frame promotes frame 2 to be the new root.
    fn delete_animation_frame(&mut self, pages: &mut PageList, v: command::AnimationFrames) {
        if v.image_id == 0 && v.image_number == 0 {
            log::warn!(target: LOG_TARGET, "delete animation frames requires image id or number");
            return;
        }
        let Some(img) = self.image_ptr_by_id_or_number(v.image_id, v.image_number) else {
            log::warn!(
                target: LOG_TARGET,
                "delete animation frames for unknown image id={} number={}",
                v.image_id,
                v.image_number
            );
            return;
        };
        let image_id = img.id;

        let has_frames = img.animation.as_ref().is_some_and(|a| !a.frames.is_empty());
        if !has_frames {
            // The image is not (or no longer) an animation. The
            // uppercase delete removes the entire image, even when it
            // still has placements.
            if !v.delete {
                return;
            }
            self.remove_placements_by_image_id(pages, image_id);
            let removed = self.images.remove(&image_id).unwrap();
            self.total_bytes -= removed.storage_size();
            return;
        }

        let img = self.images.get_mut(&image_id).unwrap();
        let anim = img.animation.as_mut().unwrap();

        // Clamp the frame number: zero selects the root frame and
        // values past the end select the last frame.
        let count: u32 = anim.frame_count();
        let mut number: u32 = v.frame.min(count);
        if number == 0 {
            number = 1;
        }

        let released: usize;
        if number == 1 {
            // Deleting the root frame promotes frame 2 to root. The
            // promoted frame's bytes stay reserved and only the old root
            // data is freed. If the promoted frame is still pending, the
            // image's own pixels become pending.
            released = img.data.len();
            let promoted = anim.frames.remove(0);
            anim.root_gap_ms = promoted.gap_ms;
            img.data = if promoted.is_pending() {
                Data::Pending(promoted.pending_len)
            } else {
                Data::Complete(promoted.data)
            };
        } else {
            let removed = anim.frames.remove((number - 2) as usize);
            released = removed.storage_size();
        }
        let anim = img.animation.as_mut().unwrap();

        // Fix up the current frame.
        let removed_idx: u32 = if number == 1 { 0 } else { number - 2 };
        let remaining: u32 = anim.frames.len() as u32;
        let content_changed: bool;
        if anim.current_index > remaining {
            anim.current_index = remaining;
            anim.frame_shown_at_ms = None;
            content_changed = true;
        } else if removed_idx == anim.current_index {
            anim.frame_shown_at_ms = None;
            content_changed = true;
        } else {
            if removed_idx < anim.current_index {
                anim.current_index -= 1;
            }
            content_changed = false;
        }

        self.release_animation_bytes(released);
        if content_changed {
            self.mark_image_content_changed(image_id);
        } else {
            self.mark_mutated();
        }
    }

    /// Delete an image if it is unused.
    fn delete_if_unused(&mut self, image_id: u32) {
        let Some(img) = self.images.get(&image_id) else {
            return;
        };
        if img.metadata.placement_count > 0 {
            return;
        }

        self.total_bytes -= img.storage_size();
        self.images.remove(&image_id);
    }

    /// Deletes all placements intersecting a screen point. If `z_filter`
    /// is set, only placements with that z-index are deleted (Zig passes a
    /// filter function and context).
    fn delete_intersecting(
        &mut self,
        pages: &mut PageList,
        t: &TerminalInfo,
        p: point::Point,
        delete_unused: bool,
        z_filter: Option<i32>,
    ) {
        // Convert our target point to a pin for comparison.
        let Some(target_pin) = pages.pin(p) else {
            return;
        };

        let matches: Vec<(PlacementKey, u32)> = self
            .placements
            .iter()
            .filter_map(|(k, placement)| {
                let img = self.image_by_id(k.image_id)?;
                let rect = placement.rect(img, t)?;
                if !rect.contains(target_pin) {
                    return None;
                }
                if let Some(z) = z_filter {
                    if placement.z != z {
                        return None;
                    }
                }
                Some((*k, img.id))
            })
            .collect();
        for (key, image_id) in matches {
            self.remove_placement(pages, key);
            if delete_unused {
                self.delete_if_unused(image_id);
            }
        }
    }

    /// Evict image to make space. This will evict the oldest image,
    /// prioritizing transient images and unused images as recommended by the
    /// published Kitty spec.
    ///
    /// This will evict as many images as necessary to make space for
    /// req bytes.
    fn evict_image(&mut self, pages: &mut PageList, req: usize) -> bool {
        self.evict_image_except(pages, req, None)
    }

    /// Evict images while preserving `exclude_id`, used when replacing an
    /// existing image whose old reservation has already been credited.
    fn evict_image_except(&mut self, pages: &mut PageList, req: usize, exclude_id: Option<u32>) -> bool {
        debug_assert!(req <= self.total_limit);

        #[derive(Clone, Copy)]
        struct Candidate {
            id: u32,
            generation: u64,

            // Lower values are evicted first:
            // 0: transient, unused
            // 1: not transient, unused
            // 2: transient, used
            // 3: not transient, used
            priority: u8,
        }

        impl Candidate {
            fn init(img: &Image) -> Candidate {
                Candidate {
                    id: img.id,
                    generation: img.generation,
                    priority: (if img.metadata.transient { 0 } else { 1 })
                        + (if img.metadata.placement_count > 0 { 2 } else { 0 }),
                }
            }

            fn less_than(&self, rhs: &Candidate) -> bool {
                if self.priority != rhs.priority {
                    return self.priority < rhs.priority;
                }
                if self.generation != rhs.generation {
                    return self.generation < rhs.generation;
                }
                self.id < rhs.id
            }
        }

        // Evicting anything is a content mutation. This matters for the
        // setLimit path in particular, which doesn't otherwise mark it.
        // Evicted placements can also orphan relative placements of
        // other images, which must be reaped along with them.
        let images_before = self.images.len();

        let mut evicted: usize = 0;
        let mut result = true;
        while evicted < req {
            let mut best: Option<Candidate> = None;
            for img in self.images.values() {
                if exclude_id.is_some() && img.id == exclude_id.unwrap() {
                    continue;
                }

                let current = Candidate::init(img);
                if best.is_none() || current.less_than(best.as_ref().unwrap()) {
                    best = Some(current);
                }
            }
            let Some(c) = best else {
                result = false;
                break;
            };

            self.remove_placements_by_image_id(pages, c.id);

            let img = self.images.remove(&c.id).unwrap();
            let image_len = img.storage_size();
            log::info!(target: LOG_TARGET, "evicting image id={} bytes={}", c.id, image_len);

            evicted += image_len;
            self.total_bytes -= image_len;
        }

        if self.images.len() != images_before {
            let _ = self.remove_orphans(pages, false);
            self.mark_mutated();
        }

        result
    }
}

/// State returned by scrollMarginsBegin: the final active-area
/// position every surviving placement pin must be restored to once
/// the scrolled rows have settled. Every begin must be paired with
/// exactly one end() call after the scroll's row operations
/// complete.
#[derive(Debug, Default)]
pub struct ScrollMargins {
    pub restores: Vec<Restore>,
}

/// `ScrollMargins.Restore`
#[derive(Debug, Clone, Copy)]
pub struct Restore {
    pub pin: PinPtr,
    pub x: CellCountInt,
    pub y: u32,
}

impl ScrollMargins {
    /// Reposition every recorded placement pin to its post-scroll
    /// position and release the state. Must be called exactly
    /// once, after the scroll's row operations complete. `pages` is the
    /// page list of the screen the state was created for.
    pub fn end(self, pages: &PageList) {
        for restore in &self.restores {
            let Some(p) = pages.pin(point::Point::Active(point::Coordinate {
                x: restore.x,
                y: restore.y,
            })) else {
                continue;
            };
            // SAFETY: the pin is tracked by `pages` (its placement
            // survived the scroll) and we have exclusive access to the
            // page list's tracked pins through the caller.
            unsafe { *restore.pin.as_ptr() = p };
        }
    }
}

/// Every placement is uniquely identified by the image ID and the
/// placement ID. If an image ID isn't specified it is assumed to be 0.
/// Likewise, if a placement ID isn't specified it is assumed to be 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlacementKey {
    pub image_id: u32,
    pub placement_id: PlacementId,
}

impl PlacementKey {
    pub fn eql(self, other: PlacementKey) -> bool {
        self == other
    }
}

/// `PlacementId.tag`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlacementTag {
    Internal,
    External,
}

/// Internal placement IDs are assigned by us for placements created
/// without an explicit ID (p=0); external IDs are client-specified.
/// The two are separate namespaces, hence the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlacementId {
    pub tag: PlacementTag,
    pub id: u32,
}

impl PlacementId {
    /// Deterministic preference order used when an operation must
    /// pick a single placement out of several (kitty uses creation
    /// order, which our map doesn't track): external IDs win over
    /// internal ones, and lower IDs win within a tag.
    pub fn preferred_over(self, other: PlacementId) -> bool {
        if self.tag != other.tag {
            return self.tag == PlacementTag::External;
        }
        self.id < other.id
    }
}

/// `ImageStorage.Placement`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// The location where this placement should be drawn.
    pub location: Location,

    /// Offset of the x/y from the top-left of the cell.
    pub x_offset: u32,
    pub y_offset: u32,

    /// Source rectangle for the image to pull from
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,

    /// The columns/rows this image occupies.
    pub columns: u32,
    pub rows: u32,

    /// The z-index for this placement.
    pub z: i32,
}

/// `Placement.Location`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// Exactly placed on a screen pin. The pin is tracked by the page
    /// list of the screen that owns the storage, which keeps it valid
    /// until the placement is removed (`Placement::deinit` untracks it).
    Pin(PinPtr),

    /// Virtual placement (U=1) for unicode placeholders.
    Virtual,

    /// Placed relative to a parent placement (P=/Q=). The
    /// placement has no screen position of its own; it is
    /// positioned at render time by resolving the parent chain
    /// to its root (see resolveChain) and therefore follows the
    /// parent through scrolls automatically. Its lifetime is
    /// tied to the parent: removing any ancestor removes it
    /// too (see removeOrphans).
    Relative(Relative),
}

/// `Placement.Relative`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relative {
    /// The parent placement. Resolved to a concrete key when
    /// the placement is created, so the parent is guaranteed
    /// to exist at that point.
    pub parent: PlacementKey,

    /// Cell offsets from the parent's origin (H=/V=).
    pub horizontal_offset: i32,
    pub vertical_offset: i32,
}

impl Relative {
    /// A relative placement with no offsets.
    pub fn new(parent: PlacementKey) -> Relative {
        Relative {
            parent,
            horizontal_offset: 0,
            vertical_offset: 0,
        }
    }
}

/// `Placement.SourceRect`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The result of [`Placement::cell_offset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellOffset {
    pub x: u32,
    pub y: u32,
}

/// The result of [`Placement::pixel_size`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelSize {
    pub width: u32,
    pub height: u32,
}

/// The result of [`Placement::grid_size`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u32,
    pub rows: u32,
}

impl Placement {
    /// A placement at `location` with every other field zero (Zig: a
    /// struct literal with defaults).
    pub const fn new(location: Location) -> Placement {
        Placement {
            location,
            x_offset: 0,
            y_offset: 0,
            source_x: 0,
            source_y: 0,
            source_width: 0,
            source_height: 0,
            columns: 0,
            rows: 0,
            z: 0,
        }
    }

    pub fn deinit(&self, pages: &mut PageList) {
        match self.location {
            Location::Pin(p) => pages.untrack_pin(p),
            Location::Virtual | Location::Relative(_) => {}
        }
    }

    /// Multiply two protocol-controlled values without allowing them to
    /// wrap. Placement geometry is exposed as u32, so values larger than
    /// that are represented by the largest possible value.
    fn saturating_mul(lhs: u32, rhs: u32) -> u32 {
        lhs.checked_mul(rhs).unwrap_or(u32::MAX)
    }

    /// Scale a dimension by an aspect ratio and round to the nearest
    /// integer. The u64 intermediate can hold the product of two u32s as
    /// well as the rounding adjustment.
    fn scale_dimension(value: u32, numerator: u32, denominator: u32) -> u32 {
        if denominator == 0 {
            return 0;
        }

        let rounded = (value as u64 * numerator as u64 + denominator as u64 / 2) / denominator as u64;
        u32::try_from(rounded).unwrap_or(u32::MAX)
    }

    /// Returns the requested source rectangle intersected with the image.
    /// A zero width or height requests the full corresponding image
    /// dimension before intersection, as defined by the Kitty protocol.
    pub fn source_rect(&self, image: &Image) -> SourceRect {
        let x = self.source_x.min(image.width);
        let y = self.source_y.min(image.height);
        SourceRect {
            x,
            y,
            width: (if self.source_width > 0 {
                self.source_width
            } else {
                image.width
            })
            .min(image.width - x),
            height: (if self.source_height > 0 {
                self.source_height
            } else {
                image.height
            })
            .min(image.height - y),
        }
    }

    /// Returns the placement offset clamped within the first cell. Pixel
    /// geometry may temporarily be unavailable, in which case there is no
    /// valid offset within the cell.
    pub fn cell_offset(&self, t: &TerminalInfo) -> CellOffset {
        let cell_width: u32 = t.width_px / t.cols as u32;
        let cell_height: u32 = t.height_px / t.rows as u32;

        CellOffset {
            x: if cell_width > 0 {
                self.x_offset.min(cell_width - 1)
            } else {
                0
            },
            y: if cell_height > 0 {
                self.y_offset.min(cell_height - 1)
            } else {
                0
            },
        }
    }

    /// Returns the size of this placement's image in pixels,
    /// taking into account the source rectangle, specified
    /// rows/columns, and aspect ratio.
    pub fn pixel_size(&self, image: &Image, t: &TerminalInfo) -> PixelSize {
        let source = self.source_rect(image);
        let width = source.width;
        let height = source.height;

        // If we don't have any specified cols or rows then the placement
        // should be the native size of the image, and doesn't need to be
        // re-scaled.
        if self.columns == 0 && self.rows == 0 {
            return PixelSize { width, height };
        }

        // We calculate the size of a cell so that we can multiply
        // it by the specified cols/rows to get the correct px size.
        //
        // We assume that the width is divided evenly by the column
        // count and the height by the row count, because it should be.
        let cell_width: u32 = t.width_px / t.cols as u32;
        let cell_height: u32 = t.height_px / t.rows as u32;
        let cell_offset = self.cell_offset(t);

        // If we have a specified cols AND rows then we calculate
        // the width and height from them directly, we don't need
        // to adjust for aspect ratio.
        if self.columns > 0 && self.rows > 0 {
            let calc_width = Self::saturating_mul(cell_width, self.columns);
            let calc_height = Self::saturating_mul(cell_height, self.rows);

            return PixelSize {
                width: calc_width.saturating_sub(cell_offset.x),
                height: calc_height.saturating_sub(cell_offset.y),
            };
        }

        // Either the columns or the rows were specified, but not both,
        // so we need to calculate the other one based on the aspect ratio.

        // If only the columns were specified, we determine
        // the height of the image based on the aspect ratio.
        if self.columns > 0 {
            let calc_width =
                Self::saturating_mul(cell_width, self.columns).saturating_sub(cell_offset.x);
            let calc_height = Self::scale_dimension(calc_width, height, width);

            return PixelSize {
                width: calc_width,
                height: calc_height,
            };
        }

        // Otherwise, only the rows were specified, so we
        // determine the width based on the aspect ratio.
        let calc_height = Self::saturating_mul(cell_height, self.rows).saturating_sub(cell_offset.y);
        let calc_width = Self::scale_dimension(calc_height, width, height);

        PixelSize {
            width: calc_width,
            height: calc_height,
        }
    }

    /// Returns the size in grid cells that this placement takes up.
    pub fn grid_size(&self, image: &Image, t: &TerminalInfo) -> GridSize {
        // If we have a specified columns and rows then this is trivial.
        if self.columns > 0 && self.rows > 0 {
            return GridSize {
                cols: self.columns,
                rows: self.rows,
            };
        }

        // Otherwise we calculate the pixel size, divide by
        // cell size, and round up to the nearest integer.
        let calc_size = self.pixel_size(image, t);
        let cell_offset = self.cell_offset(t);
        // NOTE: The divisions can only fail if the cell size is 0,
        //       in such a case it seems safe to return 0 for this.
        GridSize {
            cols: div_ceil(
                calc_size.width.saturating_add(cell_offset.x),
                t.width_px / t.cols as u32,
            ),
            rows: div_ceil(
                calc_size.height.saturating_add(cell_offset.y),
                t.height_px / t.rows as u32,
            ),
        }
    }

    /// Permanently clip `count` rows off the top of this placement,
    /// which currently spans `span` grid rows, by shrinking the
    /// source rectangle (the requested source rect is materialized
    /// into explicit values in the process). Used when a scroll
    /// within margins pushes the placement past the top margin.
    /// Requires 0 < count < span. Returns false if no visible
    /// source pixels would remain, in which case the placement is
    /// unmodified and should be deleted.
    fn clip_top(&mut self, image: &Image, t: &TerminalInfo, count: u32, span: u32) -> bool {
        debug_assert!(count > 0 && count < span);
        let src = self.source_rect(image);
        let crop: u32 = if self.rows > 0 {
            // Scaled placement: each grid row shows an equal
            // fraction of the source.
            (src.height as u64 * count as u64 / span as u64) as u32
        } else {
            // Native size: each grid row shows one cell height of
            // source pixels (kitty does the same).
            Self::saturating_mul(t.height_px / t.rows as u32, count)
        };
        if crop >= src.height {
            return false;
        }
        self.source_x = src.x;
        self.source_y = src.y + crop;
        self.source_width = src.width;
        self.source_height = src.height - crop;
        if self.rows > 0 {
            self.rows -= count;
        }
        true
    }

    /// clipTop, but clipping `count` rows off the bottom.
    fn clip_bottom(&mut self, image: &Image, t: &TerminalInfo, count: u32, span: u32) -> bool {
        debug_assert!(count > 0 && count < span);
        let src = self.source_rect(image);
        // An empty source can never be clipped smaller. This also
        // guarantees we never store an explicit zero source height,
        // which would be reinterpreted as "full image".
        if src.height == 0 {
            return false;
        }
        if self.rows > 0 {
            let crop: u32 = (src.height as u64 * count as u64 / span as u64) as u32;
            if crop >= src.height {
                return false;
            }
            self.source_height = src.height - crop;
            self.rows -= count;
        } else {
            // The remaining rows can show this many source pixels,
            // with the first cell losing cellOffset pixels to the
            // placement's y offset.
            let visible = Self::saturating_mul(t.height_px / t.rows as u32, span - count);
            let offset = self.cell_offset(t).y;
            if visible <= offset {
                return false;
            }
            self.source_height = src.height.min(visible - offset);
        }
        self.source_x = src.x;
        self.source_y = src.y;
        self.source_width = src.width;
        true
    }

    /// Returns a selection of the entire rectangle this placement
    /// occupies within the screen. This can return null for a virtual
    /// placement or when unavailable pixel geometry makes it empty.
    ///
    /// Relative placements also return null: they have no screen
    /// position of their own, so geometric queries (delete by
    /// intersection, row, column, etc.) never match them directly.
    /// They are instead removed when their parent chain is removed.
    pub fn rect(&self, image: &Image, t: &TerminalInfo) -> Option<Rect> {
        let grid_size = self.grid_size(image, t);
        let pin_ptr = match self.location {
            Location::Pin(p) => p,
            Location::Virtual | Location::Relative(_) => return None,
        };
        // SAFETY: tracked pin owned by the screen's page list.
        let pin: Pin = unsafe { *pin_ptr.as_ptr() };
        if pin.garbage {
            return None;
        }

        // A zero pixel-sized placement can produce a zero grid size when
        // pixel geometry is unavailable. It occupies no rectangle.
        if grid_size.cols == 0 || grid_size.rows == 0 {
            return None;
        }

        let mut br = match pin.down_overflow((grid_size.rows - 1) as usize) {
            PinOverflow::Offset(v) => v,
            PinOverflow::Overflow { end, .. } => end,
        };
        br.x = (pin.x as u32)
            // We need to sub one here because the x value is
            // one width already. So if the image is width "1"
            // then we add zero to X because X itself is width 1.
            .saturating_add(grid_size.cols - 1)
            .min(t.cols as u32 - 1) as CellCountInt;

        Some(Rect {
            top_left: pin,
            bottom_right: br,
        })
    }
}

/// `std.math.divCeil` for the grid size, returning 0 for a zero divisor.
fn div_ceil(numerator: u32, denominator: u32) -> u32 {
    if denominator == 0 {
        return 0;
    }
    numerator.div_ceil(denominator)
}
