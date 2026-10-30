//! Port of `src/terminal/c/terminal.zig`.
//!
//! STATUS: ported
//!
//! Notes on the port:
//! - The wrapper owns the native terminal through a raw pointer
//!   (`Box::into_raw`) and the persistent stream's handler holds the only
//!   `&'static mut` to it (Zig: both hold `*Terminal`). Every access to
//!   the native terminal goes through the handler
//!   ([`TerminalWrapper::terminal`]/[`TerminalWrapper::terminal_mut`]).
//! - Effect trampolines recover the wrapper from the handler with an
//!   offset computation, like Zig's `@fieldParentPtr`.
//! - Enum arguments (`option`, `data`, `mode`, point tags) cross the ABI as
//!   `c_int` and are validated (Zig checks them with `std.enums.fromInt`
//!   in safe builds). Callback options take the function pointer itself as
//!   the value, like Zig.
//! - C enums that mirror core enums with a different representation
//!   (`TerminalScreen`, `ProgressState`, `ProgramStatusState`,
//!   `SemanticPromptKind`, ...) are declared here with `c_api_enum!` and
//!   converted at the boundary.
//! - Allocation failure is not recoverable in Rust; the Zig paths that
//!   report `out_of_memory` for internal allocations keep their result
//!   codes where a Rust error exists (`setTitle`, `select`, ...).
//!   `continuation_alloc_io` returns a `Vec<u8>` (Zig: a slice owned by a
//!   caller-provided allocator).

use std::ffi::{c_int, c_void};
use std::mem::ManuallyDrop;

use super::color_scheme as color_scheme_c;
use super::grid_ref as grid_ref_c;
use super::grid_ref_tracked as grid_ref_tracked_c;
use super::io as c_io;
use super::kitty_graphics as kitty_gfx_c;
use super::result::Result;
use super::search as search_c;
use super::selection as selection_c;
use super::style as style_c;
use super::types::HandleAllocator;
use crate::lib::allocator::{self as lib_alloc, Allocator as CAllocator, ZigAllocator};
use crate::lib::String as LibString;
use crate::lib::TinyIo;
use crate::terminal::build_options;
use crate::terminal::clipboard;
use crate::terminal::color;
use crate::terminal::cursor::Style as CursorStyle;
use crate::terminal::device_attributes;
use crate::terminal::device_status;
use crate::terminal::kitty::clipboard as kitty_clipboard;
use crate::terminal::kitty::graphics_image::TemporaryFile;
use crate::terminal::mem as terminal_mem;
use crate::terminal::modes::{self, Mode, ModeTag, ModeTagBacking};
use crate::terminal::mouse;
use crate::terminal::osc;
use crate::terminal::osc::program_status;
use crate::terminal::osc::semantic_prompt as osc_sp;
use crate::terminal::page_list::{ScrollbarC, PageList};
use crate::terminal::point;
use crate::terminal::screen_set::Key as ScreenKey;
use crate::terminal::size::CellCountInt;
use crate::terminal::size_report;
use crate::terminal::snapshot::snapshot as snapshot_core;
use crate::terminal::stream::{self, Boundary as StreamBoundary, BoundarySet, WriteContinuationError};
use crate::terminal::stream_continuation::Tracker;
use crate::terminal::stream_terminal::{self, Handler};
use crate::terminal::terminal::{
    self as zig_terminal, CellSizePx, Resize, ResizeError, ScrollViewport as ZigScrollViewport,
    ScrollViewportC, ScrollViewportTag, Terminal as ZigTerminal,
};

const LOG_TARGET: &str = "terminal_c";

/// The persistent stream type of a C terminal.
type Stream = stream_terminal::Stream<'static>;

/// `std.Io.Dir.max_path_bytes` (Linux `PATH_MAX`).
const MAX_PATH_BYTES: usize = 4096;

/// C terminals do not retain replay bytes unless the embedding application
/// opts in through `GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES`.
pub const DEFAULT_CONTINUATION_MAX_BYTES: usize = 0;

/// Owns the `std.Io` implementation retained by every C terminal.
///
/// Snapshot decoding creates this before the native terminal exists and
/// transfers it into the final C wrapper after READY.
///
/// This is TinyIo on every target: it is stateless and supports exactly
/// the operations the terminal needs at a fraction of the code size of
/// `std.Io.Threaded` (see lib/TinyIo.zig), on POSIX and Windows alike.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Io {
    pub impl_: TinyIo,
}

impl Io {
    pub const INIT: Io = Io { impl_: TinyIo::INIT };

    /// Return the value passed to native terminal construction and decoding.
    pub fn io(self) -> TinyIo {
        self.impl_.io()
    }
}

/// Wrapper around ZigTerminal that tracks additional state for C API usage,
/// such as the persistent VT stream needed to handle escape sequences split
/// across multiple vt_write calls.
pub struct TerminalWrapper {
    /// The heap-allocated native terminal (`Box::into_raw`). Only used to
    /// free it; every access goes through `stream.handler.terminal`.
    terminal_ptr: *mut ZigTerminal,
    /// C construction has no I/O argument, so the wrapper retains the owner
    /// created by `new` or transferred from snapshot decoding until `free`.
    pub io: Io,
    /// Owned copy of the temporary directory path for some operations
    /// (e.g. kitty graphics). This is only allocated once the embedder sets
    /// the option.
    tmp_dir_path: std::option::Option<Vec<u8>>,
    /// The terminfo name reported for XTGETTCAP "TN". The stream handler holds
    /// a slice into this.
    terminfo_name_buf: [u8; Handler::MAX_TERMINFO_NAME_BYTES],
    pub stream: ManuallyDrop<Stream>,
    pub effects: Effects,
    pub tracked_grid_refs: Vec<*mut grid_ref_tracked_c::TrackedGridRef>,
    pub searches: Vec<*mut search_c::SearchWrapper>,
    alloc: HandleAllocator,
}

impl TerminalWrapper {
    /// The native terminal.
    pub fn terminal(&self) -> &ZigTerminal {
        self.stream.handler.terminal
    }

    /// The native terminal (mutable).
    pub fn terminal_mut(&mut self) -> &mut ZigTerminal {
        self.stream.handler.terminal
    }

    /// The allocator this terminal was created with (Zig: `t.gpa()`).
    pub fn alloc(&self) -> HandleAllocator {
        self.alloc
    }

    /// Fetches a `TerminalWrapper` pointer from a `Handler`
    /// (Zig: `@fieldParentPtr`).
    fn from_handler(handler: &mut Handler<'_>) -> *mut TerminalWrapper {
        let offset = std::mem::offset_of!(TerminalWrapper, stream) + std::mem::offset_of!(Stream, handler);
        (handler as *mut Handler<'_> as *mut u8).wrapping_sub(offset) as *mut TerminalWrapper
    }
}

impl Drop for TerminalWrapper {
    fn drop(&mut self) {
        self.stream.deinit();
        // SAFETY: the stream (whose handler borrows the terminal) is
        // dropped before the terminal it borrows, exactly once.
        unsafe {
            ManuallyDrop::drop(&mut self.stream);
            drop(Box::from_raw(self.terminal_ptr));
        }
    }
}

/// A single MIME representation in a clipboard write.
///
/// C: GhosttyClipboardContent
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ClipboardContent {
    pub mime: LibString,
    pub data: LibString,
}

impl ClipboardContent {
    const EMPTY: ClipboardContent = ClipboardContent {
        mime: LibString {
            ptr: std::ptr::null(),
            len: 0,
        },
        data: LibString {
            ptr: std::ptr::null(),
            len: 0,
        },
    };
}

/// A protocol-neutral request to replace or clear clipboard contents.
/// The embedder answers by calling `reply` with the request before the
/// callback returns.
///
/// C: GhosttyClipboardWrite
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ClipboardWrite {
    pub size: usize,
    pub location: clipboard::Location,
    pub contents: *const ClipboardContent,
    pub contents_len: usize,
    pub name: LibString,
    pub granted: bool,
    pub can_remember: bool,
    /// Terminal-owned reply state; opaque to the embedder.
    pub ctx: *const c_void,
    pub reply: ClipboardWriteReplyFn,
}

/// The reply to a clipboard write request.
///
/// C: GhosttyClipboardWriteReply
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ClipboardWriteReply {
    pub size: usize,
    pub result: clipboard::WriteStatus,
    pub remember: bool,
}

/// C function pointer type for replying to a clipboard write.
///
/// C: GhosttyClipboardWriteReplyFn
pub type ClipboardWriteReplyFn = unsafe extern "C" fn(*const ClipboardWrite, *const ClipboardWriteReply);

/// The reply to a clipboard read request.
///
/// C: GhosttyClipboardReadReply
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ClipboardReadReply {
    pub size: usize,
    pub result: clipboard::ReadStatus,
    pub contents: *const ClipboardContent,
    pub contents_len: usize,
    pub available: *const LibString,
    pub available_len: usize,
    pub remember: bool,
}

/// A synchronous request to read clipboard contents. The embedder answers
/// by calling `reply` with the request before the callback returns.
///
/// C: GhosttyClipboardRead
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ClipboardRead {
    pub size: usize,
    pub location: clipboard::Location,
    pub mimes: *const LibString,
    pub mimes_len: usize,
    pub list: bool,
    pub name: LibString,
    pub granted: bool,
    pub can_remember: bool,
    /// Terminal-owned reply state; opaque to the embedder.
    pub ctx: *const c_void,
    pub reply: ClipboardReadReplyFn,
}

/// C function pointer type for replying to a clipboard read.
///
/// C: GhosttyClipboardReadReplyFn
pub type ClipboardReadReplyFn = unsafe extern "C" fn(*const ClipboardRead, *const ClipboardReadReply);

/// A request to show a desktop notification.
///
/// C: GhosttyTerminalDesktopNotification
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DesktopNotification {
    pub size: usize,
    pub title: LibString,
    pub body: LibString,
}

crate::c_api_enum! {
    /// C: GhosttyTerminalProgressState
    /// (Zig: `osc.Command.ProgressReport.State`.)
    pub enum ProgressState {
        Remove = 0,
        Set = 1,
        Error = 2,
        Indeterminate = 3,
        Pause = 4,
    }
}

impl ProgressState {
    fn from_zig(s: osc::ProgressReportState) -> ProgressState {
        match s {
            osc::ProgressReportState::Remove => ProgressState::Remove,
            osc::ProgressReportState::Set => ProgressState::Set,
            osc::ProgressReportState::Error => ProgressState::Error,
            osc::ProgressReportState::Indeterminate => ProgressState::Indeterminate,
            osc::ProgressReportState::Pause => ProgressState::Pause,
        }
    }
}

/// A progress report emitted by the running program.
///
/// C: GhosttyTerminalProgressReport
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProgressReport {
    pub size: usize,
    pub state: ProgressState,
    pub progress: i8,
}

crate::c_api_enum! {
    /// C: GhosttyProgramStatusState
    /// (Zig: `osc.Command.ProgramStatus.State`.)
    pub enum ProgramStatusState {
        Idle = 0,
        Working = 1,
        Done = 2,
        Blocked = 3,
        Error = 4,
        Clear = 5,
    }
}

impl ProgramStatusState {
    fn from_zig(s: program_status::State) -> ProgramStatusState {
        match s {
            program_status::State::Idle => ProgramStatusState::Idle,
            program_status::State::Working => ProgramStatusState::Working,
            program_status::State::Done => ProgramStatusState::Done,
            program_status::State::Blocked => ProgramStatusState::Blocked,
            program_status::State::Error => ProgramStatusState::Error,
            program_status::State::Clear => ProgramStatusState::Clear,
        }
    }
}

crate::c_api_enum! {
    /// What a blocked program needs from the user. This matches
    /// `osc.Command.ProgramStatus.Kind` but adds `none` first, because C has
    /// no optional enums and a zeroed value must mean "no kind".
    ///
    /// C: GhosttyProgramStatusKind
    pub enum ProgramStatusKind {
        None = 0,
        Permission = 1,
        Question = 2,
        Auth = 3,
    }
}

/// A program status report (OSC 7501). Absent text is an empty string,
/// an absent kind is `none`, and absent progress is -1. See the header
/// for the meaning of each field.
///
/// C: GhosttyTerminalProgramStatus
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProgramStatus {
    pub size: usize,
    pub state: ProgramStatusState,
    pub kind: ProgramStatusKind,
    pub progress: i8,
    pub id: LibString,
    pub app: LibString,
    pub title: LibString,
    pub message: LibString,
}

crate::c_api_enum! {
    /// C: GhosttySemanticPromptKind
    /// (Zig: `Handler.SemanticPrompt.Kind`.)
    pub enum SemanticPromptKind {
        Invalid = 0,
        PromptStart = 1,
        InputStart = 2,
        OutputStart = 3,
        CommandEnd = 4,
    }
}

impl SemanticPromptKind {
    fn from_zig(k: stream_terminal::SemanticPromptKind) -> SemanticPromptKind {
        use stream_terminal::SemanticPromptKind as K;
        match k {
            K::Invalid => SemanticPromptKind::Invalid,
            K::PromptStart => SemanticPromptKind::PromptStart,
            K::InputStart => SemanticPromptKind::InputStart,
            K::OutputStart => SemanticPromptKind::OutputStart,
            K::CommandEnd => SemanticPromptKind::CommandEnd,
        }
    }
}

crate::c_api_enum! {
    /// C: GhosttySemanticPromptPromptKind
    /// (Zig: `Handler.SemanticPrompt.PromptKind`.)
    pub enum SemanticPromptPromptKind {
        Primary = 0,
        Right = 1,
        Continuation = 2,
        Secondary = 3,
    }
}

impl SemanticPromptPromptKind {
    fn from_zig(k: stream_terminal::SemanticPromptPromptKind) -> SemanticPromptPromptKind {
        use stream_terminal::SemanticPromptPromptKind as K;
        match k {
            K::Primary => SemanticPromptPromptKind::Primary,
            K::Right => SemanticPromptPromptKind::Right,
            K::Continuation => SemanticPromptPromptKind::Continuation,
            K::Secondary => SemanticPromptPromptKind::Secondary,
        }
    }
}

/// A shell integration event. See `Handler.SemanticPrompt`.
///
/// C: GhosttyTerminalSemanticPrompt
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SemanticPrompt {
    pub size: usize,
    pub kind: SemanticPromptKind,
    pub prompt_kind: SemanticPromptPromptKind,
    pub has_exit_code: bool,
    pub exit_code: i32,
    pub command: LibString,
    pub error: LibString,
}

/// A request to resize the window's text area to a size in rows and columns.
///
/// C: GhosttyTerminalResizeWindowGrid
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResizeWindowGrid {
    pub rows: u16,
    pub columns: u16,
}

crate::c_api_enum! {
    /// C: GhosttyTerminalResizeWindowTag
    pub enum ResizeWindowTag {
        Grid = 0,
    }
}

/// C: GhosttyTerminalResizeWindowValue
///
/// Padding: the largest variant is 4 bytes. Use [2]u64 (16 bytes)
/// so that other kinds of resize requests can be added without an
/// ABI break.
#[repr(C)]
#[derive(Clone, Copy)]
pub union ResizeWindowValue {
    pub grid: ResizeWindowGrid,
    pub _padding: [u64; 2],
}

/// A request from the running program to resize the window.
///
/// C: GhosttyTerminalResizeWindow (Zig: `ResizeWindow.C`)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ResizeWindow {
    pub tag: ResizeWindowTag,
    pub value: ResizeWindowValue,
}

impl ResizeWindow {
    /// `lib.TaggedUnion.cFieldRename`.
    pub fn c_field_rename(tag: ResizeWindowTag) -> std::option::Option<&'static str> {
        match tag {
            ResizeWindowTag::Grid => Some("grid"),
        }
    }
}

/// A borrowed unsupported string sequence.
///
/// C: GhosttyTerminalUnknownStringSequence
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UnknownStringSequence {
    pub truncated: bool,
    pub content: LibString,
}

/// An OSC sequence whose number is not implemented.
///
/// C: GhosttyTerminalUnknownOscSequence
pub type UnknownOscSequence = osc::UnknownC;

crate::c_api_enum! {
    /// C: GhosttyTerminalUnknownSequenceTag
    pub enum UnknownSequenceTag {
        Apc = 0,
        Osc = 1,
    }
}

/// C: GhosttyTerminalUnknownSequenceValue
///
/// A future borrowed CSI payload may need parameter, separator, and
/// intermediate arrays. Reserve 128 bytes so that representation and
/// other structured sequence types can be added without an ABI break.
#[repr(C)]
#[derive(Clone, Copy)]
pub union UnknownSequenceValue {
    pub apc: UnknownStringSequence,
    pub osc: UnknownOscSequence,
    pub _padding: [u64; 16],
}

/// An unsupported terminal sequence reported to the C callback.
///
/// C: GhosttyTerminalUnknownSequence (Zig: `UnknownSequence.C`)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UnknownSequence {
    pub tag: UnknownSequenceTag,
    pub value: UnknownSequenceValue,
}

impl UnknownSequence {
    /// `lib.TaggedUnion.cFieldRename`.
    pub fn c_field_rename(tag: UnknownSequenceTag) -> std::option::Option<&'static str> {
        match tag {
            UnknownSequenceTag::Apc => Some("apc"),
            UnknownSequenceTag::Osc => Some("osc"),
        }
    }
}

/// A terminal mode and boolean value used for mode configuration.
///
/// C: GhosttyTerminalModeConfig
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeConfig {
    pub mode: ModeTagBacking,
    pub value: bool,
}

impl ModeConfig {
    fn to_mode(self) -> std::option::Option<Mode> {
        let tag = ModeTag::from_bits(self.mode);
        modes::mode_from_int(tag.value, tag.ansi)
    }
}

