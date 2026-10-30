//! Port of `src/terminal/snapshot/snapshot.zig`.
//!
//! STATUS: ported
//!
//! Complete terminal snapshot encoding and restoration.
//!
//! Notes on the port:
//! - `std.Io`/allocator parameters are dropped, and with them the
//!   `BufferFirstAllocator` stack buffer the Zig encoder uses for small
//!   record payloads (an allocation-strategy detail).
//! - `Decoded.deinit` is `Drop`; the continuation is an owned
//!   `continuation::OwnedValue`.
//! - `std.EnumMap`/`EnumArray`/`EnumSet` keyed by the screen key are arrays
//!   indexed by the key.
//! - `Decoder.Progress.Page`/`.Image` are [`ProgressPage`]/[`ProgressImage`].
//! - The test "complete snapshot decode allocation failures are
//!   transactional" exercises Zig's `FailingAllocator` and has no Rust
//!   counterpart (allocation failure aborts).

use super::checkpoint;
use super::continuation;
use super::envelope;
use super::history;
use super::io::{self, Error};
use super::kitty;
use super::page;
use super::record;
use super::screen;
use super::terminal;
use crate::terminal::build_options;
use crate::terminal::page_list::Allocator;
use crate::terminal::screen::{Options as ScreenOptions, Screen as TerminalScreen};
use crate::terminal::screen_set::{Key as TerminalScreenKey, ScreenPtr};
use crate::terminal::size;
use crate::terminal::terminal::Terminal;

/// Re-export continuation to make it a bit more ergonomic to reference.
pub type Continuation<'a> = continuation::Value<'a>;

/// Errors possible while encoding one complete terminal snapshot: the
/// union of the TERMINAL, SCREEN, HISTORY, Kitty graphics, checkpoint and
/// continuation encode errors.
pub type EncodeError = Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeOptions<'a> {
    pub continuation: Continuation<'a>,
}

const KEY_COUNT: usize = TerminalScreenKey::COUNT;

/// Borrow the screen for `key` for the lifetime of the terminal borrow.
fn screen_ref(t: &Terminal, key: TerminalScreenKey) -> Option<&TerminalScreen> {
    // SAFETY: the screen set owns its screens while `t` is borrowed.
    t.screens.get(key).map(|ptr| unsafe { &*ptr.as_ptr() })
}