/// C function pointer type for the write_pty callback.
pub type WritePtyFn = unsafe extern "C" fn(Terminal, *mut c_void, *const u8, usize);

/// C function pointer type for the bell callback.
pub type BellFn = unsafe extern "C" fn(Terminal, *mut c_void);

/// C function pointer type for the color_scheme callback.
/// Returns true and fills out_scheme if a color scheme is available,
/// or returns false to silently ignore the query.
pub type ColorSchemeFn = unsafe extern "C" fn(Terminal, *mut c_void, *mut c_int) -> bool;

/// C function pointer type for the enquiry callback.
/// Returns the response bytes. The memory must remain valid
/// until the callback returns.
pub type EnquiryFn = unsafe extern "C" fn(Terminal, *mut c_void) -> LibString;

/// C function pointer type for the xtversion callback.
/// Returns the version string (e.g. "ghostty 1.2.3"). The memory
/// must remain valid until the callback returns. An empty string
/// (len=0) causes the default "libghostty" to be reported.
pub type XtversionFn = unsafe extern "C" fn(Terminal, *mut c_void) -> LibString;

/// C function pointer type for the clipboard_write callback. The request
/// is borrowed for the callback duration and must be answered through
/// its reply function before the callback returns.
pub type ClipboardWriteFn = unsafe extern "C" fn(Terminal, *mut c_void, *const ClipboardWrite);

/// C function pointer type for the clipboard_read callback. The request
/// is borrowed for the callback duration and must be answered through
/// its reply function before the callback returns.
pub type ClipboardReadFn = unsafe extern "C" fn(Terminal, *mut c_void, *const ClipboardRead);

/// C function pointer type for the desktop_notification callback. The
/// request and its strings are borrowed for the callback duration.
pub type DesktopNotificationFn = unsafe extern "C" fn(Terminal, *mut c_void, *const DesktopNotification);

/// C function pointer type for the title_changed callback.
pub type TitleChangedFn = unsafe extern "C" fn(Terminal, *mut c_void);

/// C function pointer type for the render_hold callback.
pub type RenderHoldFn = unsafe extern "C" fn(Terminal, *mut c_void, bool);

/// C function pointer type for the resize_window callback.
pub type ResizeWindowFn = unsafe extern "C" fn(Terminal, *mut c_void, *const ResizeWindow);

/// C function pointer type for the pwd_changed callback.
pub type PwdChangedFn = unsafe extern "C" fn(Terminal, *mut c_void);

/// C function pointer type for the progress_report callback.
pub type ProgressReportFn = unsafe extern "C" fn(Terminal, *mut c_void, *const ProgressReport);

/// C function pointer type for the program_status callback. The report
/// and its strings are borrowed for the callback duration.
pub type ProgramStatusFn = unsafe extern "C" fn(Terminal, *mut c_void, *const ProgramStatus);

/// C function pointer type for the semantic_prompt callback. The event
/// and its strings are borrowed for the callback duration.
pub type SemanticPromptFn = unsafe extern "C" fn(Terminal, *mut c_void, *const SemanticPrompt);

/// C function pointer type for the reset callback.
pub type ResetFn = unsafe extern "C" fn(Terminal, *mut c_void);

/// C function pointer type for the unknown_sequence callback. The request
/// and its content are borrowed for the callback duration.
pub type UnknownSequenceFn = unsafe extern "C" fn(Terminal, *mut c_void, *const UnknownSequence);

/// C function pointer type for the size callback. Used by XTWINOPS queries
/// and VT-driven mode 2048 enable reports. Returns true and fills out_size
/// if size is available, or false to suppress the XTWINOPS response or
/// mode 2048 report.
pub type SizeFn = unsafe extern "C" fn(Terminal, *mut c_void, *mut size_report::Size) -> bool;

/// C function pointer type for the device_attributes callback.
/// Returns true and fills out_attrs if attributes are available,
/// or returns false to silently ignore the query.
pub type DeviceAttributesFn = unsafe extern "C" fn(Terminal, *mut c_void, *mut DeviceAttributes) -> bool;

/// C-compatible device attributes struct.
/// C: GhosttyDeviceAttributes (Zig: `Effects.CDeviceAttributes`)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceAttributes {
    pub primary: DeviceAttributesPrimary,
    pub secondary: DeviceAttributesSecondary,
    pub tertiary: DeviceAttributesTertiary,
}

/// C: GhosttyDeviceAttributesPrimary
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceAttributesPrimary {
    pub conformance_level: u16,
    pub features: [u16; 64],
    pub num_features: usize,
}

/// C: GhosttyDeviceAttributesSecondary
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceAttributesSecondary {
    pub device_type: u16,
    pub firmware_version: u16,
    pub rom_cartridge: u16,
}

/// C: GhosttyDeviceAttributesTertiary
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceAttributesTertiary {
    pub unit_id: u32,
}

/// C callback state for terminal effects.
///
/// Most trampolines are always installed on the stream handler. They check
/// these fields and do nothing when the C callback is null.
///
/// A few trampolines are only installed while their C callback is set,
/// because the stream handler behaves differently when the Zig effect is
/// null:
///
/// - `unknown_sequence`: unknown sequences are skipped without being
///   captured.
/// - `clipboard_write`: Kitty clipboard writes fail up front instead of
///   collecting data for a write that can never finish.
/// - `clipboard_read`: clipboard reads are denied.
/// - `program_status`: the OSC 7501 support query gets no reply, so
///   programs know not to send reports.
pub struct Effects {
    pub userdata: *mut c_void,
    pub write_pty: std::option::Option<WritePtyFn>,
    pub bell: std::option::Option<BellFn>,
    pub color_scheme: std::option::Option<ColorSchemeFn>,
    pub desktop_notification: std::option::Option<DesktopNotificationFn>,
    pub device_attributes_cb: std::option::Option<DeviceAttributesFn>,
    pub enquiry: std::option::Option<EnquiryFn>,
    pub xtversion: std::option::Option<XtversionFn>,
    pub title_changed: std::option::Option<TitleChangedFn>,
    pub pwd_changed: std::option::Option<PwdChangedFn>,
    pub progress_report: std::option::Option<ProgressReportFn>,
    pub program_status: std::option::Option<ProgramStatusFn>,
    pub semantic_prompt: std::option::Option<SemanticPromptFn>,
    pub reset: std::option::Option<ResetFn>,
    pub size_cb: std::option::Option<SizeFn>,
    pub clipboard_write: std::option::Option<ClipboardWriteFn>,
    pub clipboard_read: std::option::Option<ClipboardReadFn>,
    pub unknown_sequence: std::option::Option<UnknownSequenceFn>,
    pub render_hold: std::option::Option<RenderHoldFn>,
    pub resize_window: std::option::Option<ResizeWindowFn>,

    /// Scratch buffer for DA1 feature codes. The device attributes
    /// trampoline converts C feature codes into this buffer and returns
    /// a slice pointing into it. Storing it here ensures the slice
    /// remains valid after the trampoline returns, since the caller
    /// (`reportDeviceAttributes`) reads it before any re-entrant call.
    da_features_buf: [device_attributes::Feature; 64],
}

impl Default for Effects {
    fn default() -> Self {
        Effects {
            userdata: std::ptr::null_mut(),
            write_pty: None,
            bell: None,
            color_scheme: None,
            desktop_notification: None,
            device_attributes_cb: None,
            enquiry: None,
            xtversion: None,
            title_changed: None,
            pwd_changed: None,
            progress_report: None,
            program_status: None,
            semantic_prompt: None,
            reset: None,
            size_cb: None,
            clipboard_write: None,
            clipboard_read: None,
            unknown_sequence: None,
            render_hold: None,
            resize_window: None,
            da_features_buf: [device_attributes::Feature(0); 64],
        }
    }
}

/// Opaque context behind ClipboardWrite.ctx for the reply trampoline.
struct ClipboardWriteCtx<'a> {
    write: clipboard::Write<'a>,
}

/// Opaque context behind ClipboardRead.ctx for the reply trampoline.
struct ClipboardReadCtx<'a> {
    read: clipboard::Read<'a>,
}

impl Effects {
    /// The wrapper behind `handler` and its C callback state.
    ///
    /// SAFETY (all trampolines): the handler is the one embedded in a live
    /// `TerminalWrapper`; only the `effects` field is read through the
    /// recovered pointer.
    #[inline]
    fn wrapper(handler: &mut Handler<'_>) -> (*mut TerminalWrapper, &'static Effects) {
        let w = TerminalWrapper::from_handler(handler);
        // SAFETY: see above.
        (w, unsafe { &*std::ptr::addr_of!((*w).effects) })
    }

    fn write_pty_trampoline(handler: &mut Handler<'_>, data: &[u8]) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.write_pty else {
            return;
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, data.as_ptr(), data.len()) };
    }

    fn bell_trampoline(handler: &mut Handler<'_>) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.bell else {
            return;
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata) };
    }

    fn clipboard_write_trampoline(handler: &mut Handler<'_>, write: clipboard::Write<'_>) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.clipboard_write else {
            return write.reply(clipboard::WriteResult::Unsupported);
        };

        // Most protocols currently produce one representation, so keep that
        // path allocation-free while supporting arbitrary multi-MIME writes.
        let mut stack_contents = [ClipboardContent::EMPTY; 4];
        let mut heap_contents: Vec<ClipboardContent>;
        let contents: &mut [ClipboardContent] = if write.contents.len() <= stack_contents.len() {
            &mut stack_contents[..write.contents.len()]
        } else {
            heap_contents = vec![ClipboardContent::EMPTY; write.contents.len()];
            &mut heap_contents
        };

        for (c_content, content) in contents.iter_mut().zip(write.contents) {
            *c_content = ClipboardContent {
                mime: LibString::init(content.mime),
                data: LibString::init(content.data),
            };
        }

        let ctx = ClipboardWriteCtx { write };
        let request = ClipboardWrite {
            size: std::mem::size_of::<ClipboardWrite>(),
            location: write.location,
            contents: if !contents.is_empty() {
                contents.as_ptr()
            } else {
                std::ptr::null()
            },
            contents_len: contents.len(),
            name: LibString::init(write.name),
            granted: write.granted,
            can_remember: write.can_remember,
            ctx: &ctx as *const ClipboardWriteCtx<'_> as *const c_void,
            reply: Self::clipboard_write_reply_trampoline,
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, &request) };
    }

    unsafe extern "C" fn clipboard_write_reply_trampoline(
        request: *const ClipboardWrite,
        reply: *const ClipboardWriteReply,
    ) {
        let ctx = &*((*request).ctx as *const ClipboardWriteCtx<'_>);
        let write = ctx.write;
        let reply = &*reply;
        match reply.result {
            clipboard::WriteStatus::SUCCESS => write.reply(clipboard::WriteResult::Success(clipboard::WriteSuccess {
                remember: reply.remember,
            })),
            clipboard::WriteStatus::DENIED => write.reply(clipboard::WriteResult::Denied),
            clipboard::WriteStatus::BUSY => write.reply(clipboard::WriteResult::Busy),
            clipboard::WriteStatus::INVALID_DATA => write.reply(clipboard::WriteResult::InvalidData),
            clipboard::WriteStatus::IO_ERROR => write.reply(clipboard::WriteResult::IoError),
            _ => write.reply(clipboard::WriteResult::Unsupported),
        }
    }

    fn clipboard_read_trampoline(handler: &mut Handler<'_>, read: clipboard::Read<'_>) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.clipboard_read else {
            return;
        };

        let mimes: Vec<LibString> = read.mimes.iter().map(|m| LibString::init(m)).collect();

        let ctx = ClipboardReadCtx { read };
        let request = ClipboardRead {
            size: std::mem::size_of::<ClipboardRead>(),
            location: read.location,
            mimes: if !mimes.is_empty() {
                mimes.as_ptr()
            } else {
                std::ptr::null()
            },
            mimes_len: mimes.len(),
            list: read.list,
            name: LibString::init(read.name),
            granted: read.granted,
            can_remember: read.can_remember,
            ctx: &ctx as *const ClipboardReadCtx<'_> as *const c_void,
            reply: Self::clipboard_read_reply_trampoline,
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, &request) };
    }

    unsafe extern "C" fn clipboard_read_reply_trampoline(
        request: *const ClipboardRead,
        reply: *const ClipboardReadReply,
    ) {
        let ctx = &*((*request).ctx as *const ClipboardReadCtx<'_>);
        let read = ctx.read;
        let reply = &*reply;
        match reply.result {
            clipboard::ReadStatus::SUCCESS => {}
            clipboard::ReadStatus::DENIED => return read.reply(clipboard::ReadResult::Denied),
            clipboard::ReadStatus::BUSY => return read.reply(clipboard::ReadResult::Busy),
            clipboard::ReadStatus::IO_ERROR => return read.reply(clipboard::ReadResult::IoError),
            _ => return read.reply(clipboard::ReadResult::Unsupported),
        }

        let c_contents: &[ClipboardContent] = super::types::slice_or_empty(reply.contents, reply.contents_len);
        let c_available: &[LibString] = super::types::slice_or_empty(reply.available, reply.available_len);

        let contents: Vec<clipboard::Content<'_>> = c_contents
            .iter()
            .map(|c| clipboard::Content {
                mime: lib_string_slice(c.mime),
                data: lib_string_slice(c.data),
            })
            .collect();
        let available: Vec<&[u8]> = c_available.iter().map(|m| lib_string_slice(*m)).collect();

        read.reply(clipboard::ReadResult::Success(clipboard::ReadSuccess {
            contents: &contents,
            available: &available,
            remember: reply.remember,
        }));
    }

    fn desktop_notification_trampoline(handler: &mut Handler<'_>, notification: stream::ShowDesktopNotification<'_>) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.desktop_notification else {
            return;
        };
        let request = DesktopNotification {
            size: std::mem::size_of::<DesktopNotification>(),
            title: LibString::init(notification.title),
            body: LibString::init(notification.body),
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, &request) };
    }

    fn color_scheme_trampoline(handler: &mut Handler<'_>) -> std::option::Option<device_status::ColorScheme> {
        let (w, effects) = Self::wrapper(handler);
        let func = effects.color_scheme?;
        let mut scheme: c_int = 0;
        // SAFETY: C callback contract.
        if unsafe { func(w, effects.userdata, &mut scheme) } {
            return color_scheme_c::ColorScheme::from_int(scheme).map(|s| s.to_zig());
        }
        None
    }

    fn device_attributes_trampoline(handler: &mut Handler<'_>) -> device_attributes::Attributes {
        let w = TerminalWrapper::from_handler(handler);
        // SAFETY: see `wrapper`; the features buffer lives in the wrapper.
        let effects = unsafe { &mut *std::ptr::addr_of_mut!((*w).effects) };
        let Some(func) = effects.device_attributes_cb else {
            return device_attributes::Attributes::default();
        };

        // Get our attributes from the callback.
        let mut c_attrs = DeviceAttributes {
            primary: DeviceAttributesPrimary {
                conformance_level: 0,
                features: [0; 64],
                num_features: 0,
            },
            secondary: DeviceAttributesSecondary::default(),
            tertiary: DeviceAttributesTertiary::default(),
        };
        // SAFETY: C callback contract.
        if !unsafe { func(w, effects.userdata, &mut c_attrs) } {
            return device_attributes::Attributes::default();
        }

        // All our types are non-exhaustive newtypes, so any value is valid.
        let n = c_attrs.primary.num_features.min(64);
        for i in 0..n {
            effects.da_features_buf[i] = device_attributes::Feature(c_attrs.primary.features[i]);
        }

        // SAFETY: the buffer lives in the wrapper and is read by the caller
        // before any re-entrant call (see `da_features_buf`).
        let features: &'static [device_attributes::Feature] =
            unsafe { std::slice::from_raw_parts(effects.da_features_buf.as_ptr(), n) };
        device_attributes::Attributes {
            primary: device_attributes::Primary {
                conformance_level: device_attributes::ConformanceLevel(c_attrs.primary.conformance_level),
                features,
            },
            secondary: device_attributes::Secondary {
                device_type: device_attributes::DeviceType(c_attrs.secondary.device_type),
                firmware_version: c_attrs.secondary.firmware_version,
                rom_cartridge: c_attrs.secondary.rom_cartridge,
            },
            tertiary: device_attributes::Tertiary {
                unit_id: c_attrs.tertiary.unit_id,
            },
        }
    }

    fn enquiry_trampoline<'h>(handler: &'h mut Handler<'_>) -> &'h [u8] {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.enquiry else {
            return b"";
        };
        // SAFETY: C callback contract.
        let result = unsafe { func(w, effects.userdata) };
        if result.len == 0 {
            return b"";
        }
        // SAFETY: the memory remains valid until the callback returns.
        unsafe { std::slice::from_raw_parts(result.ptr, result.len) }
    }

    fn xtversion_trampoline<'h>(handler: &'h mut Handler<'_>) -> &'h [u8] {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.xtversion else {
            return b"";
        };
        // SAFETY: C callback contract.
        let result = unsafe { func(w, effects.userdata) };
        if result.len == 0 {
            return b"";
        }
        // SAFETY: the memory remains valid until the callback returns.
        unsafe { std::slice::from_raw_parts(result.ptr, result.len) }
    }

    fn title_changed_trampoline(handler: &mut Handler<'_>) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.title_changed else {
            return;
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata) };
    }

    fn render_hold_trampoline(handler: &mut Handler<'_>, held: bool) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.render_hold else {
            return;
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, held) };
    }

    fn pwd_changed_trampoline(handler: &mut Handler<'_>) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.pwd_changed else {
            return;
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata) };
    }

    fn progress_report_trampoline(handler: &mut Handler<'_>, report: osc::ProgressReport) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.progress_report else {
            return;
        };
        let c_report = ProgressReport {
            size: std::mem::size_of::<ProgressReport>(),
            state: ProgressState::from_zig(report.state),
            progress: report.progress.map_or(-1, |v| v as i8),
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, &c_report) };
    }

    fn program_status_trampoline(handler: &mut Handler<'_>, report: program_status::Report<'_>) {
        use program_status::options;

        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.program_status else {
            return;
        };

        // Both buffers hold the longest text the parser accepts, so
        // writing to them can't fail.
        let mut title_buf = [0u8; program_status::MAX_TITLE_BYTES];
        let mut title: &mut [u8] = &mut title_buf;
        report.write_text::<options::Title, _>(&mut title).expect("title fits");
        let title_len = program_status::MAX_TITLE_BYTES - title.len();
        let mut message_buf = [0u8; program_status::MAX_MSG_BYTES];
        let mut message: &mut [u8] = &mut message_buf;
        report.write_text::<options::Msg, _>(&mut message).expect("message fits");
        let message_len = program_status::MAX_MSG_BYTES - message.len();

        let c_report = ProgramStatus {
            size: std::mem::size_of::<ProgramStatus>(),
            state: ProgramStatusState::from_zig(report.state),
            kind: match report.read_option::<options::Kind>() {
                Some(program_status::Kind::Permission) => ProgramStatusKind::Permission,
                Some(program_status::Kind::Question) => ProgramStatusKind::Question,
                Some(program_status::Kind::Auth) => ProgramStatusKind::Auth,
                None => ProgramStatusKind::None,
            },
            progress: report.read_option::<options::Progress>().map_or(-1, |v| v as i8),
            id: LibString::init(report.read_option::<options::Id>().unwrap_or(b"")),
            app: LibString::init(report.read_option::<options::App>().unwrap_or(b"")),
            title: LibString::init(&title_buf[..title_len]),
            message: LibString::init(&message_buf[..message_len]),
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, &c_report) };
    }

    fn semantic_prompt_trampoline(handler: &mut Handler<'_>, event: stream_terminal::SemanticPrompt<'_>) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.semantic_prompt else {
            return;
        };
        let c_event = SemanticPrompt {
            size: std::mem::size_of::<SemanticPrompt>(),
            kind: SemanticPromptKind::from_zig(event.kind),
            prompt_kind: SemanticPromptPromptKind::from_zig(event.prompt_kind),
            has_exit_code: event.exit_code.is_some(),
            exit_code: event.exit_code.unwrap_or(0),
            command: LibString::init(event.command),
            error: LibString::init(event.err),
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, &c_event) };
    }

    fn reset_trampoline(handler: &mut Handler<'_>) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.reset else {
            return;
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata) };
    }

    fn resize_window_trampoline(handler: &mut Handler<'_>, request: stream_terminal::ResizeWindow) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.resize_window else {
            return;
        };
        let value = match request {
            stream_terminal::ResizeWindow::Grid(grid) => ResizeWindow {
                tag: ResizeWindowTag::Grid,
                value: ResizeWindowValue {
                    grid: ResizeWindowGrid {
                        rows: grid.rows,
                        columns: grid.columns,
                    },
                },
            },
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, &value) };
    }

    fn unknown_sequence_trampoline(handler: &mut Handler<'_>, sequence: stream_terminal::UnknownSequence<'_>) {
        let (w, effects) = Self::wrapper(handler);
        let Some(func) = effects.unknown_sequence else {
            return;
        };
        let value = match sequence {
            stream_terminal::UnknownSequence::Apc(apc_value) => UnknownSequence {
                tag: UnknownSequenceTag::Apc,
                value: UnknownSequenceValue {
                    apc: UnknownStringSequence {
                        truncated: apc_value.truncated,
                        content: LibString::init(apc_value.content),
                    },
                },
            },
            stream_terminal::UnknownSequence::Osc(osc_value) => UnknownSequence {
                tag: UnknownSequenceTag::Osc,
                value: UnknownSequenceValue { osc: osc_value.cval() },
            },
        };
        // SAFETY: C callback contract.
        unsafe { func(w, effects.userdata, &value) };
    }

    fn size_trampoline(handler: &mut Handler<'_>) -> std::option::Option<size_report::Size> {
        let (w, effects) = Self::wrapper(handler);
        let func = effects.size_cb?;
        let mut s = size_report::Size::default();
        // SAFETY: C callback contract.
        if unsafe { func(w, effects.userdata, &mut s) } {
            return Some(s);
        }
        None
    }
}