/// Encode one complete terminal snapshot.
///
/// Encoding starts at the destination's current position. Only one record
/// payload is buffered at a time; completed records stream immediately. On
/// failure, the destination may contain a snapshot prefix without its required
/// FINISH marker, and an output failure may have written part of a record.
pub fn encode(destination: &mut dyn io::Writer, t: &Terminal, options: EncodeOptions<'_>) -> Result<(), EncodeError> {
    // Continuation errors must not emit even the snapshot envelope.
    continuation::validate(options.continuation)?;

    let mut stream = record::Writer::init(destination);

    // Plan each screen's Kitty graphics records up front. A screen without
    // images has no plan and gets no Kitty graphics records at all.
    let mut kitty_plans: [Option<kitty::Plan<'_>>; KEY_COUNT] = [None, None];
    if build_options::KITTY_GRAPHICS {
        for key in TerminalScreenKey::ALL {
            let Some(s) = screen_ref(t, key) else {
                continue;
            };
            kitty_plans[key as usize] = kitty::ScreenPlan::init(s, key);
        }
    }

    // 1. Envelope
    envelope::encode(stream.writer())?;

    // 2. Terminal
    terminal::encode(t, &mut stream)?;

    // 3. Each screen, followed by its Kitty graphics images and
    // placements. Screens may appear in any key order, so this visits
    // every key and skips screens that don't exist.
    for key in TerminalScreenKey::ALL {
        let Some(s) = screen_ref(t, key) else {
            continue;
        };
        screen::encode(s, key, &mut stream)?;
        if build_options::KITTY_GRAPHICS {
            if let Some(plan) = &kitty_plans[key as usize] {
                plan.encode_storage(&mut stream)?;
            }
        }
    }

    // 4. Standard Stream continuation.
    continuation::encode(options.continuation, &mut stream)?;

    // 5. Ready marker.
    checkpoint::encode(checkpoint::Kind::Ready, &mut stream)?;

    // Kitty graphics pixels are written in the order the reader needs
    // them, and the screen that is shown comes first in that order, so
    // what the reader can see arrives before what it cannot.
    let active_key = t.screens.active_key;
    let other_keys = TerminalScreenKey::ALL.into_iter().filter(|&key| key != active_key);

    // 6. The pixels of the Kitty graphics images in the shown screen's
    // viewport. They come after READY so the decoder can show the terminal
    // without waiting for them, and before history so the reader can draw
    // what is on screen as soon as possible.
    if build_options::KITTY_GRAPHICS {
        if let Some(plan) = &kitty_plans[active_key as usize] {
            plan.encode_viewport_payloads(&mut stream)?;
        }
    }

    // 7. Each screen's history, with the Kitty graphics placements pinned
    // to each page and the pixels of the images first placed there.
    for key in TerminalScreenKey::ALL {
        let Some(s) = screen_ref(t, key) else {
            continue;
        };
        history::encode(s, key, &mut stream, kitty_plans[key as usize].as_mut())?;
    }

    // 8. The pixels nothing is waiting on: images in the viewport of a
    // screen that is not shown, then every image that is not placed where
    // the reader can see it.
    if build_options::KITTY_GRAPHICS {
        for key in other_keys.clone() {
            if let Some(plan) = &kitty_plans[key as usize] {
                plan.encode_viewport_payloads(&mut stream)?;
            }
        }
        if let Some(plan) = &kitty_plans[active_key as usize] {
            plan.encode_remaining_payloads(&mut stream)?;
        }
        for key in other_keys {
            if let Some(plan) = &kitty_plans[key as usize] {
                plan.encode_remaining_payloads(&mut stream)?;
            }
        }
    }

    // 9. Finish
    checkpoint::encode(checkpoint::Kind::Finish, &mut stream)
}

/// Errors possible while restoring one complete terminal snapshot
/// (`Decoder.ReadyError || Decoder.NextError`).
pub type DecodeError = Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecodeOptions {
    /// Largest non-ground continuation the decoder may allocate and return.
    /// Set this to zero when only ground-state snapshots are acceptable.
    pub max_continuation_bytes: usize,

    /// Compress each history page as soon as it is restored.
    ///
    /// Without this, restored history stays uncompressed until the caller
    /// runs `Terminal.compress`. With it, the decode holds at most one
    /// uncompressed history page at a time and the restored terminal starts
    /// out compressed.
    ///
    /// A page that is on screen when it is restored stays uncompressed.
    /// This only happens with `Decoder` if the caller scrolls the viewport
    /// to the top of the scrollback between `next` calls.
    ///
    /// The snapshot format is unchanged, so this works with any snapshot.
    /// It has no effect on platforms that do not support compression.
    pub compress_history: bool,

    /// The Kitty graphics image storage limit, in bytes, for each restored
    /// screen, or null for the default limit of a new screen.
    ///
    /// The limit of the terminal that wrote the snapshot is not part of the
    /// snapshot, so the decoder always applies this one. Images are restored
    /// oldest first, and an image that does not fit evicts older images,
    /// exactly as a live transmission would. The newest images are kept.
    /// Zero disables Kitty graphics on the restored terminal.
    pub kitty_image_storage_limit: Option<usize>,
}

/// One decoded Terminal and the bytes needed to resume its Stream.
///
/// `decode` returns this with all history present; `Decoder::ready` returns
/// it at READY, when the terminal is complete for interactive use but its
/// scrollback is still arriving through `Decoder::next`.
///
/// A successful decode owns both values. Keep this result alive until the
/// Terminal's Stream has been restored. The usual restoration sequence is:
///
///   1. Inspect `continuation` and use its length to size continuation tracking.
///   2. Call `to_owned` once and store the returned Terminal at its final address.
///   3. Create the persistent, read-only standard TerminalStream for that
///      address. If `continuation` contains bytes, feed them exactly once and
///      verify that re-exporting the continuation returns the same bytes.
///   4. Drop this value to release the decoded continuation. The transferred
///      Terminal and its Stream remain owned by the caller.
///   5. Process PTY bytes that came after the snapshot cut.
///
/// Do not create a Stream against the address of `terminal` in this struct.
/// `to_owned` moves the Terminal, which would leave such a Stream pointing at
/// its old address.
pub struct Decoded {
    /// Present until `to_owned` transfers the Terminal to the caller. Callers
    /// may inspect it, but must transfer it before attaching a persistent
    /// TerminalStream.
    pub terminal: Option<Terminal>,

    /// For decoded results, nonempty bytes are owned and remain valid until
    /// this value is dropped. Ground needs no replay. Bytes must be replayed
    /// exactly once after the Terminal has reached its final address.
    pub continuation: continuation::OwnedValue,

    /// Advisory complete logical history extent declared by each restored
    /// screen, including the resident overlap already carried by that
    /// screen's own pages. A client may size scrollback UI from this at
    /// READY, before HISTORY pages arrive; native row totals always remain
    /// derived from the pages actually applied.
    pub history_rows: [Option<u64>; KEY_COUNT],

    /// For each screen, how many Kitty graphics images were restored and
    /// how many bytes of their pixels are still to come through
    /// `Decoder::next`. Screens without images, and every screen in a build
    /// without Kitty graphics, report zeros.
    ///
    /// This is meant for showing progress while the pixels arrive:
    ///
    /// ```ignore
    /// let total = decoded.kitty[Key::Primary as usize].pending_bytes;
    /// let mut received: u64 = 0;
    /// while let Some(progress) = decoder.next(&mut terminal)? {
    ///     if let Progress::Image(image) = progress {
    ///         if image.key == Key::Primary {
    ///             received += image.bytes as u64;
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// Pixels that the decoder drops report zero bytes, so `received` can
    /// end up below `total`. Pixels may arrive between history pages, so
    /// they are only known to be done once `next` returns null.
    pub kitty: [kitty::Counts; KEY_COUNT],
}

impl Decoded {
    /// Transfer the Terminal while retaining the continuation for replay.
    ///
    /// This may be called exactly once. Store the returned value directly at
    /// its final address before creating the TerminalStream that will replay
    /// `continuation`.
    #[allow(clippy::wrong_self_convention, clippy::should_implement_trait)]
    pub fn to_owned(&mut self) -> Terminal {
        self.terminal.take().unwrap()
    }

    /// The continuation bytes, or empty for ground (test convenience for
    /// Zig's `continuation.bytes`).
    pub fn continuation_bytes(&self) -> &[u8] {
        match &self.continuation {
            continuation::OwnedValue::Ground => &[],
            continuation::OwnedValue::Bytes(bytes) => bytes,
        }
    }
}

/// Incrementally restore one snapshot: renderable state first, then history.
///
/// The snapshot record order makes a terminal renderable at the READY
/// marker, after only a small prefix of the stream. `Decoder` exposes
/// that boundary: `ready` returns a complete, integrity-checked `Decoded` while
/// scrollback is still in flight, and each `next` call then applies one
/// history PAGE record off the critical path. `snapshot::decode` is the
/// one-shot form and is implemented over this type.
///
/// ```ignore
/// let mut decoder = snapshot::Decoder::init(&mut reader);
/// let mut decoded = decoder.ready(&DecodeOptions {
///     max_continuation_bytes: 1024 * 1024,
///     ..Default::default()
/// })?;
///
/// let mut terminal = decoded.to_owned();
/// // The terminal is usable now: restore its Stream from the decoded
/// // continuation and size scrollback UI from `decoded.history_rows`.
///
/// while let Some(_progress) = decoder.next(&mut terminal)? {
///     // Scrollback grew; e.g. refresh the scrollbar.
/// }
/// // FINISH validated: the complete snapshot has been decoded.
/// ```
///
/// The terminal is live between `next` calls: the caller may render it and
/// even feed it PTY bytes that arrived after the snapshot cut. `next`
/// re-validates its destination on every call and degrades to discarding,
/// consuming and validating wire bytes without applying them, when live
/// changes have made a page inapplicable. See `next` for the drop rules.
///
/// The decoder owns no allocations. It retains the source given to `init`;
/// the source must stay valid, and must not be read elsewhere, until
/// decoding ends. After any returned error both the decoder and the stream
/// position are invalid: abandon the decoder, and destroy an already
/// transferred Terminal or keep it with partial history, but do not resume.
pub struct Decoder<'a> {
    source: &'a mut dyn io::Reader,
    state: State,
}