/// A `GhosttyString` as a slice.
fn lib_string_slice<'a>(s: LibString) -> &'a [u8] {
    // SAFETY: C API contract: `ptr` is valid for `len` bytes.
    unsafe { super::types::slice_or_empty(s.ptr, s.len) }
}

/// C: GhosttyTerminal
pub type Terminal = *mut TerminalWrapper;

crate::c_api_enum! {
    /// C: GhosttyTerminalCompressionMode
    /// (Zig: `ZigTerminal.CompressionMode`.)
    pub enum CompressionMode {
        Incremental = 0,
        Full = 1,
    }
}

impl CompressionMode {
    fn to_zig(self) -> zig_terminal::CompressionMode {
        match self {
            CompressionMode::Incremental => zig_terminal::CompressionMode::Incremental,
            CompressionMode::Full => zig_terminal::CompressionMode::Full,
        }
    }
}

crate::c_api_enum! {
    /// C: GhosttyTerminalCompressionResult
    /// (Zig: `ZigTerminal.CompressionResult`.)
    pub enum CompressionResult {
        Unsupported = 0,
        Pending = 1,
        Complete = 2,
    }
}

impl CompressionResult {
    fn from_zig(r: zig_terminal::CompressionResult) -> CompressionResult {
        match r {
            zig_terminal::CompressionResult::Unsupported => CompressionResult::Unsupported,
            zig_terminal::CompressionResult::Pending => CompressionResult::Pending,
            zig_terminal::CompressionResult::Complete => CompressionResult::Complete,
        }
    }
}

/// The native terminal of a C terminal handle.
///
/// # Safety
/// `terminal_` must be NULL or a live terminal handle.
pub unsafe fn zig_terminal<'a>(terminal_: Terminal) -> std::option::Option<&'a mut ZigTerminal> {
    Some(terminal_.as_mut()?.terminal_mut())
}

/// Convert a C `GhosttyPointTag`.
pub fn point_tag_from_int(v: c_int) -> std::option::Option<point::Tag> {
    [point::Tag::Active, point::Tag::Viewport, point::Tag::Screen, point::Tag::History]
        .into_iter()
        .find(|t| *t as c_int == v)
}

/// Attach the persistent C stream and I/O owner to a heap-stable terminal.
/// The terminal is dropped if wrapper allocation fails.
fn wrap(
    alloc: HandleAllocator,
    t: Box<ZigTerminal>,
    io: Io,
    continuation_max_bytes: usize,
) -> std::result::Result<Terminal, OutOfMemory> {
    let terminal_ptr = Box::into_raw(t);
    // SAFETY: the terminal is heap-stable and owned by the wrapper, which
    // drops the stream (holding this borrow) before the terminal.
    let t: &'static mut ZigTerminal = unsafe { &mut *terminal_ptr };

    // Trampolines are always installed so setting C callbacks later takes
    // effect immediately.
    let mut handler: Handler<'static> = t.vt_handler();
    handler.effects = stream_terminal::Effects {
        write_pty: Some(Effects::write_pty_trampoline),
        bell: Some(Effects::bell_trampoline),
        color_scheme: Some(Effects::color_scheme_trampoline),
        desktop_notification: Some(Effects::desktop_notification_trampoline),
        drag_and_drop: None,
        device_attributes: Some(Effects::device_attributes_trampoline),
        enquiry: Some(Effects::enquiry_trampoline),
        xtversion: Some(Effects::xtversion_trampoline),
        title_changed: Some(Effects::title_changed_trampoline),
        pwd_changed: Some(Effects::pwd_changed_trampoline),
        progress_report: Some(Effects::progress_report_trampoline),
        semantic_prompt: Some(Effects::semantic_prompt_trampoline),
        reset: Some(Effects::reset_trampoline),
        size: Some(Effects::size_trampoline),
        render_hold: Some(Effects::render_hold_trampoline),
        resize_window: Some(Effects::resize_window_trampoline),

        // Installed dynamically when the callback is set; see Effects.
        clipboard_write: None,
        clipboard_read: None,
        program_status: None,
    };

    let wrapper = TerminalWrapper {
        terminal_ptr,
        io,
        tmp_dir_path: None,
        terminfo_name_buf: [0; Handler::MAX_TERMINFO_NAME_BYTES],
        stream: ManuallyDrop::new(Stream::init(stream::Options {
            allocator: Some(osc::Allocator::GLOBAL),
            continuation_max_bytes: Some(continuation_max_bytes),
            ..stream::Options::new(handler)
        })),
        effects: Effects::default(),
        tracked_grid_refs: Vec::new(),
        searches: Vec::new(),
        alloc,
    };
    let ptr = alloc.create(wrapper);
    if ptr.is_null() {
        return Err(OutOfMemory);
    }
    Ok(ptr)
}

/// `error.OutOfMemory`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfMemory;

/// (Zig: `RestoreContinuationError`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreContinuationError {
    OutOfMemory,
    ContinuationDisabled,
    ContinuationUnavailable,
    InvalidContinuation,
}

/// Replay a decoded continuation exactly once into a newly created C terminal
/// and verify that the persistent stream exports the identical canonical
/// bytes. The terminal remains valid on error and may be freed normally.
///
/// # Safety
/// `terminal_` must be NULL or a live terminal handle.
pub unsafe fn restore_continuation(
    terminal_: Terminal,
    continuation: &[u8],
) -> std::result::Result<(), RestoreContinuationError> {
    let Some(wrapper) = terminal_.as_mut() else {
        return Err(RestoreContinuationError::InvalidContinuation);
    };
    if !continuation.is_empty() {
        wrapper.stream.next_slice(continuation);
    }

    let mut exported: Vec<u8> = Vec::new();
    wrapper.stream.write_continuation(&mut exported).map_err(|err| match err {
        WriteContinuationError::WriteFailed => RestoreContinuationError::OutOfMemory,
        WriteContinuationError::ContinuationDisabled => RestoreContinuationError::ContinuationDisabled,
        WriteContinuationError::ContinuationUnavailable => RestoreContinuationError::ContinuationUnavailable,
    })?;
    if continuation != exported.as_slice() {
        return Err(RestoreContinuationError::InvalidContinuation);
    }
    Ok(())
}

/// (Zig: `FromDecodedError`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FromDecodedError {
    OutOfMemory,
    InvalidContinuation,
}

/// Transfer a core snapshot result into a caller-owned C terminal.
///
/// `io` is the owner the decoded terminal was built with and is retained
/// by the returned wrapper. The decoded terminal is transferred only
/// after its final heap address has been allocated; its
/// continuation remains in `decoded` and is replayed before returning.
/// `continuation_max_bytes` selects the returned terminal's tracking policy:
/// zero uses a temporary exact-size tracker and restores the ordinary C
/// default before returning, while a nonzero value leaves tracking enabled
/// with that limit.
pub fn from_decoded(
    alloc: HandleAllocator,
    io: Io,
    decoded: &mut snapshot_core::Decoded,
    continuation_max_bytes: usize,
) -> std::result::Result<Terminal, FromDecodedError> {
    let native = Box::new(decoded.to_owned());

    let continuation = decoded.continuation_bytes();
    debug_assert!(continuation_max_bytes == 0 || continuation.len() <= continuation_max_bytes);

    // Without opt-in retention, non-ground state needs tracking only long
    // enough to verify that replay reconstructed the exact canonical
    // continuation. With retention, even ground state needs a tracker so its
    // continuation can be exported successfully as an empty slice.
    let tracker_max_bytes = if continuation_max_bytes > 0 {
        continuation_max_bytes
    } else {
        continuation.len()
    };
    let terminal = wrap(alloc, native, io, tracker_max_bytes).map_err(|_| FromDecodedError::OutOfMemory)?;

    if !continuation.is_empty() {
        // SAFETY: freshly created handle.
        if let Err(err) = unsafe { restore_continuation(terminal, continuation) } {
            // SAFETY: the handle is not used afterwards.
            unsafe { free(terminal) };
            return Err(match err {
                // The decoded bytes fit this fresh tracker's cap, so losing them
                // while replaying can only be an allocation failure.
                RestoreContinuationError::OutOfMemory | RestoreContinuationError::ContinuationUnavailable => {
                    FromDecodedError::OutOfMemory
                }
                RestoreContinuationError::ContinuationDisabled | RestoreContinuationError::InvalidContinuation => {
                    FromDecodedError::InvalidContinuation
                }
            });
        }
    }

    // Unless the decoder opted into retention, restore the same disabled
    // tracking default used by newly created C terminals. Disabling tracking
    // does not alter the parser or UTF-8 state reconstructed above.
    if continuation_max_bytes == 0 {
        // SAFETY: freshly created handle.
        set_continuation_max_bytes(unsafe { &mut *terminal }, DEFAULT_CONTINUATION_MAX_BYTES);
    }
    Ok(terminal)
}

/// `error{InvalidValue, OutOfMemory}`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewError {
    InvalidValue,
    OutOfMemory,
}

/// C: ghostty_terminal_new
#[export_name = "ghostty_terminal_new"]
pub extern "C" fn new(
    alloc_: std::option::Option<&CAllocator>,
    result: &mut Terminal,
    cols: CellCountInt,
    rows: CellCountInt,
) -> Result {
    match new_(alloc_, cols, rows) {
        Ok(t) => {
            *result = t;
            Result::Success
        }
        Err(err) => {
            *result = std::ptr::null_mut();
            match err {
                NewError::InvalidValue => Result::InvalidValue,
                NewError::OutOfMemory => Result::OutOfMemory,
            }
        }
    }
}

fn new_(alloc_: std::option::Option<&CAllocator>, cols: CellCountInt, rows: CellCountInt) -> std::result::Result<Terminal, NewError> {
    if cols == 0 || rows == 0 {
        return Err(NewError::InvalidValue);
    }

    let alloc = HandleAllocator::new(alloc_);
    let io = Io::INIT;

    // Setup our terminal
    let mut t = ZigTerminal::init(zig_terminal::Options {
        cols,
        rows,
        ..Default::default()
    })
    .map_err(|e| match e {
        zig_terminal::InitError::InvalidValue => NewError::InvalidValue,
        zig_terminal::InitError::OutOfMemory => NewError::OutOfMemory,
    })?;

    // libghostty-vt embedders don't necessarily install Ghostty's shell
    // integration, so don't assume OSC 133 prompts can be redrawn on resize.
    // Shells can still opt in with OSC 133;A;redraw=1.
    t.flags.shell_redraws_prompt = osc_sp::Redraw::False;

    wrap(alloc, Box::new(t), io, DEFAULT_CONTINUATION_MAX_BYTES).map_err(|_| NewError::OutOfMemory)
}

/// C: ghostty_terminal_vt_write
///
/// # Safety
/// `terminal_` must be NULL or live; `ptr` must be valid for `len` reads.
#[export_name = "ghostty_terminal_vt_write"]
pub unsafe extern "C" fn vt_write(terminal_: Terminal, ptr: *const u8, len: usize) {
    let Some(wrapper) = terminal_.as_mut() else {
        return;
    };
    wrapper.stream.next_slice(super::types::slice_or_empty(ptr, len));
}

crate::c_api_enum! {
    /// A boundary as a C bit flag. The C API takes and reports boundaries as a
    /// bitmask, so each value is the bit for the stream boundary of the same
    /// name.
    ///
    /// C: GhosttyTerminalBoundary
    pub enum Boundary {
        Ground = 1 << 0,
        ApcStart = 1 << 1,
        ApcEnd = 1 << 2,
        DcsStart = 1 << 3,
        DcsEnd = 1 << 4,
        OscStart = 1 << 5,
        OscEnd = 1 << 6,
    }
}

/// Every stream boundary in declaration order.
const STREAM_BOUNDARIES: [StreamBoundary; 7] = [
    StreamBoundary::Ground,
    StreamBoundary::ApcStart,
    StreamBoundary::ApcEnd,
    StreamBoundary::DcsStart,
    StreamBoundary::DcsEnd,
    StreamBoundary::OscStart,
    StreamBoundary::OscEnd,
];

// Each bit must match the stream boundary's position in the enum,
// so the conversions below can shift instead of switching.
const _: () = {
    assert!(Boundary::Ground as c_int == 1 << StreamBoundary::Ground as u8);
    assert!(Boundary::ApcStart as c_int == 1 << StreamBoundary::ApcStart as u8);
    assert!(Boundary::ApcEnd as c_int == 1 << StreamBoundary::ApcEnd as u8);
    assert!(Boundary::DcsStart as c_int == 1 << StreamBoundary::DcsStart as u8);
    assert!(Boundary::DcsEnd as c_int == 1 << StreamBoundary::DcsEnd as u8);
    assert!(Boundary::OscStart as c_int == 1 << StreamBoundary::OscStart as u8);
    assert!(Boundary::OscEnd as c_int == 1 << StreamBoundary::OscEnd as u8);
};

impl Boundary {
    const ALL: u32 = (1 << STREAM_BOUNDARIES.len()) - 1;

    /// Convert a bitmask from C to a set. Returns null if the mask is
    /// empty or has bits that aren't boundaries.
    fn set_from_bits(bits: u32) -> std::option::Option<BoundarySet> {
        if bits == 0 || bits & !Self::ALL != 0 {
            return None;
        }
        let mut result = BoundarySet::EMPTY;
        for b in STREAM_BOUNDARIES {
            if bits & Self::bit(b) != 0 {
                result.insert(b);
            }
        }
        Some(result)
    }

    /// Convert a set to a bitmask for C.
    fn bits_from_set(set_: BoundarySet) -> u32 {
        let mut bits: u32 = 0;
        for b in STREAM_BOUNDARIES {
            if set_.contains(b) {
                bits |= Self::bit(b);
            }
        }
        bits
    }

    fn bit(b: StreamBoundary) -> u32 {
        1u32 << (b as u8)
    }
}

crate::c_api_enum! {
    /// C: GhosttyTerminalStringKind (Zig: `StreamBoundary.StringKind`)
    pub enum StringKind {
        Apc = 0,
        Dcs = 1,
        Osc = 2,
    }
}

impl StringKind {
    fn from_zig(k: stream::StringKind) -> StringKind {
        match k {
            stream::StringKind::Apc => StringKind::Apc,
            stream::StringKind::Dcs => StringKind::Dcs,
            stream::StringKind::Osc => StringKind::Osc,
        }
    }
}

crate::c_api_enum! {
    /// C: GhosttyTerminalStringIntroducer (Zig: `StreamBoundary.Introducer`)
    pub enum StringIntroducer {
        Esc = 0,
        C1 = 1,
    }
}

impl StringIntroducer {
    fn from_zig(i: stream::Introducer) -> StringIntroducer {
        match i {
            stream::Introducer::Esc => StringIntroducer::Esc,
            stream::Introducer::C1 => StringIntroducer::C1,
        }
    }
}

crate::c_api_enum! {
    /// C: GhosttyTerminalStringEnd (Zig: `StreamBoundary.EndReason`)
    pub enum StringEnd {
        StEsc = 0,
        StC1 = 1 => "st_c1",
        Cancelled = 2,
        Aborted = 3,
        Bel = 4,
    }
}

impl StringEnd {
    fn from_zig(e: stream::EndReason) -> StringEnd {
        match e {
            stream::EndReason::StEsc => StringEnd::StEsc,
            stream::EndReason::StC1 => StringEnd::StC1,
            stream::EndReason::Cancelled => StringEnd::Cancelled,
            stream::EndReason::Aborted => StringEnd::Aborted,
            stream::EndReason::Bel => StringEnd::Bel,
        }
    }
}

/// Where `vt_write_until` stopped. This flattens `StreamBoundary.Stop`
/// for C. Fields that don't apply to a stop are zero, and `reached` tells
/// the caller which fields apply.
///
/// A stop can be both a string end and a string start, when an 8-bit
/// introducer of another kind arrives inside an APC or a DCS header. The
/// Zig stop carries a kind for each. This struct has one `kind` field, so
/// it holds the kind of the string that ended, and the start flag in
/// `reached` names the kind that started.
///
/// C: GhosttyTerminalWriteStop
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteStop {
    pub size: usize,
    pub consumed: usize,
    pub reached: u32,
    pub kind: StringKind,
    pub introducer: StringIntroducer,
    pub end: StringEnd,
    pub byte: u8,
}

impl Default for WriteStop {
    fn default() -> Self {
        WriteStop {
            size: std::mem::size_of::<WriteStop>(),
            consumed: 0,
            reached: 0,
            kind: StringKind::Apc,
            introducer: StringIntroducer::Esc,
            end: StringEnd::StEsc,
            byte: 0,
        }
    }
}

/// C: ghostty_terminal_vt_write_until
///
/// # Safety
/// `terminal_` must be NULL or live; a non-null `ptr_` must be valid for
/// `len` reads; a non-null `out_` must be valid for `out_.size` bytes.
#[export_name = "ghostty_terminal_vt_write_until"]
pub unsafe extern "C" fn vt_write_until(
    terminal_: Terminal,
    ptr_: *const u8,
    len: usize,
    boundaries: u32,
    out_: *mut WriteStop,
) -> Result {
    let Some(out) = out_.as_mut() else {
        return Result::InvalidValue;
    };
    if out.size < std::mem::size_of::<WriteStop>() {
        return Result::InvalidValue;
    }

    // Reset every field so a reused struct never carries a previous stop.
    *out = WriteStop::default();

    let Some(wrapper) = terminal_.as_mut() else {
        return Result::InvalidValue;
    };
    let input: &[u8] = if !ptr_.is_null() {
        super::types::slice_or_empty(ptr_, len)
    } else if len == 0 {
        b""
    } else {
        return Result::InvalidValue;
    };
    let Some(requested) = Boundary::set_from_bits(boundaries) else {
        return Result::InvalidValue;
    };

    let Some(stop) = wrapper.stream.next_slice_until(input, requested) else {
        out.consumed = len;
        return Result::NoValue;
    };

    out.consumed = stop.consumed;
    out.reached = Boundary::bits_from_set(stop.reached);
    if let Some(start) = stop.start {
        out.kind = StringKind::from_zig(start.kind);
        out.introducer = StringIntroducer::from_zig(start.introducer);
        out.byte = start.byte;
    }
    // When a byte both ends and starts a string, the end's kind wins;
    // see WriteStop.
    if let Some(end) = stop.end {
        out.kind = StringKind::from_zig(end.kind);
        out.end = StringEnd::from_zig(end.reason);
        out.byte = end.byte;
    }

    Result::Success
}

/// (Zig: `ContinuationWriteError`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationWriteError {
    InvalidValue,
    WriteFailed,
    ContinuationDisabled,
    ContinuationUnavailable,
}

impl From<WriteContinuationError> for ContinuationWriteError {
    fn from(err: WriteContinuationError) -> Self {
        match err {
            WriteContinuationError::WriteFailed => ContinuationWriteError::WriteFailed,
            WriteContinuationError::ContinuationDisabled => ContinuationWriteError::ContinuationDisabled,
            WriteContinuationError::ContinuationUnavailable => ContinuationWriteError::ContinuationUnavailable,
        }
    }
}

/// Write the exact replay-safe continuation for a C terminal to a writer.
/// Snapshot encoding uses this helper so it can preflight continuation state
/// without converting its allocator or writer through the C ABI.
///
/// # Safety
/// `terminal_` must be NULL or live.
pub unsafe fn continuation_write_io(
    terminal_: Terminal,
    writer: &mut dyn std::io::Write,
) -> std::result::Result<(), ContinuationWriteError> {
    // Keep handle validation here so the three public output forms and the
    // snapshot encoder all use exactly the same continuation preflight.
    let Some(wrapper) = terminal_.as_ref() else {
        return Err(ContinuationWriteError::InvalidValue);
    };

    // Stream owns the tracker and distinguishes disabled tracking from a
    // tracker that lost bytes after allocation failure or limit overflow.
    wrapper.stream.write_continuation(writer)?;
    Ok(())
}

/// (Zig: `ContinuationAllocError`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationAllocError {
    InvalidValue,
    OutOfMemory,
    ContinuationDisabled,
    ContinuationUnavailable,
}

/// Return an owned copy of the terminal's replay-safe continuation.
///
/// If `allow_untracked_ground` is true, disabled tracking is accepted only
/// when the stream is provably at ground and an empty buffer is returned.
///
/// # Safety
/// `terminal_` must be NULL or live.
pub unsafe fn continuation_alloc_io(
    terminal_: Terminal,
    allow_untracked_ground: bool,
) -> std::result::Result<Vec<u8>, ContinuationAllocError> {
    let Some(wrapper) = terminal_.as_ref() else {
        return Err(ContinuationAllocError::InvalidValue);
    };
    if allow_untracked_ground && wrapper.stream.continuation.is_none() && wrapper.stream.ground() {
        return Ok(Vec::new());
    }

    // The allocating writer gives snapshot encoding and the public allocation
    // API one common way to obtain an exact owned continuation.
    let mut aw: Vec<u8> = Vec::new();

    // A write failure from an allocating writer can only be allocation failure;
    // no external callback participates in this path.
    continuation_write_io(terminal_, &mut aw).map_err(|err| match err {
        ContinuationWriteError::InvalidValue => ContinuationAllocError::InvalidValue,
        ContinuationWriteError::WriteFailed => ContinuationAllocError::OutOfMemory,
        ContinuationWriteError::ContinuationDisabled => ContinuationAllocError::ContinuationDisabled,
        ContinuationWriteError::ContinuationUnavailable => ContinuationAllocError::ContinuationUnavailable,
    })?;
    Ok(aw)
}

/// Map errors that are intrinsic to continuation export. Public callback
/// failures receive finer classification in `continuation_write` below.
fn continuation_error_result(err: ContinuationWriteError) -> Result {
    match err {
        ContinuationWriteError::InvalidValue => Result::InvalidValue,
        ContinuationWriteError::WriteFailed => Result::IoError,
        ContinuationWriteError::ContinuationDisabled => Result::InvalidValue,
        ContinuationWriteError::ContinuationUnavailable => Result::InvalidValue,
    }
}

/// C: ghostty_terminal_continuation_write
///
/// # Safety
/// `terminal_` must be NULL or live; `writer` must follow the
/// `GhosttyWriter` contract.
#[export_name = "ghostty_terminal_continuation_write"]
pub unsafe extern "C" fn continuation_write(terminal_: Terminal, writer: c_io::Writer) -> Result {
    // Reject the missing callback before invoking the common helper so this is
    // classified as a bad argument rather than a write failure.
    if writer.write.is_none() {
        return Result::InvalidValue;
    }

    // The callback was validated above, so invalid_write can only mean output
    // accounting overflow. Keep that distinct from callback rejection.
    let mut buffer = [0u8; c_io::WriterAdapter::RECOMMENDED_BUFFER_LEN];
    let mut adapter = c_io::WriterAdapter::init_buffered(writer, &mut buffer);
    'write: {
        match continuation_write_io(terminal_, &mut adapter) {
            Ok(()) => {}
            Err(ContinuationWriteError::WriteFailed) => break 'write,
            Err(err) => return continuation_error_result(err),
        }
        if adapter.flush().is_err() {
            break 'write;
        }
        return Result::Success;
    }

    if adapter.invalid_write {
        return Result::LimitExceeded;
    }
    if adapter.callback_failed {
        return Result::IoError;
    }
    continuation_error_result(ContinuationWriteError::WriteFailed)
}

/// A writer that only counts bytes (Zig: `std.Io.Writer.Discarding`).
#[derive(Default)]
struct Discarding {
    count: usize,
}

impl std::io::Write for Discarding {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.count += data.len();
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// C: ghostty_terminal_continuation_buf
///
/// # Safety
/// `terminal_` must be NULL or live; a non-null `out_` must be valid for
/// `out_len` writes; a non-null `out_written_` must be writable.
#[export_name = "ghostty_terminal_continuation_buf"]
pub unsafe extern "C" fn continuation_buf(
    terminal_: Terminal,
    out_: *mut u8,
    out_len: usize,
    out_written_: *mut usize,
) -> Result {
    let Some(out_written) = out_written_.as_mut() else {
        return Result::InvalidValue;
    };
    // All failure paths leave deterministic output metadata.
    *out_written = 0;
    if out_.is_null() && out_len != 0 {
        return Result::InvalidValue;
    }

    if out_.is_null() {
        // A null/zero destination is the explicit size-query form. Discarding
        // runs the real exporter, so disabled or unavailable tracking is still
        // detected before a required length is reported.
        let mut discarding = Discarding::default();
        if let Err(err) = continuation_write_io(terminal_, &mut discarding) {
            return continuation_error_result(err);
        }
        *out_written = discarding.count;
        return Result::OutOfSpace;
    }

    // Fixed writers report WriteFailed when capacity is exhausted. The stream
    // exporter itself remains all-or-nothing from the API's perspective.
    let buf = std::slice::from_raw_parts_mut(out_, out_len);
    let mut writer: &mut [u8] = buf;
    match continuation_write_io(terminal_, &mut writer) {
        Ok(()) => {}
        Err(ContinuationWriteError::WriteFailed) => {
            // Re-run against a counter to return the full required capacity,
            // not merely the prefix that fit in the caller's buffer.
            let mut discarding = Discarding::default();
            if let Err(count_err) = continuation_write_io(terminal_, &mut discarding) {
                return continuation_error_result(count_err);
            }
            *out_written = discarding.count;
            return Result::OutOfSpace;
        }
        Err(err) => return continuation_error_result(err),
    }

    // The initialized prefix of the fixed destination.
    *out_written = out_len - writer.len();
    Result::Success
}

/// C: ghostty_terminal_continuation_alloc
///
/// # Safety
/// `terminal_` must be NULL or live; non-null output pointers must be
/// writable.
#[export_name = "ghostty_terminal_continuation_alloc"]
pub unsafe extern "C" fn continuation_alloc(
    terminal_: Terminal,
    alloc_: std::option::Option<&CAllocator>,
    out_ptr_: *mut *mut u8,
    out_len_: *mut usize,
) -> Result {
    let Some(out_ptr) = out_ptr_.as_mut() else {
        return Result::InvalidValue;
    };
    let Some(out_len) = out_len_.as_mut() else {
        return Result::InvalidValue;
    };
    // Make ownership unambiguous even if validation or allocation fails.
    *out_ptr = std::ptr::null_mut();
    *out_len = 0;

    let bytes = match continuation_alloc_io(terminal_, false) {
        Ok(bytes) => bytes,
        Err(err) => {
            return match err {
                ContinuationAllocError::InvalidValue => Result::InvalidValue,
                ContinuationAllocError::OutOfMemory => Result::OutOfMemory,
                ContinuationAllocError::ContinuationDisabled => Result::InvalidValue,
                ContinuationAllocError::ContinuationUnavailable => Result::InvalidValue,
            }
        }
    };

    // An idle parser has no continuation. Never export an empty slice's
    // dangling pointer to a foreign runtime (and never allocate for it).
    if bytes.is_empty() {
        return Result::Success;
    }

    // Ownership crosses the ABI here; callers release this exact pointer and
    // length with ghostty_free and the same allocator selection.
    let alloc = lib_alloc::default(alloc_);
    let ptr = alloc.alloc(bytes.len(), 0, 0);
    if ptr.is_null() {
        return Result::OutOfMemory;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    *out_ptr = ptr;
    *out_len = bytes.len();
    Result::Success
}

fn continuation_max_bytes(wrapper: &TerminalWrapper) -> usize {
    // Absence of a tracker is the public representation of disabled tracking.
    wrapper.stream.continuation.as_ref().map_or(0, |tracker| tracker.max_bytes)
}

/// Change continuation tracking policy without disturbing normal VT parser
/// state. Bytes which were not retained while disabled or after exceeding an
/// earlier cap cannot be reconstructed: export remains unavailable until a
/// later feed reaches ground or contains a new replay start.
fn set_continuation_max_bytes(wrapper: &mut TerminalWrapper, max_bytes: usize) {
    if max_bytes == 0 {
        // Disabling releases retained bytes immediately. Parser and UTF-8 state
        // continue normally; only future replay/export information is lost.
        if let Some(tracker) = &mut wrapper.stream.continuation {
            tracker.deinit();
        }
        wrapper.stream.continuation = None;
        return;
    }

    if let Some(tracker) = &mut wrapper.stream.continuation {
        // Changing a live cap preserves retained bytes when they still fit.
        tracker.max_bytes = max_bytes;
        if tracker.bytes.len() > max_bytes {
            // Once a retained prefix is discarded it cannot be reconstructed
            // from parser state alone. Mark it broken until Stream observes a
            // ground state or a new replay-safe sequence start.
            tracker.bytes.clear();
            tracker.broken = true;
        }
        return;
    }

    // Enabling from zero starts an empty tracker. It can immediately track
    // only if no earlier bytes are needed.
    wrapper.stream.continuation = Some(Tracker::init(osc::Allocator::GLOBAL, max_bytes));
    if !wrapper.stream.ground() {
        // The parser was already mid-sequence while tracking was disabled, so
        // exporting now would omit an unknown prefix.
        wrapper.stream.continuation.as_mut().unwrap().broken = true;
    }
}

/// C: ghostty_terminal_compression_activity
///
/// # Safety
/// `terminal_` must be NULL or live; a non-null `out_activity_` must be
/// writable.
#[export_name = "ghostty_terminal_compression_activity"]
pub unsafe extern "C" fn compression_activity(terminal_: Terminal, out_activity_: *mut u64) -> Result {
    let Some(t) = zig_terminal(terminal_) else {
        return Result::InvalidValue;
    };
    let Some(out_activity) = out_activity_.as_mut() else {
        return Result::InvalidValue;
    };
    *out_activity = t.compression_activity();
    Result::Success
}

/// C: ghostty_terminal_compress
///
/// # Safety
/// `terminal_` must be NULL or live; a non-null `out_result_` must be
/// writable.
#[export_name = "ghostty_terminal_compress"]
pub unsafe extern "C" fn compress(terminal_: Terminal, mode_: c_int, out_result_: *mut CompressionResult) -> Result {
    let Some(t) = zig_terminal(terminal_) else {
        return Result::InvalidValue;
    };
    let Some(out_result) = out_result_.as_mut() else {
        return Result::InvalidValue;
    };
    let Some(mode) = CompressionMode::from_int(mode_) else {
        return Result::InvalidValue;
    };
    *out_result = CompressionResult::from_zig(t.compress(mode.to_zig()));
    Result::Success
}

crate::c_api_enum! {
    /// C: GhosttyTerminalOption
    ///
    /// The input type of each option is documented in the header
    /// (Zig: `Option.InType`).
    pub enum Option {
        Userdata = 0,
        WritePty = 1,
        Bell = 2,
        Enquiry = 3,
        Xtversion = 4,
        TitleChanged = 5,
        Size = 6 => "size_cb",
        ColorScheme = 7,
        DeviceAttributes = 8,
        Title = 9,
        Pwd = 10,
        ColorForeground = 11,
        ColorBackground = 12,
        ColorCursor = 13,
        ColorPalette = 14,
        KittyImageStorageLimit = 15,
        KittyImageMediumFile = 16,
        KittyImageMediumTempFile = 17,
        KittyImageMediumSharedMem = 18,
        ApcMaxBytes = 19,
        ApcMaxBytesKitty = 20,
        Selection = 21,
        DefaultCursorStyle = 22,
        DefaultCursorBlink = 23,
        GlyphProtocol = 24,
        PwdChanged = 25,
        ClipboardWrite = 26,
        ScrollbackMaxBytes = 27,
        ScrollbackMaxLines = 28,
        DesktopNotification = 29,
        ProgressReport = 30,
        ContinuationMaxBytes = 31,
        TitleReport = 32,
        ModeDefault = 33,
        Mode = 34,
        UnknownSequence = 35,
        UnknownMaxBytes = 36,
        TerminfoName = 37,
        ClipboardRead = 38,
        ClipboardWriteMaxBytes = 39,
        ResizePullScrollback = 40,
        RenderHold = 41,
        SemanticPrompt = 42,
        Reset = 43,
        XtChecksumReport = 44,
        XtChecksumExtension = 45,
        ProgramStatus = 46,
        ResizeWindow = 47,
    }
}

/// C: ghostty_terminal_set
///
/// # Safety
/// `terminal_` must be NULL or live; `value` must be NULL or point to the
/// option's input type (for callback options, `value` is the function
/// pointer itself).
#[export_name = "ghostty_terminal_set"]
pub unsafe extern "C" fn set(terminal_: Terminal, option: c_int, value: *const c_void) -> Result {
    let Some(option) = Option::from_int(option) else {
        log::warn!(target: LOG_TARGET, "terminal_set invalid option value={option}");
        return Result::InvalidValue;
    };

    let Some(wrapper) = terminal_.as_mut() else {
        return Result::InvalidValue;
    };

    set_typed(wrapper, option, value)
}

/// Reinterpret `value` as an optional C function pointer.
unsafe fn func<F: Copy>(value: *const c_void) -> std::option::Option<F> {
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*const c_void>());
    if value.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy::<*const c_void, F>(&value))
    }
}