enum State {
    /// `ready` has not been called.
    Start,

    /// READY validated; HISTORY sequences remain before FINISH.
    History(History),

    /// FINISH validated. `next` returns null.
    Finished,

    /// A call returned an error; the stream position is unknown.
    Failed,
}

struct History {
    /// The generation observed at READY for every screen the snapshot
    /// declared. Presence is the declaration check for HISTORY routing;
    /// the value detects screens removed or replaced by live terminal
    /// use between calls.
    generations: [Option<usize>; KEY_COUNT],

    /// Keys whose HISTORY sequence was already consumed.
    routed: [bool; KEY_COUNT],

    /// HISTORY sequences that have not begun streaming yet.
    pending: usize,

    /// The sequence currently streaming PAGE records.
    current: Option<Sequence>,

    /// Terminal width at READY. Encoded pages assume this width and are
    /// dropped rather than mixed into a screen reflowed to another.
    cols: size::CellCountInt,

    /// Options for each history page, taken from `ready`.
    page_options: history::DecodeOptions,

    /// The Kitty graphics image storage generation of each screen at
    /// READY. History placements only attach to images that were
    /// restored at READY. See `kitty::PageTarget.ready_generation`.
    kitty_generations: [Option<u64>; KEY_COUNT],

    /// A record header already read from the source but not decoded yet.
    /// After each history PAGE, the decoder reads the next header to see
    /// whether that page's KITTY_IMAGE_PAGE_PLACEMENTS follows. If it is
    /// some other record, its header waits here for the next call.
    pending_header: Option<record::Header>,