/// Read `value` as an optional `*const T`.
unsafe fn val<'a, T>(value: *const c_void) -> std::option::Option<&'a T> {
    (value as *const T).as_ref()
}

unsafe fn set_typed(wrapper: &mut TerminalWrapper, option: Option, value: *const c_void) -> Result {
    match option {
        Option::Userdata => wrapper.effects.userdata = value as *mut c_void,
        Option::WritePty => wrapper.effects.write_pty = func(value),
        Option::Bell => wrapper.effects.bell = func(value),
        Option::ColorScheme => wrapper.effects.color_scheme = func(value),
        Option::DesktopNotification => wrapper.effects.desktop_notification = func(value),
        Option::DeviceAttributes => wrapper.effects.device_attributes_cb = func(value),
        Option::Enquiry => wrapper.effects.enquiry = func(value),
        Option::Xtversion => wrapper.effects.xtversion = func(value),
        Option::TitleChanged => wrapper.effects.title_changed = func(value),
        Option::PwdChanged => wrapper.effects.pwd_changed = func(value),
        Option::ProgressReport => wrapper.effects.progress_report = func(value),
        Option::ResizeWindow => wrapper.effects.resize_window = func(value),
        Option::Size => wrapper.effects.size_cb = func(value),
        Option::RenderHold => wrapper.effects.render_hold = func(value),
        Option::SemanticPrompt => wrapper.effects.semantic_prompt = func(value),
        Option::Reset => wrapper.effects.reset = func(value),
        Option::ClipboardWrite => {
            wrapper.effects.clipboard_write = func(value);
            wrapper.stream.handler.effects.clipboard_write = if !value.is_null() {
                Some(Effects::clipboard_write_trampoline)
            } else {
                None
            };
        }
        Option::ClipboardRead => {
            wrapper.effects.clipboard_read = func(value);
            wrapper.stream.handler.effects.clipboard_read = if !value.is_null() {
                Some(Effects::clipboard_read_trampoline)
            } else {
                None
            };
        }
        Option::ProgramStatus => {
            // Installed dynamically because the terminal only answers the
            // OSC 7501 support query when something consumes reports.
            wrapper.effects.program_status = func(value);
            wrapper.stream.handler.effects.program_status = if !value.is_null() {
                Some(Effects::program_status_trampoline)
            } else {
                None
            };
        }
        Option::UnknownSequence => {
            wrapper.effects.unknown_sequence = func(value);
            wrapper.stream.handler.unknown_sequence = if !value.is_null() {
                Some(Effects::unknown_sequence_trampoline)
            } else {
                None
            };
        }
        Option::TitleReport => {
            wrapper.stream.handler.title_report = val::<bool>(value).copied().unwrap_or(false);
        }
        Option::XtChecksumReport => {
            wrapper.stream.handler.xt_checksum_report = val::<bool>(value).copied().unwrap_or(false);
        }
        Option::Title => {
            let str_ = val::<LibString>(value).map_or(&b""[..], |v| lib_string_slice(*v));
            if wrapper.terminal_mut().set_title(str_).is_err() {
                return Result::OutOfMemory;
            }
        }
        Option::Pwd => {
            let str_ = val::<LibString>(value).map_or(&b""[..], |v| lib_string_slice(*v));
            if wrapper.terminal_mut().set_pwd(str_).is_err() {
                return Result::OutOfMemory;
            }
        }
        Option::TerminfoName => {
            let str_ = val::<LibString>(value).map_or(&b""[..], |v| lib_string_slice(*v));
            if str_.len() > wrapper.terminfo_name_buf.len() {
                return Result::InvalidValue;
            }
            wrapper.terminfo_name_buf[..str_.len()].copy_from_slice(str_);
            wrapper.stream.handler.terminfo_name = if !str_.is_empty() {
                // SAFETY: the buffer lives in the heap-stable wrapper, which
                // outlives its stream.
                Some(std::slice::from_raw_parts(wrapper.terminfo_name_buf.as_ptr(), str_.len()))
            } else {
                None
            };
        }
        Option::ColorForeground => {
            let t = wrapper.terminal_mut();
            t.colors.foreground.default = val::<color::RgbC>(value).map(|v| color::Rgb::from_c(*v));
            t.flags.dirty.palette = true;
        }
        Option::ColorBackground => {
            let t = wrapper.terminal_mut();
            t.colors.background.default = val::<color::RgbC>(value).map(|v| color::Rgb::from_c(*v));
            t.flags.dirty.palette = true;
        }
        Option::ColorCursor => {
            let t = wrapper.terminal_mut();
            t.colors.cursor.default = val::<color::RgbC>(value).map(|v| color::Rgb::from_c(*v));
            t.flags.dirty.palette = true;
        }
        Option::ColorPalette => {
            let t = wrapper.terminal_mut();
            t.colors.palette.change_default(match val::<color::PaletteC>(value) {
                Some(v) => color::palette_zval(v),
                None => color::DEFAULT,
            });
            t.flags.dirty.palette = true;
        }
        Option::KittyImageStorageLimit => {
            if !build_options::KITTY_GRAPHICS {
                return Result::Success;
            }
            let limit: usize = val::<u64>(value).map_or(0, |v| *v as usize);
            let t = wrapper.terminal_mut();
            for (_, mut screen) in t.screens.iter() {
                let screen = &mut *screen;
                screen.kitty_images.set_limit(&mut screen.pages, limit);
            }
        }
        Option::KittyImageMediumFile | Option::KittyImageMediumSharedMem => {
            if !build_options::KITTY_GRAPHICS {
                return Result::Success;
            }
            let Some(&v) = val::<bool>(value) else {
                return Result::Success;
            };
            let t = wrapper.terminal_mut();
            for (_, mut screen) in t.screens.iter() {
                match option {
                    Option::KittyImageMediumFile => screen.kitty_images.image_limits.file = v,
                    Option::KittyImageMediumSharedMem => screen.kitty_images.image_limits.shared_memory = v,
                    _ => unreachable!(),
                }
            }
        }
        Option::KittyImageMediumTempFile => {
            if !build_options::KITTY_GRAPHICS {
                return Result::Success;
            }
            if let Some(v) = val::<LibString>(value) {
                if v.len > MAX_PATH_BYTES {
                    return Result::OutOfMemory;
                }
                let path = lib_string_slice(*v).to_vec();
                let t = wrapper.terminal_mut();
                for (_, mut screen) in t.screens.iter() {
                    screen.kitty_images.image_limits.temporary_file = TemporaryFile::Enabled {
                        directory: path.clone(),
                    };
                }

                // Every screen points at the new copy now so the previous
                // one can be released.
                wrapper.tmp_dir_path = Some(path);
            } else {
                let t = wrapper.terminal_mut();
                for (_, mut screen) in t.screens.iter() {
                    screen.kitty_images.image_limits.temporary_file = TemporaryFile::Disabled;
                }
                wrapper.tmp_dir_path = None;
            }
        }
        Option::ApcMaxBytes => {
            wrapper.stream.handler.apc_handler.max_bytes = match val::<usize>(value) {
                Some(&v) => crate::terminal::apc::MaxBytes::init_full(v),
                None => crate::terminal::apc::MaxBytes::init(None, None),
            };
        }
        Option::ApcMaxBytesKitty => {
            let max_bytes = &mut wrapper.stream.handler.apc_handler.max_bytes;
            match val::<usize>(value) {
                Some(&v) => max_bytes.put(crate::terminal::apc::Protocol::Kitty, v),
                None => max_bytes.remove(crate::terminal::apc::Protocol::Kitty),
            }
        }
        Option::GlyphProtocol => {
            let Some(&enabled) = val::<bool>(value) else {
                return Result::Success;
            };
            wrapper
                .stream
                .handler
                .apc_handler
                .enable(crate::terminal::apc::Protocol::Glyph, enabled);
            if !enabled {
                wrapper.terminal_mut().glyph_glossary.clear_and_free();
            }
        }
        Option::Selection => {
            let t = wrapper.terminal_mut();
            if let Some(ptr) = val::<selection_c::CSelection>(value) {
                let Some(sel) = ptr.to_zig() else {
                    return Result::InvalidValue;
                };
                if t.screens.active.select(Some(sel)).is_err() {
                    return Result::OutOfMemory;
                }
            } else {
                t.screens.active.clear_selection();
            }
        }
        Option::DefaultCursorStyle => {
            let raw = val::<c_int>(value).copied().unwrap_or(TerminalCursorStyle::Block as c_int);
            let Some(style) = TerminalCursorStyle::from_int(raw).map(TerminalCursorStyle::to_zig) else {
                return Result::InvalidValue;
            };
            wrapper.terminal_mut().set_default_cursor_style(style);
        }
        Option::DefaultCursorBlink => {
            let blink = val::<bool>(value).copied().unwrap_or(false);
            wrapper.terminal_mut().set_default_cursor_blink(Some(blink));
        }
        Option::ScrollbackMaxBytes => {
            wrapper
                .terminal_mut()
                .set_scrollback_max_bytes(val::<usize>(value).copied());
        }
        Option::ScrollbackMaxLines => {
            wrapper
                .terminal_mut()
                .set_scrollback_max_lines(val::<usize>(value).copied());
        }
        Option::ContinuationMaxBytes => {
            set_continuation_max_bytes(
                wrapper,
                val::<usize>(value).copied().unwrap_or(DEFAULT_CONTINUATION_MAX_BYTES),
            );
        }
        Option::UnknownMaxBytes => {
            // One limit applies to every unknown sequence type.
            let max_bytes = val::<usize>(value).copied().unwrap_or(0);
            wrapper.stream.handler.apc_handler.unknown_max_bytes = max_bytes;
            wrapper.stream.parser.osc_parser.unknown_max_bytes = max_bytes;
        }
        Option::ClipboardWriteMaxBytes => {
            wrapper.stream.handler.kitty_clipboard_write_max_bytes =
                val::<usize>(value).copied().unwrap_or(kitty_clipboard::MAX_WRITE_SIZE);
        }
        Option::ResizePullScrollback => {
            wrapper.terminal_mut().flags.resize_pull_scrollback = val::<bool>(value).copied().unwrap_or(true);
        }
        Option::XtChecksumExtension => {
            let bits = val::<u8>(value).copied().unwrap_or(0);
            // `std.math.cast(u5, bits)`
            if bits > 0x1F {
                return Result::InvalidValue;
            }
            wrapper
                .terminal_mut()
                .set_default_xt_checksum(crate::terminal::xt_checksum::Flags::from_bits(bits));
        }
        Option::Mode | Option::ModeDefault => {
            let Some(&config) = val::<ModeConfig>(value) else {
                return Result::InvalidValue;
            };
            let Some(mode) = config.to_mode() else {
                return Result::InvalidValue;
            };
            let t = wrapper.terminal_mut();
            match option {
                Option::Mode => t.modes.set(mode, config.value),
                Option::ModeDefault => {
                    if !modes::default_configurable(mode) {
                        return Result::InvalidValue;
                    }
                    t.modes.set_default(mode, config.value);
                }
                _ => unreachable!(),
            }
        }
    }
    Result::Success
}

crate::c_api_enum! {
    /// C: GhosttyTerminalCursorStyle
    pub enum TerminalCursorStyle {
        Bar = 0,
        Block = 1,
        Underline = 2,
        BlockHollow = 3,
    }
}

impl TerminalCursorStyle {
    fn to_zig(self) -> CursorStyle {
        match self {
            TerminalCursorStyle::Bar => CursorStyle::Bar,
            TerminalCursorStyle::Block => CursorStyle::Block,
            TerminalCursorStyle::Underline => CursorStyle::Underline,
            TerminalCursorStyle::BlockHollow => CursorStyle::BlockHollow,
        }
    }
}

/// C: GhosttyTerminalScrollViewport
pub type ScrollViewport = ScrollViewportC;

/// C: ghostty_terminal_scroll_viewport
///
/// # Safety
/// `terminal_` must be NULL or live; `behavior.tag` must be a valid tag.
#[export_name = "ghostty_terminal_scroll_viewport"]
pub unsafe extern "C" fn scroll_viewport(terminal_: Terminal, behavior: ScrollViewport) {
    let Some(t) = zig_terminal(terminal_) else {
        return;
    };
    t.scroll_viewport(match behavior.tag {
        ScrollViewportTag::Top => ZigScrollViewport::Top,
        ScrollViewportTag::Bottom => ZigScrollViewport::Bottom,
        ScrollViewportTag::Delta => ZigScrollViewport::Delta(behavior.value.delta),
        ScrollViewportTag::Row => ZigScrollViewport::Row(behavior.value.row),
        ScrollViewportTag::DeltaPrompt => ZigScrollViewport::DeltaPrompt(behavior.value.delta_prompt),
    });
}

/// C: ghostty_terminal_resize
///
/// # Safety
/// `terminal_` must be NULL or live.
#[export_name = "ghostty_terminal_resize"]
pub unsafe extern "C" fn resize(
    terminal_: Terminal,
    cols: CellCountInt,
    rows: CellCountInt,
    cell_width_px: u32,
    cell_height_px: u32,
) -> Result {
    let Some(wrapper) = terminal_.as_mut() else {
        return Result::InvalidValue;
    };
    match wrapper.stream.handler.resize(Resize {
        cols,
        rows,
        cell_size_px: Some(CellSizePx {
            width: cell_width_px,
            height: cell_height_px,
        }),
    }) {
        Ok(()) => Result::Success,
        Err(ResizeError::InvalidValue) => Result::InvalidValue,
        Err(ResizeError::OutOfMemory) => Result::OutOfMemory,
    }
}

/// C: ghostty_terminal_reset
///
/// # Safety
/// `terminal_` must be NULL or live.
#[export_name = "ghostty_terminal_reset"]
pub unsafe extern "C" fn reset(terminal_: Terminal) {
    let Some(wrapper) = terminal_.as_mut() else {
        return;
    };
    let t = wrapper.terminal_mut();

    // A reset always turns off synchronized output, ending its hold.
    let sync = t.modes.get(Mode::SynchronizedOutput);
    t.full_reset();
    if sync {
        Effects::render_hold_trampoline(&mut wrapper.stream.handler, false);
    }
}

/// C: GhosttyKittyGraphics
pub type KittyGraphics = kitty_gfx_c::KittyGraphics;

crate::c_api_enum! {
    /// C: GhosttyTerminalScreen (Zig: `ScreenSet.Key`)
    pub enum TerminalScreen {
        Primary = 0,
        Alternate = 1,
    }
}

impl TerminalScreen {
    pub fn from_zig(k: ScreenKey) -> TerminalScreen {
        match k {
            ScreenKey::Primary => TerminalScreen::Primary,
            ScreenKey::Alternate => TerminalScreen::Alternate,
        }
    }

    pub fn to_zig(self) -> ScreenKey {
        match self {
            TerminalScreen::Primary => ScreenKey::Primary,
            TerminalScreen::Alternate => ScreenKey::Alternate,
        }
    }
}

/// C: GhosttyTerminalScrollbar
pub type TerminalScrollbar = ScrollbarC;

/// C: GhosttyTerminalMemoryUsage
///
/// This is a sized struct, so new fields may only be added to the end.
/// The figures for each screen are separate fields rather than a nested
/// struct per screen, because only the outermost struct can grow. Add a
/// new per-screen field as a primary and alternate pair.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalMemoryUsage {
    pub size: usize,
    pub compression_supported: bool,
    pub primary_pages: u64,
    pub primary_virtual_bytes: u64,
    pub primary_resident_bytes: u64,
    pub primary_compressed_pages: u64,
    pub primary_compressed_bytes: u64,
    pub primary_image_bytes: u64,
    pub alternate_pages: u64,
    pub alternate_virtual_bytes: u64,
    pub alternate_resident_bytes: u64,
    pub alternate_compressed_pages: u64,
    pub alternate_compressed_bytes: u64,
    pub alternate_image_bytes: u64,
}

impl Default for TerminalMemoryUsage {
    fn default() -> Self {
        TerminalMemoryUsage {
            size: std::mem::size_of::<TerminalMemoryUsage>(),
            compression_supported: false,
            primary_pages: 0,
            primary_virtual_bytes: 0,
            primary_resident_bytes: 0,
            primary_compressed_pages: 0,
            primary_compressed_bytes: 0,
            primary_image_bytes: 0,
            alternate_pages: 0,
            alternate_virtual_bytes: 0,
            alternate_resident_bytes: 0,
            alternate_compressed_pages: 0,
            alternate_compressed_bytes: 0,
            alternate_image_bytes: 0,
        }
    }
}

impl TerminalMemoryUsage {
    /// Gather the memory usage of `t`. The size field is set to
    /// `caller_size` instead of our own size, so that copying the result
    /// into the caller's struct keeps the size they passed in.
    fn init(t: &ZigTerminal, caller_size: usize) -> TerminalMemoryUsage {
        let mut result = TerminalMemoryUsage {
            size: caller_size,
            compression_supported: terminal_mem::can_reclaim(terminal_mem::DecommitMode::Strict),
            ..Default::default()
        };

        let primary = t.screens.get(ScreenKey::Primary).unwrap().memory_usage();
        result.primary_pages = primary.pages.pages as u64;
        result.primary_virtual_bytes = primary.pages.virtual_bytes as u64;
        result.primary_resident_bytes = primary.pages.resident_bytes as u64;
        result.primary_compressed_pages = primary.pages.compressed_pages as u64;
        result.primary_compressed_bytes = primary.pages.compressed_bytes as u64;
        result.primary_image_bytes = primary.image_bytes as u64;

        // The alternate screen is created on first use. Until then its
        // fields stay zero.
        if let Some(screen) = t.screens.get(ScreenKey::Alternate) {
            let alternate = screen.memory_usage();
            result.alternate_pages = alternate.pages.pages as u64;
            result.alternate_virtual_bytes = alternate.pages.virtual_bytes as u64;
            result.alternate_resident_bytes = alternate.pages.resident_bytes as u64;
            result.alternate_compressed_pages = alternate.pages.compressed_pages as u64;
            result.alternate_compressed_bytes = alternate.pages.compressed_bytes as u64;
            result.alternate_image_bytes = alternate.image_bytes as u64;
        }

        result
    }
}

crate::c_api_enum! {
    /// C: GhosttyTerminalData
    ///
    /// The output type of each kind is documented in the header (Zig:
    /// `TerminalData.OutType`).
    pub enum TerminalData {
        Invalid = 0,
        Cols = 1,
        Rows = 2,
        CursorX = 3,
        CursorY = 4,
        CursorPendingWrap = 5,
        ActiveScreen = 6,
        CursorVisible = 7,
        KittyKeyboardFlags = 8,
        Scrollbar = 9,
        CursorStyle = 10,
        MouseTracking = 11,
        Title = 12,
        Pwd = 13,
        TotalRows = 14,
        ScrollbackRows = 15,
        WidthPx = 16,
        HeightPx = 17,
        ColorForeground = 18,
        ColorBackground = 19,
        ColorCursor = 20,
        ColorPalette = 21,
        ColorForegroundDefault = 22,
        ColorBackgroundDefault = 23,
        ColorCursorDefault = 24,
        ColorPaletteDefault = 25,
        KittyImageStorageLimit = 26,
        KittyImageMediumFile = 27,
        KittyImageMediumTempFile = 28,
        KittyImageMediumSharedMem = 29,
        KittyGraphics = 30,
        Selection = 31,
        ViewportActive = 32,
        VtProcessingError = 33,
        ScrollbackMaxBytes = 34,
        ScrollbackMaxLines = 35,
        ContinuationMaxBytes = 36,
        Mode = 37,
        VtGround = 38,
        CursorAtPrompt = 39,
        ClipboardWriteMaxBytes = 40,
        MouseShape = 41,
        MemoryUsage = 42,
    }
}

/// C: ghostty_terminal_get
///
/// # Safety
/// `terminal_` must be NULL or live; `out` must be NULL or point to the
/// kind's output type.
#[export_name = "ghostty_terminal_get"]
pub unsafe extern "C" fn get(terminal_: Terminal, data: c_int, out: *mut c_void) -> Result {
    let Some(data) = TerminalData::from_int(data) else {
        log::warn!(target: LOG_TARGET, "terminal_get invalid data value={data}");
        return Result::InvalidValue;
    };

    if out.is_null() {
        return Result::InvalidValue;
    }

    match data {
        TerminalData::Invalid => Result::InvalidValue,
        _ => get_typed(terminal_, data, out),
    }
}

/// C: ghostty_terminal_get_multi
///
/// # Safety
/// `keys` and `values` must be NULL or valid for `count` entries; see
/// [`get`].
#[export_name = "ghostty_terminal_get_multi"]
pub unsafe extern "C" fn get_multi(
    terminal_: Terminal,
    count: usize,
    keys: *const c_int,
    values: *const *mut c_void,
    out_written: *mut usize,
) -> Result {
    if keys.is_null() || values.is_null() {
        return Result::InvalidValue;
    }

    for i in 0..count {
        let result = get(terminal_, *keys.add(i), *values.add(i));
        if result != Result::Success {
            if let Some(w) = out_written.as_mut() {
                *w = i;
            }
            return result;
        }
    }
    if let Some(w) = out_written.as_mut() {
        *w = count;
    }
    Result::Success
}

unsafe fn get_typed(terminal_: Terminal, data: TerminalData, out: *mut c_void) -> Result {
    let Some(wrapper) = terminal_.as_mut() else {
        return Result::InvalidValue;
    };
    let ground = wrapper.stream.ground();
    let semantic_failure = wrapper.stream.handler.semantic_failure;
    let continuation_max = continuation_max_bytes(wrapper);
    let clipboard_write_max_bytes = wrapper.stream.handler.kitty_clipboard_write_max_bytes;
    let t = wrapper.terminal_mut();

    macro_rules! write {
        ($ty:ty, $v:expr) => {
            *(out as *mut $ty) = $v
        };
    }

    match data {
        TerminalData::Invalid => return Result::InvalidValue,
        TerminalData::Cols => write!(CellCountInt, t.cols),
        TerminalData::Rows => write!(CellCountInt, t.rows),
        TerminalData::CursorX => write!(CellCountInt, t.screens.active.cursor.x),
        TerminalData::CursorY => write!(CellCountInt, t.screens.active.cursor.y),
        TerminalData::CursorPendingWrap => write!(bool, t.screens.active.cursor.pending_wrap),
        TerminalData::ActiveScreen => write!(TerminalScreen, TerminalScreen::from_zig(t.screens.active_key)),
        TerminalData::CursorVisible => write!(bool, t.modes.get(Mode::CursorVisible)),
        TerminalData::KittyKeyboardFlags => write!(u8, t.screens.active.kitty_keyboard.current().int()),
        TerminalData::Scrollbar => write!(TerminalScrollbar, t.screens.active.pages.scrollbar().cval()),
        TerminalData::CursorStyle => write!(style_c::Style, style_c::Style::from_style(&t.screens.active.cursor.style)),
        TerminalData::MouseTracking => write!(
            bool,
            t.modes.get(Mode::MouseEventX10)
                || t.modes.get(Mode::MouseEventNormal)
                || t.modes.get(Mode::MouseEventButton)
                || t.modes.get(Mode::MouseEventAny)
        ),
        TerminalData::MouseShape => write!(c_int, t.mouse_shape as c_int),
        TerminalData::Title => write!(LibString, {
            let title = t.get_title().unwrap_or(b"");
            LibString {
                ptr: title.as_ptr(),
                len: title.len(),
            }
        }),
        TerminalData::Pwd => write!(LibString, {
            let pwd = t.get_pwd().unwrap_or(b"");
            LibString {
                ptr: pwd.as_ptr(),
                len: pwd.len(),
            }
        }),
        TerminalData::TotalRows => write!(usize, t.screens.active.pages.total_rows),
        TerminalData::ScrollbackRows => write!(usize, t.screens.active.pages.total_rows - t.rows as usize),
        TerminalData::WidthPx => write!(u32, t.width_px),
        TerminalData::HeightPx => write!(u32, t.height_px),
        TerminalData::ColorForeground => {
            let Some(c) = t.colors.foreground.get() else {
                return Result::NoValue;
            };
            write!(color::RgbC, c.cval())
        }
        TerminalData::ColorBackground => {
            let Some(c) = t.colors.background.get() else {
                return Result::NoValue;
            };
            write!(color::RgbC, c.cval())
        }
        TerminalData::ColorCursor => {
            let Some(c) = t.colors.cursor.get() else {
                return Result::NoValue;
            };
            write!(color::RgbC, c.cval())
        }
        TerminalData::ColorForegroundDefault => {
            let Some(c) = t.colors.foreground.default else {
                return Result::NoValue;
            };
            write!(color::RgbC, c.cval())
        }
        TerminalData::ColorBackgroundDefault => {
            let Some(c) = t.colors.background.default else {
                return Result::NoValue;
            };
            write!(color::RgbC, c.cval())
        }
        TerminalData::ColorCursorDefault => {
            let Some(c) = t.colors.cursor.default else {
                return Result::NoValue;
            };
            write!(color::RgbC, c.cval())
        }
        TerminalData::ColorPalette => write!(color::PaletteC, color::palette_cval(&t.colors.palette.current)),
        TerminalData::ColorPaletteDefault => write!(color::PaletteC, color::palette_cval(t.colors.palette.original())),
        TerminalData::KittyImageStorageLimit => {
            if !build_options::KITTY_GRAPHICS {
                return Result::NoValue;
            }
            write!(u64, t.screens.active.kitty_images.total_limit as u64)
        }
        TerminalData::KittyImageMediumFile => {
            if !build_options::KITTY_GRAPHICS {
                return Result::NoValue;
            }
            write!(bool, t.screens.active.kitty_images.image_limits.file)
        }
        TerminalData::KittyImageMediumTempFile => {
            if !build_options::KITTY_GRAPHICS {
                return Result::NoValue;
            }
            let screen = &*t.screens.active;
            let dir: &[u8] = match &screen.kitty_images.image_limits.temporary_file {
                TemporaryFile::Enabled { directory } => directory,
                TemporaryFile::Disabled => b"",
            };
            write!(LibString, LibString::init(dir))
        }
        TerminalData::KittyImageMediumSharedMem => {
            if !build_options::KITTY_GRAPHICS {
                return Result::NoValue;
            }
            write!(bool, t.screens.active.kitty_images.image_limits.shared_memory)
        }
        TerminalData::KittyGraphics => {
            if !build_options::KITTY_GRAPHICS {
                return Result::NoValue;
            }
            let screen = t.screens.active.as_ptr();
            write!(KittyGraphics, std::ptr::addr_of_mut!((*screen).kitty_images))
        }
        TerminalData::Selection => {
            let Some(sel) = t.screens.active.selection else {
                return Result::NoValue;
            };
            write!(selection_c::CSelection, selection_c::CSelection::from_zig(sel))
        }
        TerminalData::ViewportActive => write!(
            bool,
            t.screens.active.pages.viewport == crate::terminal::page_list::Viewport::Active
        ),
        TerminalData::VtProcessingError => write!(bool, semantic_failure),
        TerminalData::VtGround => write!(bool, ground),
        TerminalData::ScrollbackMaxBytes => {
            let max = t.screens.get(ScreenKey::Primary).unwrap().pages.limits.bytes.explicit;
            if max == usize::MAX {
                return Result::NoValue;
            }
            write!(usize, max)
        }
        TerminalData::ScrollbackMaxLines => {
            let max = t.screens.get(ScreenKey::Primary).unwrap().pages.limits.lines.explicit;
            if max == usize::MAX {
                return Result::NoValue;
            }
            write!(usize, max)
        }
        TerminalData::ContinuationMaxBytes => write!(usize, continuation_max),
        TerminalData::ClipboardWriteMaxBytes => write!(usize, clipboard_write_max_bytes),
        TerminalData::Mode => {
            let out = &mut *(out as *mut ModeConfig);
            let Some(mode) = out.to_mode() else {
                return Result::InvalidValue;
            };
            out.value = t.modes.get(mode);
        }
        TerminalData::CursorAtPrompt => write!(bool, t.cursor_is_at_prompt()),
        TerminalData::MemoryUsage => {
            // A smaller size means the caller doesn't have every field of
            // the first version of this struct, so reject it. A larger size
            // means the caller was built against a newer version with more
            // fields. We write only the fields we know and leave the rest
            // alone.
            //
            // When fields are added later, compare against the size of the
            // first version here, and copy only min(out.size, current size)
            // bytes.
            let out = out as *mut TerminalMemoryUsage;
            let size = (*out).size;
            if size < std::mem::size_of::<TerminalMemoryUsage>() {
                return Result::InvalidValue;
            }
            *out = TerminalMemoryUsage::init(t, size);
        }
    }

    Result::Success
}

/// C: ghostty_terminal_grid_ref
///
/// # Safety
/// `terminal_` must be NULL or live; a non-null `out_ref` must be writable.
#[export_name = "ghostty_terminal_grid_ref"]
pub unsafe extern "C" fn grid_ref(terminal_: Terminal, pt: point::PointC, out_ref: *mut grid_ref_c::CGridRef) -> Result {
    let Some(t) = zig_terminal(terminal_) else {
        return Result::InvalidValue;
    };
    let zig_pt = point::Point::from_c(pt);
    let Some(p) = t.screens.active.pages.pin(zig_pt) else {
        return Result::InvalidValue;
    };
    if let Some(out) = out_ref.as_mut() {
        *out = grid_ref_c::CGridRef::from_pin(p);
    }
    Result::Success
}

/// C: ghostty_terminal_grid_ref_track
///
/// # Safety
/// `terminal_` must be NULL or live; a non-null `out_ref` must be writable.
#[export_name = "ghostty_terminal_grid_ref_track"]
pub unsafe extern "C" fn grid_ref_track(
    terminal_: Terminal,
    pt: point::PointC,
    out_ref: *mut grid_ref_tracked_c::CTrackedGridRef,
) -> Result {
    let Some(wrapper) = terminal_.as_mut() else {
        return Result::InvalidValue;
    };
    let Some(out) = out_ref.as_mut() else {
        return Result::InvalidValue;
    };
    *out = std::ptr::null_mut();

    let alloc = wrapper.alloc;
    let t = wrapper.terminal_mut();
    let mut screen = t.screens.active;
    let list: &mut PageList = &mut screen.pages;
    let Some(p) = list.pin(point::Point::from_c(pt)) else {
        return Result::InvalidValue;
    };
    let Ok(tracked_pin) = list.track_pin(p) else {
        return Result::OutOfMemory;
    };

    let screen_key = TerminalScreen::from_zig(t.screens.active_key);
    let screen_generation = t.screens.generation(t.screens.active_key);
    let r = alloc.create(grid_ref_tracked_c::TrackedGridRef {
        alloc,
        terminal: terminal_,
        screen_key,
        screen_generation,
        pin: tracked_pin,
    });
    if r.is_null() {
        list.untrack_pin(tracked_pin);
        return Result::OutOfMemory;
    }

    // Store the tracked ref in the terminal so that when we free
    // the terminal the tracked ref can be detached safely.
    wrapper.tracked_grid_refs.push(r);

    *out = r;
    Result::Success
}

/// C: ghostty_terminal_point_from_grid_ref
///
/// # Safety
/// `terminal_` must be NULL or live; `ref_` must be valid and its node live;
/// a non-null `out` must be writable.
#[export_name = "ghostty_terminal_point_from_grid_ref"]
pub unsafe extern "C" fn point_from_grid_ref(
    terminal_: Terminal,
    ref_: &grid_ref_c::CGridRef,
    tag: c_int,
    out: *mut point::Coordinate,
) -> Result {
    let Some(t) = zig_terminal(terminal_) else {
        return Result::InvalidValue;
    };
    let Some(tag) = point_tag_from_int(tag) else {
        return Result::InvalidValue;
    };
    let Some(p) = ref_.to_pin() else {
        return Result::InvalidValue;
    };
    let Some(pt) = t.screens.active.pages.point_from_pin(tag, p) else {
        return Result::NoValue;
    };
    if let Some(o) = out.as_mut() {
        *o = pt.coord();
    }
    Result::Success
}