    /// An error from reading that next header. The page before it was
    /// already applied, so the call that applied it still reports the
    /// page, and the error is returned by the following call instead.
    pending_error: Option<record::NextHeaderError>,
}

#[derive(Debug, Clone, Copy)]
struct Sequence {
    key: TerminalScreenKey,

    /// PAGE records not yet consumed from this sequence.
    remaining: u32,

    /// False once any page in this sequence was dropped. Later pages
    /// are older, and applying one above a gap would corrupt scrollback
    /// order, so a single drop discards the remainder of the sequence.
    apply: bool,
}

/// Errors possible while decoding the renderable prefix through READY:
/// the Kitty graphics, envelope, TERMINAL, SCREEN, continuation and
/// checkpoint decode errors, plus:
///
/// - `UnexpectedScreenKey`: a SCREEN names a key not declared by TERMINAL.
/// - `DuplicateScreen`: more than one SCREEN names the same key.
pub type ReadyError = Error;

/// One record consumed by `next`.
///
/// ```ignore
/// while let Some(progress) = decoder.next(&mut terminal)? {
///     match progress {
///         Progress::Image(image) => log::debug!("image {} arrived", image.image_id),
///         Progress::Page(page) => log::debug!("{} rows of scrollback", page.rows),
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// A history PAGE, together with its Kitty graphics placements.
    Page(ProgressPage),

    /// The pixels of a Kitty graphics image or animation frame.
    Image(ProgressImage),
}

/// `Decoder.Progress.Page`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressPage {
    /// The screen whose HISTORY contained the page.
    pub key: TerminalScreenKey,

    /// Rows prepended above that screen's existing content, or zero
    /// when the page was consumed and validated but dropped.
    pub rows: usize,

    /// PAGE records still to come in the same HISTORY.
    pub remaining: u32,
}

/// `Decoder.Progress.Image`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressImage {
    /// The screen the image is on.
    pub key: TerminalScreenKey,

    /// The ID of the image the pixels are for.
    pub image_id: u32,

    /// The frame the pixels are for, as the Kitty graphics protocol
    /// counts frames. 1 is the image itself and 2 and up are its
    /// animation frames.
    pub frame: u32,

    /// The number of pixel bytes attached to the image, or zero
    /// when the pixels were consumed and validated but dropped.
    pub bytes: usize,
}

impl Progress {
    /// The page payload (Zig: `progress.page`, which asserts the tag).
    pub fn page(self) -> ProgressPage {
        match self {
            Progress::Page(page) => page,
            Progress::Image(_) => panic!("progress is not a page"),
        }
    }

    /// The image payload (Zig: `progress.image`, which asserts the tag).
    pub fn image(self) -> ProgressImage {
        match self {
            Progress::Image(image) => image,
            Progress::Page(_) => panic!("progress is not an image"),
        }
    }
}

/// Errors possible while decoding history records through FINISH: record
/// header, Kitty graphics, checkpoint, history and page errors, plus:
///
/// - `DecoderNotReady`: `next` was called before `ready` completed.
/// - `DecoderFailed`: a prior `next` call failed and invalidated the
///   stream position.
/// - `UnexpectedHistoryKey`: a HISTORY names a key not declared by TERMINAL.
/// - `DuplicateHistory`: more than one HISTORY names the same key.
/// - `InvalidPageDimensions`, `RowCountOverflow`, `PageSizeOverflow`: a
///   history PAGE cannot join a native PageList. These propagate from
///   `next_page`'s non-limit finalize errors; only limit failures are
///   deliberately converted into drops.
pub type NextError = Error;

impl<'a> Decoder<'a> {
    /// The decoder begins reading only when `ready` is called.
    pub fn init(source: &'a mut dyn io::Reader) -> Decoder<'a> {
        Decoder {
            source,
            state: State::Start,
        }
    }

    /// Decode through the READY marker and return the renderable state.
    ///
    /// The result is transactional and complete for interactive use: the
    /// Terminal, its continuation, and the advisory history extents are all
    /// validated through READY. Scrollback history is not present yet and
    /// arrives through `next`. Asserts this is the first call.
    ///
    /// The Terminal also has every Kitty graphics image from the snapshot,
    /// and the placements on its screens. The pixels arrive later through
    /// `next`, and the images are pending until then. Placements on history
    /// pages also arrive through `next`, together with their pages.
    /// `Decoded.kitty` says how many pixels are still to come.
    pub fn ready(&mut self, options: &DecodeOptions) -> Result<Decoded, ReadyError> {
        assert!(matches!(self.state, State::Start));
        let result = self.ready_inner(options);
        if result.is_err() {
            self.state = State::Failed;
        }
        result
    }

    fn ready_inner(&mut self, options: &DecodeOptions) -> Result<Decoded, ReadyError> {
        let reader: &mut dyn io::Reader = &mut *self.source;

        // Read the envelope, which is currently just a verification step.
        envelope::decode(reader)?;

        // TERMINAL establishes terminal-wide state and allocates empty
        // screen slots with their final routing. SCREEN values replace those
        // slots in place so ScreenSet pointers, including the active
        // pointer, stay valid.
        let header = record::next_header(reader)?;
        let result = terminal::decode(reader, header)?;

        // TERMINAL initializes exactly the number of screen slots it
        // declared. Decode that many SCREEN sequences and route each one by
        // its encoded key.
        let screen_count = result.screens.iter().count();
        let mut screen_options: ScreenOptions = {
            let primary = result.screens.get(TerminalScreenKey::Primary).unwrap();
            let explicit_bytes = primary.pages.limits.bytes.explicit;
            let explicit_lines = primary.pages.limits.lines.explicit;
            ScreenOptions {
                cols: result.cols,
                rows: result.rows,
                max_scrollback_bytes: if explicit_bytes == usize::MAX {
                    None
                } else {
                    Some(explicit_bytes)
                },
                max_scrollback_lines: if explicit_lines == usize::MAX {
                    None
                } else {
                    Some(explicit_lines)
                },
                ..ScreenOptions::default()
            }
        };
        if let Some(limit) = options.kitty_image_storage_limit {
            screen_options.kitty_image_storage_limit = limit;
        }

        // Each SCREEN sequence may be followed by that screen's optional
        // KITTY_IMAGE_STORAGE record. The header after it is read ahead to find
        // out, so each loop starts with the next record's header in hand.
        // The header left over at the end belongs to CONTINUATION.
        let mut routed = [false; KEY_COUNT];
        let mut history_rows: [Option<u64>; KEY_COUNT] = [None; KEY_COUNT];
        let mut header = record::next_header(reader)?;
        for _ in 0..screen_count {
            // Decode the screen and move it into the slot for its key.
            let key = {
                let decoded = screen::decode(reader, header, Allocator::GLOBAL, &screen_options)?;

                let Some(mut slot) = result.screens.get(decoded.key) else {
                    return Err(Error::UnexpectedScreenKey);
                };
                if routed[decoded.key as usize] {
                    return Err(Error::DuplicateScreen);
                }
                routed[decoded.key as usize] = true;
                history_rows[decoded.key as usize] = Some(decoded.history_rows);

                // Replacing the slot's value drops the placeholder screen.
                *slot = decoded.screen;
                decoded.key
            };

            // A KITTY_IMAGE_STORAGE record right after the screen holds its
            // images and placements.
            header = record::next_header(reader)?;
            if build_options::KITTY_GRAPHICS && header.tag == record::Tag::KITTY_IMAGE_STORAGE {
                let mut screen = result.screens.get(key).unwrap();
                kitty::decode_storage(reader, header, key, &mut screen)?;
                header = record::next_header(reader)?;
            }
        }

        // CONTINUATION is required after every active screen. Nonempty bytes
        // are owned locally until the READY prefix validates.
        let decoded_continuation = continuation::decode(reader, header, options.max_continuation_bytes)?;

        // READY marks the exact envelope-through-CONTINUATION boundary.
        let header = record::next_header(reader)?;
        checkpoint::decode(checkpoint::Kind::Ready, reader, header)?;

        // Record where history may be applied. The declared keys route the
        // HISTORY sequences, and the generations detect declared screens
        // that live terminal use destroys or replaces before their history
        // arrives.
        let mut generations: [Option<usize>; KEY_COUNT] = [None; KEY_COUNT];
        let mut kitty_generations: [Option<u64>; KEY_COUNT] = [None; KEY_COUNT];
        let mut kitty_counts: [kitty::Counts; KEY_COUNT] = [kitty::Counts::default(); KEY_COUNT];
        for (key, screen) in result.screens.iter() {
            generations[key as usize] = Some(result.screens.generation(key));
            if build_options::KITTY_GRAPHICS {
                let storage = &screen.kitty_images;
                kitty_generations[key as usize] = Some(storage.generation);
                kitty_counts[key as usize] = kitty::Counts::init(storage);
            }
        }

        self.state = State::History(History {
            generations,
            routed: [false; KEY_COUNT],
            pending: screen_count,
            current: None,
            cols: result.cols,
            page_options: history::DecodeOptions {
                compress: options.compress_history,
            },
            kitty_generations,
            pending_header: None,
            pending_error: None,
        });
        Ok(Decoded {
            terminal: Some(result),
            continuation: decoded_continuation,
            history_rows,
            kitty: kitty_counts,
        })
    }

    /// Decode one history PAGE record and prepend it to its screen in `t`,
    /// or decode the pixels of one Kitty graphics image or animation frame.
    ///
    /// `t` must be the Terminal restored by this decoder's `ready` call,
    /// wherever the caller now stores it. Returns null once FINISH has
    /// validated, which completes the snapshot; the source is
    /// left positioned exactly after it. Further calls return null.
    ///
    /// Wire validation stays strict: framing, CRCs, record order, routing,
    /// and both marker records are always enforced. Application is
    /// forgiving, because the terminal is live and may have changed since
    /// READY. A page is consumed but dropped, reported as zero rows, when:
    ///
    ///   * its screen was removed, or removed and recreated, since READY
    ///     (detected through ScreenSet generations);
    ///   * the terminal was resized since READY, since encoded pages assume
    ///     the encoded width;
    ///   * prepending it would exceed the screen's scrollback limits, e.g.
    ///     because post-cut output already consumed that budget, making
    ///     immediate eviction the only alternative.
    ///
    /// Once a page in a sequence is dropped, the rest of that sequence is
    /// discarded too: later pages are older, and applying one above a gap
    /// would corrupt scrollback ordering. History for other screens is
    /// unaffected.
    ///
    /// The Kitty graphics placements pinned to a page are restored by the
    /// same call as the page. When a page is dropped, its placements are
    /// dropped too.
    ///
    /// Kitty graphics pixels may come at any point, even between history
    /// pages, one image or frame per call. `next` reports them as
    /// `Progress::Image`. Like pages, they are never applied where they no
    /// longer belong. The pixels are consumed but dropped, reported as zero
    /// bytes, when:
    ///
    ///   * their screen was removed, or removed and recreated, since READY;
    ///   * live input deleted the image, or sent a new image with the same
    ///     ID, since READY.
    pub fn next(&mut self, t: &mut Terminal) -> Result<Option<Progress>, NextError> {
        match self.state {
            // `ready` must succeed before history exists to decode, and a
            // failed decoder no longer knows its stream position.
            State::Start => return Err(Error::DecoderNotReady),
            State::Failed => return Err(Error::DecoderFailed),
            State::Finished => return Ok(None),
            State::History(_) => {}
        }
        let result = self.next_inner(t);
        if result.is_err() {
            self.state = State::Failed;
        }
        result
    }

    fn next_inner(&mut self, t: &mut Terminal) -> Result<Option<Progress>, NextError> {
        let State::History(state) = &mut self.state else {
            unreachable!()
        };
        let source: &mut dyn io::Reader = &mut *self.source;

        loop {
            // Read the next header before deciding how to decode the
            // record. Unknown records before it were skipped.
            let header = take_header(state, source)?;

            // Kitty graphics pixels may come anywhere after READY, even
            // between the pages of a HISTORY sequence, in any screen order.
            if build_options::KITTY_GRAPHICS && header.tag == record::Tag::KITTY_IMAGE_PAYLOAD {
                return next_payload(state, source, t, header).map(Some);
            }

            // Stream the current sequence's next PAGE if one remains.
            if let Some(sequence) = state.current {
                if sequence.remaining > 0 {
                    return next_page(state, source, t, header).map(Some);
                }
                state.current = None;
            }

            // All sequences are consumed: FINISH ends the snapshot, leaving
            // trailing transport bytes unread.
            if state.pending == 0 {
                checkpoint::decode(checkpoint::Kind::Finish, source, header)?;
                if build_options::SLOW_RUNTIME_SAFETY {
                    for key in TerminalScreenKey::ALL {
                        if state.generations[key as usize].is_none() {
                            continue;
                        }
                        let Some(restored) = t.screens.get(key) else {
                            continue;
                        };
                        restored.pages.assert_integrity();
                        restored.assert_integrity();
                    }
                }
                self.state = State::Finished;
                return Ok(None);
            }

            // Begin the next HISTORY sequence. Keys route the sequences
            // order-independently, but each must name a declared screen and
            // may not repeat.
            state.pending -= 1;
            let manifest = history::Decoder::init(&mut *source, header)?;
            let key = manifest.header.key;
            let page_count = manifest.header.page_count;
            if state.generations[key as usize].is_none() {
                return Err(Error::UnexpectedHistoryKey);
            }
            if state.routed[key as usize] {
                return Err(Error::DuplicateHistory);
            }
            state.routed[key as usize] = true;
            state.current = Some(Sequence {
                key,
                remaining: page_count,
                apply: true,
            });
        }
    }
}

/// Return the next record header: the one a previous call read ahead,
/// or else the next one from the source. A failure from reading ahead
/// is returned here.
fn take_header(state: &mut History, source: &mut dyn io::Reader) -> Result<record::Header, NextError> {
    if let Some(err) = state.pending_error {
        return Err(err);
    }
    if let Some(header) = state.pending_header.take() {
        return Ok(header);
    }
    record::next_header(source)
}

/// The screen for `key` in `t`, if it is still the screen restored at
/// READY. Returns null if live input removed it, or removed it and
/// created a new one, since then.
fn restored_screen(state: &History, t: &Terminal, key: TerminalScreenKey) -> Option<ScreenPtr> {
    let restored = t.screens.get(key)?;
    if Some(t.screens.generation(key)) != state.generations[key as usize] {
        return None;
    }
    Some(restored)
}

/// Consume one KITTY_IMAGE_PAYLOAD record, attaching its pixels to their
/// image when it is still the one restored at READY and dropping them
/// otherwise.
fn next_payload(
    state: &mut History,
    source: &mut dyn io::Reader,
    t: &mut Terminal,
    header: record::Header,
) -> Result<Progress, NextError> {
    debug_assert!(build_options::KITTY_GRAPHICS);

    // Pixels may go to any screen restored at READY that live input
    // hasn't removed or replaced since.
    let mut targets: kitty::PayloadTargets<'_> = [None, None];
    for key in TerminalScreenKey::ALL {
        if state.generations[key as usize].is_none() {
            continue;
        }
        let Some(restored) = restored_screen(state, t, key) else {
            continue;
        };
        targets[key as usize] = Some(kitty::PayloadTarget {
            // SAFETY: each key names a distinct live screen owned by `t`,
            // which is exclusively borrowed for this call.
            screen: unsafe { restored.as_mut() },
            ready_generation: state.kitty_generations[key as usize].unwrap(),
        });
    }

    let payload = kitty::decode_payload(source, header, &mut targets)?;
    if state.generations[payload.key as usize].is_none() {
        return Err(Error::UnexpectedKittyKey);
    }
    Ok(Progress::Image(ProgressImage {
        key: payload.key,
        image_id: payload.image_id,
        frame: payload.frame,
        bytes: payload.bytes,
    }))
}

/// Consume one PAGE record from the current sequence, whose header
/// `next` already read, applying it to the live terminal when it is
/// still applicable and discarding it otherwise. The page's Kitty
/// graphics placements are restored with it, or dropped with it.
fn next_page(
    state: &mut History,
    source: &mut dyn io::Reader,
    t: &mut Terminal,
    header: record::Header,
) -> Result<Progress, NextError> {
    let mut sequence = state.current.unwrap();

    // Application is re-decided on every page so live terminal changes
    // between calls degrade to discarding instead of failing decoding.
    let destination: Option<ScreenPtr> = if !sequence.apply || t.cols != state.cols {
        None
    } else {
        restored_screen(state, t, sequence.key)
    };

    sequence.remaining -= 1;
    let (applied, rows): (Option<ScreenPtr>, usize) = match destination {
        None => {
            // The record must still be consumed so decoding remains
            // aligned with the following records and FINISH marker.
            sequence.apply = false;
            state.current = Some(sequence);
            page::discard(source, header)?;
            (None, 0)
        }
        Some(mut restored) => {
            // The one-shot history decoder rejects a Screen which already has
            // complete history. This incremental path intentionally bypasses
            // that guard: the terminal has been live since READY and may have
            // accumulated new history. Generation and width checks above keep
            // the destination compatible, while per-page finalization enforces
            // its current scrollback limits.
            state.current = Some(sequence);
            match history::decode_page(source, header, &mut restored, state.page_options) {
                Ok(rows) => (Some(restored), rows),
                // The page no longer fits the screen's scrollback limits,
                // e.g. because post-cut output consumed the budget, so it
                // would be evicted immediately anyway. Its record was fully
                // consumed, leaving the stream aligned on the next record.
                Err(Error::MaxSizeExceeded | Error::MaxLinesExceeded) => {
                    sequence.apply = false;
                    state.current = Some(sequence);
                    (None, 0)
                }
                Err(other) => return Err(other),
            }
        }
    };

    let progress = Progress::Page(ProgressPage {
        key: sequence.key,
        rows,
        remaining: sequence.remaining,
    });

    // The page's Kitty graphics placements, if any, follow it. Read the
    // next header to find out. Anything else waits for the next call,
    // and so does a failure to read it, because this page is already
    // applied and its progress should not be lost.
    let following = match record::next_header(source) {
        Ok(header) => header,
        Err(err) => {
            state.pending_error = Some(err);
            return Ok(progress);
        }
    };
    if build_options::KITTY_GRAPHICS && following.tag == record::Tag::KITTY_IMAGE_PAGE_PLACEMENTS {
        // The page was just added at the top of the screen, which
        // makes it the first page of the screen's page list.
        let target = applied.map(|s| {
            let page = s.pages.pages.first.unwrap();
            kitty::PageTarget {
                // SAFETY: the screen is live and owned by `t`, which is
                // exclusively borrowed for this call.
                screen: unsafe { s.as_mut() },
                page,
                ready_generation: state.kitty_generations[sequence.key as usize].unwrap(),
            }
        });
        kitty::decode_page_placements(source, following, sequence.key, target)?;
        return Ok(progress);
    }

    state.pending_header = Some(following);
    Ok(progress)
}

/// Restore one complete snapshot into a native Terminal and continuation.
///
/// This consumes one snapshot through FINISH and leaves any following bytes
/// in the reader for the containing transport. Restoration is transactional:
/// the returned result is either complete and ready or not (error return).
/// Individual record codecs normalize optional semantic state, and history
/// beyond the declared scrollback limits is dropped rather than rejected,
/// while framing, markers, declared sequence counts, and unique
/// cross-record screen routing remain strict.
///
/// This is the one-shot form of `Decoder`, which can additionally hand the
/// renderable terminal to the caller at READY, before history arrives.
pub fn decode(source: &mut dyn io::Reader, options: &DecodeOptions) -> Result<Decoded, DecodeError> {
    let mut decoder = Decoder::init(source);
    let mut result = decoder.ready(options)?;

    // The Terminal has not been transferred, so history applies in place.
    let t = result.terminal.as_mut().unwrap();
    while decoder.next(t)?.is_some() {}
    Ok(result)
}

/// Errors possible while restoring a snapshot that must end at end-of-file:
/// decode and read errors, plus `TrailingData` when FINISH was followed by
/// additional bytes.
pub type DecodeExactError = Error;

/// Restore one snapshot and require FINISH to be followed by end-of-file.
///
/// This is intended for bounded snapshot files and buffers. On a live stream,
/// checking for end-of-file may block; use `decode` to stop at FINISH instead.
pub fn decode_exact(source: &mut dyn io::Reader, options: &DecodeOptions) -> Result<Decoded, DecodeExactError> {
    let result = decode(&mut *source, options)?;
    match source.peek_byte() {
        Err(Error::EndOfStream) => Ok(result),
        Err(err) => Err(err),
        Ok(_) => Err(Error::TrailingData),
    }
}

/// Build a native Screen around `pages` with every other field at its
/// default value, with the cursor at the active area's top-left (Zig
/// tests: a `Screen` struct literal). Shared by the snapshot test suites.
#[cfg(test)]
pub(crate) fn test_screen_from_pages(mut pages: crate::terminal::page_list::PageList) -> TerminalScreen {
    use crate::terminal::point::{Coordinate, Point};
    use crate::terminal::screen::{CharsetState, Cursor, Dirty, SemanticPrompt};

    let cursor_pin = pages
        .track_pin(pages.pin(Point::Active(Coordinate::default())).unwrap())
        .unwrap();
    // SAFETY: the pin was just tracked in `pages`.
    let cursor_rac = unsafe { cursor_pin.as_ref() }.row_and_cell();
    TerminalScreen {
        pages,
        no_scrollback: false,
        cursor: Cursor::new(cursor_pin, cursor_rac.row, cursor_rac.cell),
        saved_cursor: None,
        selection: None,
        charset: CharsetState::default(),
        protected_mode: crate::terminal::ansi::ProtectedMode::Off,
        kitty_keyboard: Default::default(),
        kitty_images: Default::default(),
        semantic_prompt: SemanticPrompt::DISABLED,
        dirty: Dirty::default(),
    }
}