/// C: ghostty_terminal_free
///
/// # Safety
/// `terminal_` must be NULL or a terminal from [`new`] not used afterwards.
#[export_name = "ghostty_terminal_free"]
pub unsafe extern "C" fn free(terminal_: Terminal) {
    let Some(wrapper) = terminal_.as_mut() else {
        return;
    };

    for &r in &wrapper.tracked_grid_refs {
        (*r).terminal = std::ptr::null_mut();
    }
    wrapper.tracked_grid_refs = Vec::new();
    for &search in &wrapper.searches {
        (*search).terminal = std::ptr::null_mut();
    }
    wrapper.searches = Vec::new();
    let alloc = wrapper.alloc;
    alloc.destroy(terminal_);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Zig's `std.mem.Allocator.failing`: every allocation fails.
    struct FailingZig;

    impl ZigAllocator for FailingZig {
        fn alloc(&self, _len: usize, _alignment: u8, _ra: usize) -> *mut u8 {
            std::ptr::null_mut()
        }

        fn resize(&self, _m: *mut u8, _old: usize, _a: u8, _new: usize, _ra: usize) -> bool {
            false
        }

        fn remap(&self, _m: *mut u8, _old: usize, _a: u8, _new: usize, _ra: usize) -> *mut u8 {
            std::ptr::null_mut()
        }

        fn free(&self, _m: *mut u8, _len: usize, _a: u8, _ra: usize) {}
    }

    static FAILING_ZIG: FailingZig = FailingZig;

    struct StaticDyn(&'static dyn ZigAllocator);
    // SAFETY: wraps a stateless allocator.
    unsafe impl Sync for StaticDyn {}
    static FAILING_DYN: StaticDyn = StaticDyn(&FAILING_ZIG);

    /// `CAllocator.fromZig(&std.mem.Allocator.failing)`.
    pub(crate) static FAILING_C_ALLOCATOR: CAllocator = CAllocator::from_zig(&FAILING_DYN.0);

    // -----------------------------------------------------------------
    // Rust test helpers (Zig tests spell these out inline).

    use crate::lib::allocator::C_ALLOCATOR;
    use crate::terminal::c::allocator as allocator_c;
    use std::cell::{Cell, RefCell};

    fn new_t(cols: CellCountInt, rows: CellCountInt) -> Terminal {
        let mut t: Terminal = std::ptr::null_mut();
        assert_eq!(Result::Success, new(Some(&C_ALLOCATOR), &mut t, cols, rows));
        assert!(!t.is_null());
        t
    }

    fn p<T>(v: &T) -> *const c_void {
        v as *const T as *const c_void
    }

    fn o<T>(v: &mut T) -> *mut c_void {
        v as *mut T as *mut c_void
    }

    fn vw(t: Terminal, data: &[u8]) {
        unsafe { vt_write(t, data.as_ptr(), data.len()) }
    }

    fn zt<'a>(t: Terminal) -> &'a mut ZigTerminal {
        unsafe { zig_terminal(t).unwrap() }
    }

    fn wr<'a>(t: Terminal) -> &'a mut TerminalWrapper {
        unsafe { &mut *t }
    }

    fn plain(t: Terminal) -> String {
        zt(t).plain_string().unwrap()
    }

    fn sopt(t: Terminal, option: Option, value: *const c_void) -> Result {
        unsafe { set(t, option as c_int, value) }
    }

    fn gdat(t: Terminal, data: TerminalData, out: *mut c_void) -> Result {
        unsafe { get(t, data as c_int, out) }
    }

    fn mode_tag(value: u16, ansi: bool) -> ModeTagBacking {
        ModeTag::new(value, ansi).to_bits()
    }

    fn enable_continuation(t: Terminal) {
        let limit: usize = 1024;
        assert_eq!(Result::Success, sopt(t, Option::ContinuationMaxBytes, p(&limit)));
    }

    fn cont_buf(t: Terminal, buf: Option2<&mut [u8]>, written: &mut usize) -> Result {
        unsafe {
            match buf {
                Some(b) => continuation_buf(t, b.as_mut_ptr(), b.len(), written),
                None => continuation_buf(t, std::ptr::null_mut(), 0, written),
            }
        }
    }

    type Option2<T> = std::option::Option<T>;

    fn sv(t: Terminal, v: ZigScrollViewport) {
        unsafe { scroll_viewport(t, v.cval()) }
    }

    fn scrollbar(t: Terminal) -> TerminalScrollbar {
        let mut s = TerminalScrollbar { total: 0, offset: 0, len: 0 };
        assert_eq!(Result::Success, gdat(t, TerminalData::Scrollbar, o(&mut s)));
        s
    }

    fn viewport_active(t: Terminal) -> bool {
        let mut v = false;
        assert_eq!(Result::Success, gdat(t, TerminalData::ViewportActive, o(&mut v)));
        v
    }

    // A shared write_pty capture (Zig: each test's `S.writePty`). Tests
    // run on their own threads, so thread-locals are per test.
    thread_local! {
        static LAST: RefCell<Option2<Vec<u8>>> = const { RefCell::new(None) };
        static ALL: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        static CALLS: Cell<usize> = const { Cell::new(0) };
        static LAST_UD: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
    }

    unsafe extern "C" fn write_pty_capture(_: Terminal, ud: *mut c_void, ptr: *const u8, len: usize) {
        let data = std::slice::from_raw_parts(ptr, len);
        LAST.with(|l| *l.borrow_mut() = Some(data.to_vec()));
        ALL.with(|a| a.borrow_mut().extend_from_slice(data));
        CALLS.set(CALLS.get() + 1);
        LAST_UD.set(ud);
    }

    fn last() -> Option2<Vec<u8>> {
        LAST.with(|l| l.borrow().clone())
    }

    fn clear_last() {
        LAST.with(|l| *l.borrow_mut() = None);
    }

    fn all_written() -> Vec<u8> {
        ALL.with(|a| a.borrow().clone())
    }

    fn clear_all() {
        ALL.with(|a| a.borrow_mut().clear());
    }

    fn set_write_pty(t: Terminal) {
        assert_eq!(
            Result::Success,
            sopt(t, Option::WritePty, write_pty_capture as WritePtyFn as *const c_void)
        );
    }

    fn cs(s: LibString) -> Vec<u8> {
        lib_string_slice(s).to_vec()
    }

    fn hex_upper(s: &[u8]) -> String {
        s.iter().map(|b| format!("{b:02X}")).collect()
    }

    // -----------------------------------------------------------------

    #[test]
    fn new_free() {
        let t = new_t(80, 24);
        unsafe { free(t) };
    }

    #[test]
    fn new_invalid_value() {
        let mut t: Terminal = std::ptr::null_mut();
        assert_eq!(Result::InvalidValue, new(Some(&C_ALLOCATOR), &mut t, 0, 24));
        assert!(t.is_null());

        assert_eq!(Result::InvalidValue, new(Some(&C_ALLOCATOR), &mut t, 80, 0));
        assert!(t.is_null());
    }

    #[test]
    fn continuation_option_and_data() {
        let t = new_t(80, 24);

        let mut value: usize = 0;
        assert_eq!(Result::Success, gdat(t, TerminalData::ContinuationMaxBytes, o(&mut value)));
        assert_eq!(DEFAULT_CONTINUATION_MAX_BYTES, value);

        let custom: usize = 1234;
        assert_eq!(Result::Success, sopt(t, Option::ContinuationMaxBytes, p(&custom)));
        assert_eq!(Result::Success, gdat(t, TerminalData::ContinuationMaxBytes, o(&mut value)));
        assert_eq!(custom, value);

        assert_eq!(Result::Success, sopt(t, Option::ContinuationMaxBytes, std::ptr::null()));
        assert_eq!(Result::Success, gdat(t, TerminalData::ContinuationMaxBytes, o(&mut value)));
        assert_eq!(DEFAULT_CONTINUATION_MAX_BYTES, value);

        let disabled: usize = 0;
        assert_eq!(Result::Success, sopt(t, Option::ContinuationMaxBytes, p(&disabled)));
        assert_eq!(Result::Success, gdat(t, TerminalData::ContinuationMaxBytes, o(&mut value)));
        assert_eq!(0, value);

        let mut written: usize = 99;
        assert_eq!(Result::InvalidValue, cont_buf(t, None, &mut written));
        assert_eq!(0, written);
        unsafe { free(t) };
    }

    #[test]
    fn continuation_buffer_and_allocator_export_exact_suffix() {
        let t = new_t(80, 24);
        enable_continuation(t);

        vw(t, b"A\x1b[31");

        let mut required: usize = 0;
        assert_eq!(Result::OutOfSpace, cont_buf(t, None, &mut required));
        assert_eq!(4, required);

        let mut short = [0u8; 2];
        assert_eq!(Result::OutOfSpace, cont_buf(t, Some(&mut short), &mut required));
        assert_eq!(4, required);

        let mut buf = [0u8; 8];
        let mut written: usize = 0;
        assert_eq!(Result::Success, cont_buf(t, Some(&mut buf), &mut written));
        assert_eq!(b"\x1b[31", &buf[..written]);

        unsafe {
            let mut out_ptr: *mut u8 = std::ptr::null_mut();
            let mut out_len: usize = 0;
            assert_eq!(
                Result::Success,
                continuation_alloc(t, Some(&C_ALLOCATOR), &mut out_ptr, &mut out_len)
            );
            assert!(!out_ptr.is_null());
            assert_eq!(b"\x1b[31", std::slice::from_raw_parts(out_ptr, out_len));
            allocator_c::free(Some(&C_ALLOCATOR), out_ptr, out_len);

            vw(t, b"m");
            assert_eq!(Result::OutOfSpace, cont_buf(t, None, &mut required));
            assert_eq!(0, required);

            // Completing the sequence must return an empty, allocation-free result.
            let failing = &FAILING_C_ALLOCATOR;
            assert_eq!(
                Result::Success,
                continuation_alloc(t, Some(failing), &mut out_ptr, &mut out_len)
            );
            assert!(out_ptr.is_null());
            assert_eq!(0, out_len);
            allocator_c::free(Some(failing), out_ptr, out_len);
            free(t);
        }
    }

    #[test]
    fn continuation_export_tracks_split_utf8() {
        let t = new_t(80, 24);
        enable_continuation(t);

        let prefix = [0xF0u8, 0x9F];
        vw(t, &prefix);

        let mut buf = [0u8; 4];
        let mut written: usize = 0;
        assert_eq!(Result::Success, cont_buf(t, Some(&mut buf), &mut written));
        assert_eq!(&prefix[..], &buf[..written]);

        let suffix = [0x98u8, 0x80];
        vw(t, &suffix);
        assert_eq!(Result::OutOfSpace, cont_buf(t, None, &mut written));
        assert_eq!(0, written);
        unsafe { free(t) };
    }

    #[test]
    fn continuation_callback_writer_reports_success_and_failure() {
        struct Sink {
            bytes: [u8; 8],
            len: usize,
        }

        unsafe extern "C" fn sink_write(userdata: *mut c_void, data: *const u8, len: usize) -> bool {
            let s = &mut *(userdata as *mut Sink);
            if len > s.bytes.len() - s.len {
                return false;
            }
            s.bytes[s.len..s.len + len].copy_from_slice(std::slice::from_raw_parts(data, len));
            s.len += len;
            true
        }

        unsafe extern "C" fn sink_fail(_: *mut c_void, _: *const u8, _: usize) -> bool {
            false
        }

        let t = new_t(80, 24);
        enable_continuation(t);
        vw(t, b"\x1b[31");

        unsafe {
            let mut sink = Sink { bytes: [0; 8], len: 0 };
            assert_eq!(
                Result::Success,
                continuation_write(
                    t,
                    c_io::Writer {
                        write: Some(sink_write),
                        userdata: o(&mut sink),
                    }
                )
            );
            assert_eq!(b"\x1b[31", &sink.bytes[..sink.len]);

            let fail = c_io::Writer {
                write: Some(sink_fail),
                userdata: std::ptr::null_mut(),
            };
            assert_eq!(Result::IoError, continuation_write(t, fail));
            assert_eq!(
                Result::InvalidValue,
                continuation_write(
                    t,
                    c_io::Writer {
                        write: None,
                        userdata: std::ptr::null_mut(),
                    }
                )
            );
            assert_eq!(Result::InvalidValue, continuation_write(std::ptr::null_mut(), fail));
            free(t);
        }
    }

    #[test]
    fn continuation_runtime_reconfiguration_recovers_safely() {
        let t = new_t(80, 24);

        let enough: usize = 8;
        assert_eq!(Result::Success, sopt(t, Option::ContinuationMaxBytes, p(&enough)));
        vw(t, b"\x1b[123");
        let too_small: usize = 2;
        assert_eq!(Result::Success, sopt(t, Option::ContinuationMaxBytes, p(&too_small)));

        let mut written: usize = 99;
        assert_eq!(Result::InvalidValue, cont_buf(t, None, &mut written));
        assert_eq!(0, written);

        // Raising the limit cannot reconstruct discarded bytes, but a new ESC is
        // a complete replay start and repairs tracking without first grounding.
        assert_eq!(Result::Success, sopt(t, Option::ContinuationMaxBytes, p(&enough)));
        vw(t, b"\x1b[");
        let mut buf = [0u8; 8];
        assert_eq!(Result::Success, cont_buf(t, Some(&mut buf), &mut written));
        assert_eq!(b"\x1b[", &buf[..written]);

        let disabled: usize = 0;
        assert_eq!(Result::Success, sopt(t, Option::ContinuationMaxBytes, p(&disabled)));
        assert_eq!(Result::Success, sopt(t, Option::ContinuationMaxBytes, p(&enough)));
        assert_eq!(Result::InvalidValue, cont_buf(t, None, &mut written));

        // Completing the sequence reaches ground and resets the broken marker.
        vw(t, b"m");
        assert_eq!(Result::OutOfSpace, cont_buf(t, None, &mut written));
        assert_eq!(0, written);
        unsafe { free(t) };
    }

    #[test]
    fn continuation_internal_allocation_and_restoration() {
        let source = new_t(80, 24);
        enable_continuation(source);
        vw(source, b"\x1b[31");

        unsafe {
            let bytes = continuation_alloc_io(source, false).unwrap();
            assert_eq!(b"\x1b[31", bytes.as_slice());

            let restored = new_t(80, 24);
            enable_continuation(restored);
            restore_continuation(restored, &bytes).unwrap();

            let reexported = continuation_alloc_io(restored, false).unwrap();
            assert_eq!(bytes, reexported);

            let invalid = new_t(80, 24);
            enable_continuation(invalid);
            assert_eq!(
                Err(RestoreContinuationError::InvalidContinuation),
                restore_continuation(invalid, b"A")
            );

            free(invalid);
            free(restored);
            free(source);
        }
    }

    #[test]
    fn set_scrollback_limits() {
        let t = new_t(80, 3);

        let primary = zt(t).screens.get(ScreenKey::Primary).unwrap();

        let max_bytes: usize = 0;
        assert_eq!(Result::Success, sopt(t, Option::ScrollbackMaxBytes, p(&max_bytes)));
        assert_eq!(0, primary.pages.limits.bytes.explicit);
        assert!(primary.no_scrollback);

        assert_eq!(Result::Success, sopt(t, Option::ScrollbackMaxBytes, std::ptr::null()));
        assert_eq!(usize::MAX, primary.pages.limits.bytes.explicit);
        assert!(!primary.no_scrollback);

        let max_lines: usize = 12;
        assert_eq!(Result::Success, sopt(t, Option::ScrollbackMaxLines, p(&max_lines)));
        assert_eq!(max_lines, primary.pages.limits.lines.explicit);

        assert_eq!(Result::Success, sopt(t, Option::ScrollbackMaxLines, std::ptr::null()));
        assert_eq!(usize::MAX, primary.pages.limits.lines.explicit);
        unsafe { free(t) };
    }

    #[test]
    fn free_null() {
        unsafe { free(std::ptr::null_mut()) };
    }

    #[test]
    fn scroll_viewport_test() {
        let t = new_t(5, 2);

        assert!(viewport_active(t));

        // Write "hello" on the first line
        vw(t, b"hello");

        // Push "hello" into scrollback with 3 newlines (index = ESC D)
        vw(t, b"\x1bD\x1bD\x1bD");
        // Viewport should be empty now since hello scrolled off
        assert_eq!("", plain(t));

        // Scroll to top: "hello" should be visible again
        sv(t, ZigScrollViewport::Top);
        assert!(!viewport_active(t));
        assert_eq!("hello", plain(t));

        // Scroll to bottom: viewport should be empty again
        sv(t, ZigScrollViewport::Bottom);
        assert!(viewport_active(t));
        assert_eq!("", plain(t));

        // Scroll up by delta to bring "hello" back into view
        sv(t, ZigScrollViewport::Delta(-3));
        assert!(!viewport_active(t));
        assert_eq!("hello", plain(t));
        unsafe { free(t) };
    }

    #[test]
    fn scroll_viewport_row() {
        let t = new_t(5, 2);

        // Write 4 rows so that rows "1" and "2" are pushed into scrollback:
        // total rows is 4, viewport length is 2.
        vw(t, b"1\r\n2\r\n3\r\n4");
        assert!(viewport_active(t));

        // Row 0 is the top of the scrollback.
        sv(t, ZigScrollViewport::Row(0));
        assert!(!viewport_active(t));
        assert_eq!("1\n2", plain(t));

        // An absolute row within the scrollback becomes the first visible
        // row and round-trips through the scrollbar offset.
        sv(t, ZigScrollViewport::Row(1));
        assert_eq!("2\n3", plain(t));
        let s = scrollbar(t);
        assert_eq!(4, s.total);
        assert_eq!(1, s.offset);
        assert_eq!(2, s.len);

        // A row past the end clamps to the active area.
        sv(t, ZigScrollViewport::Row(9999));
        assert!(viewport_active(t));
        assert_eq!("3\n4", plain(t));
        assert_eq!(2, scrollbar(t).offset);
        unsafe { free(t) };
    }

    #[test]
    fn scroll_viewport_row_alt_screen() {
        let t = new_t(5, 2);

        // Enter the alternate screen, which has no scrollback.
        vw(t, b"\x1b[?1049h");
        let mut screen = TerminalScreen::Primary;
        assert_eq!(Result::Success, gdat(t, TerminalData::ActiveScreen, o(&mut screen)));
        assert_eq!(TerminalScreen::Alternate, screen);

        // Scrolling to any row keeps the viewport on the active area.
        sv(t, ZigScrollViewport::Row(0));
        assert!(viewport_active(t));
        sv(t, ZigScrollViewport::Row(9999));
        assert!(viewport_active(t));

        // With no scrollback the scrollbar covers exactly the active area.
        let s = scrollbar(t);
        assert_eq!(2, s.total);
        assert_eq!(0, s.offset);
        assert_eq!(2, s.len);
        unsafe { free(t) };
    }

    #[test]
    fn scroll_viewport_delta_prompt() {
        let t = new_t(80, 2);

        let transcript = concat!(
            "\x1b]133;P;k=i\x07p1\r\n",
            "\x1b]133;B\x07cmd1\r\n",
            "\x1b]133;C\x07out1\r\n",
            "\x1b]133;P;k=i\x07p2\r\n",
            "\x1b]133;B\x07cmd2\r\n",
            "\x1b]133;C\x07out2\r\n",
            "\x1b]133;P;k=i\x07p3\r\n",
            "\x1b]133;B\x07cmd3\r\n",
            "\x1b]133;C\x07out3\r\n",
            "\x1b]133;P;k=i\x07p4",
        );
        vw(t, transcript.as_bytes());

        // A negative count goes to the previous prompt, and a positive count
        // returns to the next prompt relative to the current viewport top.
        sv(t, ZigScrollViewport::DeltaPrompt(-1));
        assert_eq!(6, scrollbar(t).offset);
        sv(t, ZigScrollViewport::DeltaPrompt(-1));
        assert_eq!(3, scrollbar(t).offset);
        sv(t, ZigScrollViewport::DeltaPrompt(1));
        assert_eq!(6, scrollbar(t).offset);

        // Counts beyond the retained prompt history clamp to the oldest or
        // newest matching prompt, including the signed minimum boundary.
        sv(t, ZigScrollViewport::DeltaPrompt(isize::MIN));
        assert_eq!(0, scrollbar(t).offset);
        sv(t, ZigScrollViewport::DeltaPrompt(isize::MAX));
        assert_eq!(8, scrollbar(t).offset);

        // A zero count leaves the selected prompt unchanged.
        sv(t, ZigScrollViewport::DeltaPrompt(0));
        assert_eq!(8, scrollbar(t).offset);
        unsafe { free(t) };
    }

    #[test]
    fn scroll_viewport_delta_prompt_continuation() {
        let t = new_t(80, 2);

        let transcript = concat!(
            "\x1b]133;P;k=i\x07p1\r\n",
            "\x1b]133;P;k=c\x07p1-continued\r\n",
            "\x1b]133;B\x07cmd1\r\n",
            "\x1b]133;C\x07out1\r\n",
            "\x1b]133;P;k=i\x07p2\r\n",
            "\x1b]133;B\x07cmd2\r\n",
            "\x1b]133;C\x07out2",
        );
        vw(t, transcript.as_bytes());

        // The continuation row belongs to the first prompt group. Moving
        // forward from that group lands on the second prompt.
        sv(t, ZigScrollViewport::Row(0));
        sv(t, ZigScrollViewport::DeltaPrompt(1));
        assert_eq!(4, scrollbar(t).offset);
        unsafe { free(t) };
    }

    #[test]
    fn scroll_viewport_delta_prompt_final_continuation_snaps_to_active() {
        let t = new_t(80, 3);

        let transcript = concat!(
            "\x1b]133;P;k=i\x07prompt\r\n",
            "\x1b]133;P;k=c\x07continued\r\n",
            "continued\r\ncontinued\r\ncontinued\r\ncontinued\r\ncontinued",
        );
        vw(t, transcript.as_bytes());
        sv(t, ZigScrollViewport::Row(1));
        assert_eq!(1, scrollbar(t).offset);

        sv(t, ZigScrollViewport::DeltaPrompt(1));
        assert_eq!(4, scrollbar(t).offset);
        unsafe { free(t) };
    }

    #[test]
    fn scroll_viewport_delta_prompt_no_markers_and_alternate_screen() {
        let t = new_t(80, 2);

        vw(t, b"one\r\ntwo\r\nthree\r\nfour");
        sv(t, ZigScrollViewport::Row(0));

        // Without OSC 133 prompt markers, a request leaves the viewport alone.
        sv(t, ZigScrollViewport::DeltaPrompt(1));
        assert_eq!(0, scrollbar(t).offset);

        vw(t, b"\x1b[?1049h");
        sv(t, ZigScrollViewport::DeltaPrompt(-1));
        let s = scrollbar(t);
        assert_eq!(0, s.offset);
        assert_eq!(2, s.total);
        unsafe { free(t) };
    }

    fn memory_usage(t: Terminal) -> TerminalMemoryUsage {
        let mut m = TerminalMemoryUsage::default();
        assert_eq!(Result::Success, gdat(t, TerminalData::MemoryUsage, o(&mut m)));
        m
    }

    fn compressible_history() -> Vec<u8> {
        let line = b"repeated and compressible terminal history\r\n";
        line.repeat(4_000)
    }

    #[test]
    fn get_memory_usage() {
        let t = new_t(80, 24);

        let fresh = memory_usage(t);
        assert_eq!(std::mem::size_of::<TerminalMemoryUsage>(), fresh.size);
        assert_eq!(
            terminal_mem::can_reclaim(terminal_mem::DecommitMode::Strict),
            fresh.compression_supported
        );
        assert!(fresh.primary_pages > 0);
        assert!(fresh.primary_virtual_bytes > 0);
        assert!(fresh.primary_resident_bytes <= fresh.primary_virtual_bytes);
        assert_eq!(0, fresh.primary_compressed_pages);
        assert_eq!(0, fresh.primary_compressed_bytes);
        assert_eq!(0, fresh.primary_image_bytes);

        // The alternate screen doesn't exist yet.
        assert_eq!(0, fresh.alternate_pages);
        assert_eq!(0, fresh.alternate_virtual_bytes);
        assert_eq!(0, fresh.alternate_resident_bytes);
        assert_eq!(0, fresh.alternate_compressed_pages);
        assert_eq!(0, fresh.alternate_compressed_bytes);
        assert_eq!(0, fresh.alternate_image_bytes);

        // Write compressible history.
        vw(t, &compressible_history());

        let history = memory_usage(t);
        assert!(history.primary_pages > fresh.primary_pages);
        assert!(history.primary_resident_bytes > fresh.primary_resident_bytes);

        let mut compression_result = CompressionResult::Unsupported;
        assert_eq!(Result::Success, unsafe {
            compress(t, CompressionMode::Full as c_int, &mut compression_result)
        });
        assert_eq!(
            history.compression_supported,
            compression_result == CompressionResult::Complete
        );

        let compressed = memory_usage(t);
        assert_eq!(history.primary_pages, compressed.primary_pages);
        assert_eq!(history.primary_virtual_bytes, compressed.primary_virtual_bytes);
        if compressed.compression_supported {
            assert!(compressed.primary_compressed_pages > 0);
            assert!(compressed.primary_compressed_bytes > 0);
            assert!(compressed.primary_resident_bytes < history.primary_resident_bytes);
        } else {
            assert_eq!(0, compressed.primary_compressed_pages);
        }

        // The query never restores a compressed page.
        let again = memory_usage(t);
        assert_eq!(compressed, again);

        // Kitty images are counted separately from pages.
        if build_options::KITTY_GRAPHICS {
            // 1x2 RGB image, 6 bytes of pixel data.
            vw(t, b"\x1b_Ga=t,t=d,f=24,i=1,s=1,v=2;////////\x1b\\");
            let images = memory_usage(t);
            assert!(images.primary_image_bytes > 0);

            vw(t, b"\x1b_Ga=d,d=I,i=1\x1b\\");
            let images = memory_usage(t);
            assert_eq!(0, images.primary_image_bytes);
        }

        // Entering the alternate screen creates it.
        vw(t, b"\x1b[?1049h");
        let alternate = memory_usage(t);
        assert!(alternate.alternate_pages > 0);
        assert!(alternate.alternate_virtual_bytes > 0);
        assert!(alternate.alternate_resident_bytes > 0);
        assert!(alternate.alternate_resident_bytes <= alternate.alternate_virtual_bytes);
        assert_eq!(compressed.primary_pages, alternate.primary_pages);
        unsafe { free(t) };
    }

    #[test]
    fn get_memory_usage_size_rule() {
        let t = new_t(80, 24);

        // A size smaller than the first layout is rejected and nothing is
        // written.
        let mut small = TerminalMemoryUsage {
            size: std::mem::size_of::<TerminalMemoryUsage>() - 1,
            primary_pages: 1234,
            ..Default::default()
        };
        assert_eq!(Result::InvalidValue, gdat(t, TerminalData::MemoryUsage, o(&mut small)));
        assert_eq!(std::mem::size_of::<TerminalMemoryUsage>() - 1, small.size);
        assert_eq!(1234, small.primary_pages);

        // A size larger than ours, from a caller built against a newer
        // layout, gets the known prefix and the rest is left untouched.
        #[repr(C)]
        struct Larger {
            usage: TerminalMemoryUsage,
            extra: u64,
        }
        let mut large = Larger {
            usage: TerminalMemoryUsage {
                size: std::mem::size_of::<Larger>(),
                ..Default::default()
            },
            extra: 0xDEADBEEF,
        };
        assert_eq!(Result::Success, gdat(t, TerminalData::MemoryUsage, o(&mut large)));
        assert_eq!(std::mem::size_of::<Larger>(), large.usage.size);
        assert!(large.usage.primary_pages > 0);
        assert_eq!(0xDEADBEEF, large.extra);
        unsafe { free(t) };
    }

    #[test]
    fn scroll_viewport_null() {
        sv(std::ptr::null_mut(), ZigScrollViewport::Top);
        sv(std::ptr::null_mut(), ZigScrollViewport::Row(1));
    }

    #[test]
    fn compression_invalid_arguments() {
        unsafe {
            let mut activity: u64 = 0;
            let mut compression_result = CompressionResult::Unsupported;

            assert_eq!(
                Result::InvalidValue,
                compression_activity(std::ptr::null_mut(), &mut activity)
            );
            assert_eq!(
                Result::InvalidValue,
                compress(
                    std::ptr::null_mut(),
                    CompressionMode::Incremental as c_int,
                    &mut compression_result
                )
            );

            let t = new_t(80, 24);

            assert_eq!(Result::InvalidValue, compression_activity(t, std::ptr::null_mut()));
            assert_eq!(
                Result::InvalidValue,
                compress(t, CompressionMode::Incremental as c_int, std::ptr::null_mut())
            );
            assert_eq!(Result::InvalidValue, compress(t, -1, &mut compression_result));
            free(t);
        }
    }

    #[test]
    fn compression_activity_and_incremental_scheduling() {
        unsafe {
            let t = new_t(80, 24);

            let mut initial_activity: u64 = 0;
            assert_eq!(Result::Success, compression_activity(t, &mut initial_activity));

            vw(t, &compressible_history());

            let mut activity: u64 = 0;
            assert_eq!(Result::Success, compression_activity(t, &mut activity));
            assert_ne!(activity, initial_activity);

            let mut compression_result = CompressionResult::Unsupported;
            let mut done = false;
            for _ in 0..1_000 {
                assert_eq!(
                    Result::Success,
                    compress(t, CompressionMode::Incremental as c_int, &mut compression_result)
                );

                match compression_result {
                    CompressionResult::Pending => continue,
                    CompressionResult::Complete => {
                        done = true;
                        break;
                    }
                    CompressionResult::Unsupported => unreachable!(),
                }
            }
            assert!(done, "TestUnexpectedResult");

            // Compression changes storage representation, not the activity token.
            let mut final_activity: u64 = 0;
            assert_eq!(Result::Success, compression_activity(t, &mut final_activity));
            assert_eq!(activity, final_activity);

            assert_eq!(
                Result::Success,
                compress(t, CompressionMode::Full as c_int, &mut compression_result)
            );
            assert_eq!(CompressionResult::Complete, compression_result);
            free(t);
        }
    }

    #[test]
    fn reset_test() {
        let t = new_t(80, 24);
        vw(t, b"Hello");
        unsafe { reset(t) };
        assert_eq!("", plain(t));
        unsafe { free(t) };
    }

    #[test]
    fn reset_null() {
        unsafe { reset(std::ptr::null_mut()) };
    }

    #[test]
    fn resize_test() {
        let t = new_t(80, 24);
        assert_eq!(Result::Success, unsafe { resize(t, 40, 12, 9, 18) });
        assert_eq!(40, zt(t).cols);
        assert_eq!(12, zt(t).rows);
        unsafe { free(t) };
    }

    #[test]
    fn resize_null() {
        assert_eq!(Result::InvalidValue, unsafe { resize(std::ptr::null_mut(), 80, 24, 9, 18) });
    }

    #[test]
    fn resize_invalid_value() {
        let t = new_t(80, 24);
        assert_eq!(Result::InvalidValue, unsafe { resize(t, 0, 24, 9, 18) });
        assert_eq!(Result::InvalidValue, unsafe { resize(t, 80, 0, 9, 18) });
        unsafe { free(t) };
    }

    #[test]
    fn resize_shrinks_both_axes_with_cursor_at_bottom() {
        let t = new_t(80, 24);

        // CSI 24;1H -> park the cursor on the bottom row (1-based).
        vw(t, b"\x1b[24;1H");

        // Shrink both axes; pre-resize cursor.y sits past the new bottom row.
        // Previously this underflowed in PageList.resizeCols.
        assert_eq!(Result::Success, unsafe { resize(t, 79, 23, 8, 16) });
        assert_eq!(79, zt(t).cols);
        assert_eq!(23, zt(t).rows);
        unsafe { free(t) };
    }

    #[test]
    fn set_and_get_mode() {
        let t = new_t(80, 24);

        // DEC mode 25 (cursor_visible) defaults to true
        let mut config = ModeConfig {
            mode: mode_tag(25, false),
            value: false,
        };
        assert_eq!(Result::Success, gdat(t, TerminalData::Mode, o(&mut config)));
        assert!(config.value);

        // Set it to false
        config.value = false;
        assert_eq!(Result::Success, sopt(t, Option::Mode, p(&config)));
        assert_eq!(Result::Success, gdat(t, TerminalData::Mode, o(&mut config)));
        assert!(!config.value);

        // ANSI mode 4 (insert) defaults to false
        config.mode = mode_tag(4, true);
        assert_eq!(Result::Success, gdat(t, TerminalData::Mode, o(&mut config)));
        assert!(!config.value);

        config.value = true;
        assert_eq!(Result::Success, sopt(t, Option::Mode, p(&config)));
        assert_eq!(Result::Success, gdat(t, TerminalData::Mode, o(&mut config)));
        assert!(config.value);
        unsafe { free(t) };
    }

    #[test]
    fn set_mode_default_updates_current_and_reset_value() {
        let t = new_t(80, 24);

        let mut config = ModeConfig {
            mode: mode_tag(2027, false),
            value: true,
        };
        assert_eq!(Result::Success, sopt(t, Option::ModeDefault, p(&config)));
        assert!(zt(t).modes.get(Mode::GraphemeCluster));
        assert!(zt(t).modes.default.get(Mode::GraphemeCluster));

        config.value = false;
        assert_eq!(Result::Success, sopt(t, Option::Mode, p(&config)));
        assert!(!zt(t).modes.get(Mode::GraphemeCluster));
        assert!(zt(t).modes.default.get(Mode::GraphemeCluster));

        // Setting the default also unconditionally replaces the current value.
        config.value = true;
        assert_eq!(Result::Success, sopt(t, Option::ModeDefault, p(&config)));
        assert!(zt(t).modes.get(Mode::GraphemeCluster));

        config.value = false;
        assert_eq!(Result::Success, sopt(t, Option::Mode, p(&config)));
        vw(t, b"\x1bc");
        assert!(zt(t).modes.get(Mode::GraphemeCluster));

        config.value = false;
        assert_eq!(Result::Success, sopt(t, Option::ModeDefault, p(&config)));
        assert!(!zt(t).modes.get(Mode::GraphemeCluster));
        assert!(!zt(t).modes.default.get(Mode::GraphemeCluster));
        unsafe { free(t) };
    }

    #[test]
    fn set_mode_default_validates_configuration() {
        let t = new_t(80, 24);

        assert_eq!(Result::InvalidValue, sopt(t, Option::ModeDefault, std::ptr::null()));

        let mut config = ModeConfig {
            mode: mode_tag(9999, false),
            value: true,
        };
        assert_eq!(Result::InvalidValue, sopt(t, Option::ModeDefault, p(&config)));

        config.mode = mode_tag(1047, false);
        assert_eq!(Result::InvalidValue, sopt(t, Option::ModeDefault, p(&config)));
        unsafe { free(t) };
    }

    #[test]
    fn set_and_get_mode_null() {
        let mut config = ModeConfig {
            mode: mode_tag(25, false),
            value: true,
        };
        assert_eq!(Result::InvalidValue, sopt(std::ptr::null_mut(), Option::Mode, p(&config)));
        assert_eq!(
            Result::InvalidValue,
            gdat(std::ptr::null_mut(), TerminalData::Mode, o(&mut config))
        );
    }

    #[test]
    fn set_and_get_mode_validate_configuration() {
        let t = new_t(80, 24);

        assert_eq!(Result::InvalidValue, sopt(t, Option::Mode, std::ptr::null()));
        assert_eq!(Result::InvalidValue, gdat(t, TerminalData::Mode, std::ptr::null_mut()));

        let mut config = ModeConfig {
            mode: mode_tag(9999, false),
            value: true,
        };
        assert_eq!(Result::InvalidValue, sopt(t, Option::Mode, p(&config)));
        assert_eq!(Result::InvalidValue, gdat(t, TerminalData::Mode, o(&mut config)));
        unsafe { free(t) };
    }

    #[test]
    fn vt_write_test() {
        let t = new_t(80, 24);
        vw(t, b"Hello");
        assert_eq!("Hello", plain(t));
        unsafe { free(t) };
    }

    // CHUNK2
}
