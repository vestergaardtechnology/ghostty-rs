//! Port of `src/terminal/stream_terminal.zig`.
//!
//! STATUS: ported
//!
//! Port notes / deviations:
//! - `Handler` holds `&'a mut Terminal` (Zig: `*Terminal`). Tests that
//!   inspect the terminal while a stream is alive go through
//!   `s.handler.terminal`.
//! - Effects are plain function pointers taking `&mut Handler`, like Zig.
//!   Callbacks that need state use statics/thread-locals or recover their
//!   embedder from the handler (Zig uses `@fieldParentPtr` the same way).
//! - Clipboard read/write replies (`ClipboardReadState`,
//!   `KittyClipboardReadState`, `KittyClipboardWriteReplyState`): the
//!   reply state does not hold a pointer back to the handler (that would
//!   alias the `&mut Handler` the callback holds). A reply encodes its
//!   response (and notes a requested grant) in the state, and the handler
//!   applies it as soon as the callback returns. The bytes written and the
//!   grants recorded are the same; only a write the callback itself makes
//!   to the pty after replying is ordered before the reply.
//! - `paste` encodes into a buffer and then streams it to `write_pty` in
//!   `PASTE_CHUNK_SIZE` chunks (Zig streams through a buffered `PtyWriter`
//!   whose drain calls `write_pty`; the chunks are identical because the
//!   writer always fills its buffer before draining). The paste encoder
//!   borrows the terminal and grants, which can't coexist with the
//!   `&mut Handler` that `write_pty` takes.
//! - `Terminal.io()` is represented by [`Handler::io`], the entropy source
//!   for Kitty paste event one-time passwords.
//! - Allocation failure is not recoverable in Rust. The tests that force
//!   allocation failures with a `FailingAllocator` use the [`VT_TW`]
//!   tripwire at the same allocation points instead.

use std::ffi::c_void;
use std::fmt;
use std::io::Write as _;
use std::sync::OnceLock;

use base64::Engine as _;

use crate::simd::base64 as simd_base64;
use crate::terminal::apc;
use crate::terminal::build_options;
use crate::terminal::clipboard;
use crate::terminal::color;
use crate::terminal::csi;
use crate::terminal::dcs;
use crate::terminal::device_attributes;
use crate::terminal::device_status;
use crate::terminal::kitty::clipboard as kitty_clipboard;
use crate::terminal::kitty::clipboard_command::MetadataError;
use crate::terminal::kitty::clipboard_grants::Direction as GrantDirection;
use crate::terminal::kitty::clipboard_write::{DataError, Options as WriteStateOptions};
use crate::terminal::kitty::color as kitty_color;
use crate::terminal::kitty::dnd as kitty_dnd;
use crate::terminal::kitty::key::SetMode as KittyKeySetMode;
use crate::terminal::modes::{self, Mode};
use crate::terminal::mouse;
use crate::terminal::osc;
use crate::terminal::osc::parsers::program_status;
use crate::terminal::osc::parsers::semantic_prompt as osc_sp;
use crate::terminal::parser;
use crate::terminal::paste as paste_pkg;
use crate::terminal::sgr;
use crate::terminal::size_report;
use crate::terminal::stream::{self, Action};
use crate::terminal::sys;
use crate::terminal::terminal::{DeccolmMode, Resize, ResizeError, SwitchScreenMode, Terminal};
use crate::terminal::xt_checksum;
use crate::terminfo;
use crate::tripwire;

const LOG_TARGET: &str = "stream_terminal";

/// This is a Stream implementation that processes actions against
/// a Terminal and updates the Terminal state.
pub type Stream<'a> = stream::Stream<Handler<'a>>;

/// Failure points for tests (Rust-only, see the module docs). Each one
/// is an allocation in the Zig code that a test forces to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailPoint {
    /// The terminal title allocation (`Terminal.setTitle`).
    WindowTitle,
    /// The OSC 52 decode buffer.
    ClipboardDecode,
    /// The Kitty clipboard write transaction state.
    KittyClipboardWriteBegin,
}

/// Allocation failure (Zig: `error.OutOfMemory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfMemory;

impl fmt::Display for OutOfMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OutOfMemory")
    }
}

impl std::error::Error for OutOfMemory {}

/// Tripwire for [`FailPoint`].
pub static VT_TW: tripwire::Module<FailPoint, OutOfMemory> = tripwire::Module::new();

/// The error of a fallible VT action. Every error is logged and flags
/// `Handler::semantic_failure`.
type VtError = Box<dyn std::error::Error>;

/// Maximum byte length accepted for `terminfo_name`
/// (`Handler.max_terminfo_name_bytes`).
pub const MAX_TERMINFO_NAME_BYTES: usize = 128;

/// The size of the chunks a paste streams to write_pty in
/// (`Handler.paste_chunk_size`).
pub const PASTE_CHUNK_SIZE: usize = 4096;

/// A stream handler that updates terminal state. By default, it is
/// readonly in the sense that it only updates terminal state and ignores
/// all other sequences that require a response or otherwise have side
/// effects (e.g. clipboards).
///
/// You can manually set various effects callbacks in the `effects` field
/// to implement certain effects such as bells, titles, clipboard, etc.
pub struct Handler<'a> {
    /// The terminal state to modify.
    pub terminal: &'a mut Terminal,

    /// True after an error prevented a terminal-owned semantic update.
    ///
    /// When an error happens during terminal processing, streams continue
    /// forward and remain best-effort. A terminal can't really stop in
    /// the middle it must go on. But this is flagged to true to let
    /// consumers know some sort of unhandle-able error state happened
    /// (e.g. an allocation failure).
    ///
    /// Only non-handled outcomes set this. Gracefully handled outcomes
    /// that don't meaningfully negatively impact the terminal state
    /// such as hitting Kitty image limits, failure to write a response,
    /// do not flag this.
    pub semantic_failure: bool,

    /// Callbacks for certain effects that handlers may have. These
    /// may or may not fully replace internal handling of certain effects,
    /// but they allow for the handler to trigger or query external
    /// effects.
    pub effects: Effects,

    /// Whether CSI 21 t may report the terminal title. This is disabled by
    /// default because reporting an attacker-controlled title to the pty can
    /// inject text into the input stream of the foreground process.
    pub title_report: bool,

    /// Whether DECRQCRA may report the checksum of an area of the screen,
    /// and XTCHECKSUM may change how it's calculated. This is disabled by
    /// default because a program can checksum one cell at a time and so
    /// read back everything on the screen, including output from other
    /// programs.
    pub xt_checksum_report: bool,

    /// Collects APC strings (`ESC _ ... ESC \`) as they arrive. Programs
    /// use APC to send commands to the terminal, such as Kitty graphics.
    ///
    /// A command runs only when its APC ends with the string terminator.
    /// If the program cancels the APC with CAN or SUB, or another 8-bit
    /// control byte aborts it, the command is discarded. For example, this
    /// input stores no image because CAN (0x18) cancels it:
    ///
    /// ```text
    /// ESC _ G a=t,f=24,s=1,v=1,i=7;AAAA 0x18
    /// ```
    pub apc_handler: apc::Handler,

    /// The DCS command handler maintains state for DCS queries.
    pub dcs_handler: dcs::Handler,

    /// The in-flight Kitty clipboard protocol (OSC 5522) write
    /// transaction, if any. None means no transaction is active.
    /// Heap-allocated since transactions are rare and short-lived.
    pub kitty_clipboard_write: Option<Box<kitty_clipboard::WriteState>>,

    /// Kitty clipboard protocol (OSC 5522) session password grants,
    /// recorded when a clipboard_read or clipboard_write reply asks to
    /// remember the user's decision.
    pub kitty_clipboard_grants: kitty_clipboard::Grants,

    /// Maximum total decoded bytes accumulated by one Kitty clipboard
    /// protocol (OSC 5522) write transaction, captured when the
    /// transaction begins. Data beyond the limit fails the transaction
    /// with EFBIG.
    pub kitty_clipboard_write_max_bytes: usize,

    /// Called for escape sequences this library does not implement, so you
    /// can implement them yourself. See `UnknownSequence` for the kinds of
    /// sequences that are reported.
    ///
    /// Nothing is reported until you also set a byte limit for each kind
    /// you want. The limits default to zero, which turns reporting off:
    ///
    ///   - APC: `apc_handler.unknown_max_bytes`
    ///   - OSC: `stream::Options::osc_unknown_max_bytes` when creating the
    ///     stream, or `stream.parser.osc_parser.unknown_max_bytes` later.
    ///
    /// This example handles a made-up OSC 7400 and ignores everything
    /// else:
    ///
    /// ```ignore
    /// fn on_unknown(handler: &mut Handler<'_>, seq: UnknownSequence<'_>) {
    ///     let UnknownSequence::Osc(v) = seq else { return };
    ///
    ///     // Match the number and its ";" so "74000;" is not included.
    ///     let prefix = b"7400;";
    ///     if v.truncated || !v.content.starts_with(prefix) { return; }
    ///     handle_status(handler, &v.content[prefix.len()..]);
    /// }
    ///
    /// let mut handler = Handler::init(&mut terminal);
    /// handler.unknown_sequence = Some(on_unknown);
    /// let mut stream = Stream::init(stream::Options {
    ///     osc_unknown_max_bytes: 4096,
    ///     ..stream::Options::new(handler)
    /// });
    /// ```
    ///
    /// Only complete sequences are reported. A sequence the program
    /// cancels with CAN or SUB, or that another 8-bit control byte
    /// aborts, is discarded without calling this.
    ///
    /// The callback runs while the stream processes input. It may write a
    /// reply to the pty, and the reply stays in order with the terminal's
    /// own replies. It must not feed more input to the same stream.
    pub unknown_sequence: Option<UnknownSequenceFn>,

    /// The name of the terminfo entry this terminal runs as, reported in
    /// response to an XTGETTCAP query for "TN".
    ///
    /// Empty names and names longer than `MAX_TERMINFO_NAME_BYTES` are
    /// silently ignored.
    pub terminfo_name: Option<&'a [u8]>,

    /// The entropy source for Kitty paste event one-time passwords (Zig:
    /// `terminal.io()`). Defaults to the OS CSPRNG.
    pub io: sys::IoRandomSecureFn,
}

/// The `unknown_sequence` callback type.
pub type UnknownSequenceFn = for<'h, 't, 's> fn(&'h mut Handler<'t>, UnknownSequence<'s>);

/// (`Handler.Effects`)
#[derive(Clone, Copy)]
pub struct Effects {
    /// Called when the terminal needs to write data back to the pty,
    /// e.g. in response to a DECRQM query. The data is only valid
    /// during the lifetime of the call so callers must copy it
    /// if it needs to be stored or used after the call returns.
    pub write_pty: Option<for<'h, 't, 'd> fn(&'h mut Handler<'t>, &'d [u8])>,

    /// Called when the bell is rung (BEL).
    pub bell: Option<for<'h, 't> fn(&'h mut Handler<'t>)>,

    /// Called when the running program requests a desktop notification
    /// via OSC 9 or OSC 777. The title and body are borrowed and only
    /// valid for the duration of the callback.
    pub desktop_notification: Option<for<'h, 't, 'n> fn(&'h mut Handler<'t>, stream::ShowDesktopNotification<'n>)>,

    /// Called when drag and drop protocol state changes in a way the
    /// embedder may need to act on: the running program registering
    /// or unregistering to accept drops, answering a drag, or
    /// concluding a drop. The event says what changed; the details
    /// are read from `handler.terminal.kitty_dnd` (Kitty's OSC 72 is
    /// the only drag and drop protocol today). Native drag events
    /// flow the other way, by calling `kitty::dnd::State` directly.
    pub drag_and_drop: Option<for<'h, 't> fn(&'h mut Handler<'t>, kitty_dnd::Event)>,

    /// Called in response to a color scheme DSR query (CSI ? 996 n).
    /// Returns the current color scheme. Return None to silently
    /// ignore the query.
    pub color_scheme: Option<for<'h, 't> fn(&'h mut Handler<'t>) -> Option<device_status::ColorScheme>>,

    /// Called in response to a device attributes query (CSI c,
    /// CSI > c, CSI = c). Returns the response to encode and
    /// write back to the pty.
    pub device_attributes: Option<for<'h, 't> fn(&'h mut Handler<'t>) -> device_attributes::Attributes>,

    /// Called in response to ENQ (0x05). Returns the raw response
    /// bytes to write back to the pty. The returned memory must be
    /// valid for the lifetime of the call.
    pub enquiry: Option<for<'h, 't> fn(&'h mut Handler<'t>) -> &'h [u8]>,

    /// Called for XTWINOPS size queries (CSI 14/16/18/19 t) and when VT input
    /// enables in-band size reports (mode 2048). Returns the current
    /// terminal geometry used for encoding. Return None to suppress the
    /// XTWINOPS response or mode 2048 report.
    pub size: Option<for<'h, 't> fn(&'h mut Handler<'t>) -> Option<size_report::Size>>,

    /// Called when the terminal title changes via escape sequences
    /// (e.g. OSC 0/2). The new title can be queried via
    /// handler.terminal.get_title().
    pub title_changed: Option<for<'h, 't> fn(&'h mut Handler<'t>)>,

    /// Called when the terminal pwd changes via escape sequences
    /// (e.g. OSC 7). The new pwd can be queried via
    /// handler.terminal.get_pwd().
    pub pwd_changed: Option<for<'h, 't> fn(&'h mut Handler<'t>)>,

    /// Called when the running program reports progress via OSC 9;4.
    pub progress_report: Option<for<'h, 't> fn(&'h mut Handler<'t>, osc::ProgressReport)>,

    /// Called when the running program sends a program status report
    /// (OSC 7501). Programs use these to say what they are doing, such
    /// as working, done, or waiting on the user. The report's
    /// text is only valid until this returns, so copy what you keep.
    ///
    /// A full reset (RIS) removes every record. When that happens, the
    /// terminal calls this with a `clear` report without an id, and
    /// then calls `reset`.
    ///
    /// Setting this also makes the terminal answer the support query,
    /// `OSC 7501 ; ?`, through `write_pty`. The reply is
    /// `osc::program_status::QUERY_REPLY`, which lists the states and
    /// kinds the terminal accepts. While this is None, the query gets
    /// no reply, so a program that asks sees the protocol as
    /// unsupported. Asking is optional, so reports may still arrive.
    /// They are dropped.
    pub program_status: Option<for<'h, 't, 'r> fn(&'h mut Handler<'t>, program_status::Report<'r>)>,

    /// Called when the shell reports a step of a command through
    /// shell integration: a prompt starts, input starts, output
    /// starts, or the command ends. See `SemanticPrompt` for the
    /// steps and an example.
    ///
    /// The terminal has already applied the sequence when this is
    /// called. A sequence the terminal rejects is never reported.
    pub semantic_prompt: Option<for<'h, 't, 's> fn(&'h mut Handler<'t>, SemanticPrompt<'s>)>,

    /// Called after the running program performs a full reset (RIS,
    /// `ESC c`). The terminal has already reset itself, which clears
    /// the screen, scrollback, title, and pwd. `title_changed` and
    /// `pwd_changed` are not called for this, so update anything that
    /// shows them here.
    ///
    /// A full reset also removes the progress report and program
    /// status records, and `progress_report` and `program_status` are
    /// called for that before this is called. A soft reset (DECSTR)
    /// doesn't call this.
    ///
    /// Shells don't report the end of a command that a reset
    /// interrupts, so clear any state you keep for the current
    /// command here.
    pub reset: Option<for<'h, 't> fn(&'h mut Handler<'t>)>,

    /// Called when the running program asks to resize the window. See
    /// `ResizeWindow` for the kinds of requests. The terminal isn't
    /// resized by this; it's up to the embedder whether to allow the
    /// request, and if so to resize the terminal itself.
    pub resize_window: Option<for<'h, 't> fn(&'h mut Handler<'t>, ResizeWindow)>,

    /// Called when the running program writes to a clipboard.
    pub clipboard_write: Option<for<'h, 't, 'w> fn(&'h mut Handler<'t>, clipboard::Write<'w>)>,

    /// Called when the running program requests clipboard contents
    /// (OSC 52 with a "?" payload, or a Kitty clipboard (OSC 5522)
    /// read). Answering one lets the program read the user's
    /// clipboard, so the embedder is expected to mediate consent.
    ///
    /// Reads are synchronous: the callback must answer through
    /// `read.reply` before it returns, so an embedder that needs to
    /// ask the user must block (e.g. run a modal prompt) while the
    /// stream waits. Returning without a reply, or replying with any
    /// failure, answers the program with an empty clipboard (OSC 52)
    /// or the matching protocol status (OSC 5522) so it doesn't hang.
    /// If this is None, OSC 52 reads are ignored and OSC 5522 reads
    /// are refused with EPERM.
    ///
    /// OSC 5522 requests carry the program's MIME list, name, and
    /// password grant state; a reply that sets `remember` records a
    /// session grant so later requests with the same password arrive
    /// with `granted` set. Kitty itself serves a request for only the
    /// targets listing (`list` with no `mimes`) without prompting.
    ///
    /// Installing this also enables Kitty paste events (mode 5522):
    /// `paste` sends the program an event instead of the text, and
    /// the program's follow-up read arrives here with `granted` set
    /// since the user already pasted. See `paste`.
    pub clipboard_read: Option<for<'h, 't, 'r> fn(&'h mut Handler<'t>, clipboard::Read<'r>)>,

    /// Called in response to an XTVERSION query. Returns the version
    /// string to report (e.g. "ghostty 1.2.3"). The returned memory
    /// must be valid for the lifetime of the call. The maximum length
    /// is 256 bytes; longer strings will be silently ignored.
    pub xtversion: Option<for<'h, 't> fn(&'h mut Handler<'t>) -> &'h [u8]>,

    /// Called with `true` when the running program asks the terminal
    /// to stop updating the screen, and with `false` when it allows
    /// updates again. The time in between is a "render hold". Programs
    /// use a hold so that the user never sees a half-drawn frame.
    ///
    /// Today the only source of a hold is synchronized output (mode
    /// 2026). The hold begins when VT input sets the mode. It ends
    /// when VT input resets the mode, on a full reset, and on a resize
    /// through `Handler::resize`. The calls always come in pairs:
    /// setting the mode during a hold does nothing, and neither does
    /// resetting it when there is no hold. Writing `terminal.modes`
    /// directly never calls this.
    ///
    /// When a hold begins, nothing after the sequence that began it
    /// has been processed yet, so the terminal contains exactly the
    /// frame the program wants left on screen. A renderer can capture
    /// that frame from within this callback (e.g. `RenderState::update`)
    /// and then skip updates until the hold ends. Checking the mode
    /// before each draw instead can't do this. It leaves whatever was
    /// drawn last on screen, and it loses a finished frame entirely
    /// when one hold ends and the next begins between two draws.
    ///
    /// The terminal has no clock so it never ends a hold on its own.
    /// The caller must use a timeout so that a program that never
    /// releases its hold can't freeze the screen.
    pub render_hold: Option<for<'h, 't> fn(&'h mut Handler<'t>, bool)>,
}

impl Effects {
    /// No effects means that the stream effectively becomes readonly
    /// that only affects pure terminal state and ignores all side
    /// effects beyond that.
    pub const READONLY: Effects = Effects {
        bell: None,
        clipboard_read: None,
        clipboard_write: None,
        color_scheme: None,
        desktop_notification: None,
        device_attributes: None,
        drag_and_drop: None,
        enquiry: None,
        progress_report: None,
        program_status: None,
        reset: None,
        semantic_prompt: None,
        resize_window: None,
        size: None,
        render_hold: None,
        title_changed: None,
        pwd_changed: None,
        write_pty: None,
        xtversion: None,
    };
}

impl Default for Effects {
    fn default() -> Self {
        Effects::READONLY
    }
}

/// A sequence this library does not implement, passed to the
/// `unknown_sequence` callback. The data is only valid until the
/// callback returns. Copy it if you need it later.
///
/// More kinds of sequences may be added later, so match on this with
/// a `_` arm that ignores kinds you don't handle.
/// (`Handler.UnknownSequence`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnknownSequence<'a> {
    /// An APC sequence (`ESC _`) whose identifier is not implemented.
    Apc(UnknownSequenceString<'a>),

    /// An OSC sequence (`ESC ]`) whose number is not implemented.
    Osc(UnknownSequenceOsc<'a>),
}

/// Content between a string sequence's introducer and terminator.
/// (`Handler.UnknownSequence.String`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownSequenceString<'a> {
    pub content: &'a [u8],
    pub truncated: bool,
}

/// An OSC sequence whose number is not implemented.
/// (`Handler.UnknownSequence.Osc`)
pub type UnknownSequenceOsc<'a> = osc::Unknown<'a>;

/// A request from the running program to resize the window, passed to
/// the `resize_window` effect.
///
/// More kinds of requests may be added later. (`Handler.ResizeWindow`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResizeWindow {
    /// Resize the window's text area to a size in rows and columns
    /// (CSI 8 ; rows ; columns t). A zero means the program asked to
    /// keep that dimension.
    Grid(ResizeWindowGrid),
}

/// (`Handler.ResizeWindow.Grid`)
pub type ResizeWindowGrid = stream::ResizeWindow;

/// A shell integration event, passed to the `semantic_prompt` effect.
///
/// Many shells tell the terminal where each prompt, command, and
/// command output begins. Each command goes through four steps, and
/// the effect is called once for each step the shell reports:
///
///   1. `PromptStart`: the shell starts drawing a prompt.
///   2. `InputStart`: the prompt is drawn and the user can type.
///   3. `OutputStart`: the user submitted the command and it runs.
///   4. `CommandEnd`: the command finished.
///
/// Then the shell draws the next prompt and the steps start over.
///
/// Shells differ in what they report. Many don't send the command
/// line or the exit code, and some skip steps, so handle each event
/// on its own instead of expecting a strict order. A shell may also
/// start the same prompt more than once, for example when it redraws
/// the prompt after a resize, so treat a repeated `PromptStart` as
/// harmless.
///
/// The event describes what happened, not how the shell said it.
/// Today events come from OSC 133. More shell integration protocols
/// may report through this same type later.
///
/// The strings are only valid until the callback returns. Copy them
/// if you need them later.
///
/// This example logs each command's result:
///
/// ```ignore
/// fn on_semantic_prompt(_: &mut Handler<'_>, event: SemanticPrompt<'_>) {
///     match event.kind {
///         SemanticPromptKind::CommandEnd => match event.exit_code {
///             Some(code) => log::info!("command exited with {}", code),
///             None => log::info!("command finished"),
///         },
///         _ => {}
///     }
/// }
///
/// let mut handler = Handler::init(&mut terminal);
/// handler.effects.semantic_prompt = Some(on_semantic_prompt);
/// ```
/// (`Handler.SemanticPrompt`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticPrompt<'a> {
    /// Which step of the command this event reports.
    pub kind: SemanticPromptKind,

    /// Which prompt is starting, for `PromptStart`. Always
    /// `Primary` for other kinds.
    pub prompt_kind: SemanticPromptPromptKind,

    /// The command's exit code, for `CommandEnd` when the shell
    /// reported one. None otherwise.
    pub exit_code: Option<i32>,

    /// The command line about to run, for `OutputStart`. The shell
    /// sends it encoded, and this is the decoded text. Empty if the
    /// shell didn't send one or it couldn't be decoded.
    pub command: &'a [u8],

    /// A description of what went wrong, for `CommandEnd` when the
    /// shell sent one. Empty otherwise. Few shells send this, and the
    /// exit code is the usual way to tell whether a command failed.
    pub err: &'a [u8],
}

impl<'a> SemanticPrompt<'a> {
    /// An event of the given kind with every other field at its default.
    pub const fn new(kind: SemanticPromptKind) -> SemanticPrompt<'a> {
        SemanticPrompt {
            kind,
            prompt_kind: SemanticPromptPromptKind::Primary,
            exit_code: None,
            command: b"",
            err: b"",
        }
    }
}

/// C: GhosttySemanticPromptKind (`Handler.SemanticPrompt.Kind`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub enum SemanticPromptKind {
    /// Never reported. This exists so that a zeroed C value is not
    /// mistaken for a real event.
    #[default]
    Invalid,

    /// The shell started drawing a prompt.
    PromptStart,

    /// The prompt is drawn and the user can start typing.
    InputStart,

    /// The user submitted the command and it started running.
    OutputStart,

    /// The command finished running.
    CommandEnd,
}

/// C: GhosttySemanticPromptPromptKind (`Handler.SemanticPrompt.PromptKind`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub enum SemanticPromptPromptKind {
    /// The main prompt shown before each command. This is used
    /// when the shell doesn't say which prompt it is drawing.
    #[default]
    Primary,

    /// A prompt drawn at the right edge of the line.
    Right,

    /// A prompt at the start of an extra line of a command that
    /// spans several lines.
    Continuation,

    /// Another prompt for an extra line of input, such as bash's
    /// PS2. Shells differ in whether they report extra lines as
    /// continuation or secondary prompts, so most callers should
    /// treat the two the same.
    Secondary,
}

/// A paste request; see `Handler::paste`. (`Handler.Paste`)
pub type Paste<'a> = paste_pkg::Request<'a>;

/// (`Handler.PasteError`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteError {
    /// Allocation failure.
    OutOfMemory,

    /// No secure entropy for a paste event's one-time password.
    EntropyUnavailable,

    /// The data could inject commands and allow_unsafe was false.
    /// Nothing was written.
    UnsafePaste,

    /// The contents reader failed. Nothing was written.
    ReadFailed,

    /// No write_pty effect is set, so nothing can be written.
    NoWritePty,
}

impl fmt::Display for PasteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for PasteError {}

// Hex-encoded "TN", the XTGETTCAP key naming the terminfo entry.
// The static map also carries this key with Ghostty's own name, so
// it is intercepted before the lookup: an embedder that never
// configured a name must not be reported as Ghostty's entry.
const ENCODED_TN_KEY: &[u8] = b"544E";

/// The XTGETTCAP lookup table (Zig builds it at comptime).
fn xtgettcap_map() -> &'static terminfo::source::XtgettcapMap {
    static MAP: OnceLock<terminfo::source::XtgettcapMap> = OnceLock::new();
    MAP.get_or_init(|| terminfo::GHOSTTY.xtgettcap_map())
}

/// The MIME list of an OSC 52 read.
const TEXT_PLAIN_MIMES: &[&[u8]] = &[b"text/plain"];

/// A fixed-buffer writer helper: run `f` against `buf` and return the
/// number of bytes written (Zig: `std.Io.Writer.fixed`).
fn write_fixed(buf: &mut [u8], f: impl FnOnce(&mut &mut [u8]) -> std::io::Result<()>) -> std::io::Result<usize> {
    let total = buf.len();
    let mut writer: &mut [u8] = buf;
    f(&mut writer)?;
    Ok(total - writer.len())
}

impl<'a> Handler<'a> {
    /// Maximum byte length accepted for `terminfo_name`.
    pub const MAX_TERMINFO_NAME_BYTES: usize = MAX_TERMINFO_NAME_BYTES;

    /// The size of the chunks a paste streams to write_pty in.
    pub const PASTE_CHUNK_SIZE: usize = PASTE_CHUNK_SIZE;

    pub fn init(terminal: &'a mut Terminal) -> Handler<'a> {
        Handler {
            terminal,
            semantic_failure: false,
            effects: Effects::READONLY,
            title_report: false,
            xt_checksum_report: false,
            apc_handler: apc::Handler::default(),
            dcs_handler: dcs::Handler::default(),
            kitty_clipboard_write: None,
            kitty_clipboard_grants: kitty_clipboard::Grants::default(),
            kitty_clipboard_write_max_bytes: kitty_clipboard::MAX_WRITE_SIZE,
            unknown_sequence: None,
            terminfo_name: None,
            io: sys::os_random_secure,
        }
    }

    pub fn deinit(&mut self) {
        self.kitty_clipboard_abort();
        self.kitty_clipboard_grants = kitty_clipboard::Grants::default();
        self.apc_handler.deinit();
        self.dcs_handler = dcs::Handler::default();
    }

    /// Resize the terminal and apply any side effects (if supported)
    /// as a result of that.
    ///
    /// This is different than a direct `Terminal::resize` operation
    /// because it also handles the side effects like mode 2048 in-band
    /// size reports if write_pty is set.
    pub fn resize(&mut self, value: Resize) -> Result<(), ResizeError> {
        // Resize always turns off synchronized output, ending its hold.
        let sync = self.terminal.modes.get(Mode::SynchronizedOutput);
        self.terminal.resize(value)?;
        if sync {
            self.render_hold(false);
        }

        // Mode 2048 reports require complete, current cell pixel geometry.
        let Some(cell_size) = value.cell_size_px else {
            return Ok(());
        };

        // If we have no in-band size reports enabled then do nothing.
        if !self.terminal.modes.get(Mode::InBandSizeReports) {
            return Ok(());
        }

        // If we have no write_pty effect, do nothing.
        let Some(write_pty) = self.effects.write_pty else {
            return Ok(());
        };

        // The maximum mode-2048 response is covered by size_report's maximum
        // value test. Reserve the final byte for the callback's sentinel.
        let mut buf = [0u8; 128];
        let n = write_fixed(&mut buf[..127], |w| {
            size_report::encode(
                w,
                size_report::Style::Mode2048,
                size_report::Size {
                    rows: value.rows,
                    columns: value.cols,
                    cell_width: cell_size.width,
                    cell_height: cell_size.height,
                },
            )
        })
        .expect("mode 2048 report fits");
        write_pty(self, &buf[..n]);
        Ok(())
    }

    /// Paste into the terminal, applying the terminal's current state
    /// as necessary to owner mode 5522, bracketed paste, unsafe paste, etc.
    /// Returns true if anything was written to the pty.
    ///
    /// The output streams to write_pty in chunks of `PASTE_CHUNK_SIZE`.
    /// The contents are read at most once and only the pasted text
    /// representation is ever read, buffered whole while it is checked
    /// and encoded; see `terminal::paste`.
    pub fn paste(&mut self, req: Paste<'_>) -> Result<bool, PasteError> {
        if self.effects.write_pty.is_none() {
            return Err(PasteError::NoWritePty);
        }

        let mut out: Vec<u8> = Vec::new();
        let io = self.io;
        let has_clipboard_read = self.effects.clipboard_read.is_some();
        let result = paste_pkg::paste(
            paste_pkg::Context {
                terminal: &*self.terminal,
                writer: &mut out,
                // Paste events need the program's follow-up Kitty
                // clipboard read served.
                kitty_clipboard: if has_clipboard_read {
                    Some(paste_pkg::KittyClipboard {
                        grants: &mut self.kitty_clipboard_grants,
                        io,
                    })
                } else {
                    None
                },
            },
            req,
        );

        // Delivered on error too: a partial paste has its frame closed
        // and the program must see that.
        for chunk in out.chunks(PASTE_CHUNK_SIZE) {
            self.write_pty(chunk);
        }

        result.map_err(|err| match err {
            // The pty writer never fails.
            paste_pkg::Error::WriteFailed => unreachable!(),
            paste_pkg::Error::ReadFailed => PasteError::ReadFailed,
            paste_pkg::Error::OutOfMemory => PasteError::OutOfMemory,
            paste_pkg::Error::UnsafePaste => PasteError::UnsafePaste,
            paste_pkg::Error::EntropyUnavailable => PasteError::EntropyUnavailable,
        })
    }

    fn unknown_sequence_(&mut self, value: UnknownSequence<'_>) {
        let Some(func) = self.unknown_sequence else {
            return;
        };
        func(self, value);
    }

    #[inline]
    fn vt_fallible(&mut self, action: Action<'_>) -> Result<(), VtError> {
        match action {
            Action::Print(v) => self.terminal.print(v.cp)?,
            Action::PrintSlice(v) => self.terminal.print_slice(v.cps)?,
            Action::PrintRepeat(v) => self.terminal.print_repeat(v)?,
            Action::Backspace => self.terminal.backspace(),
            Action::CarriageReturn => self.terminal.carriage_return(),
            Action::Linefeed => self.terminal.linefeed()?,
            Action::Index => self.terminal.index()?,
            Action::NextLine => {
                self.terminal.index()?;
                self.terminal.carriage_return();
            }
            Action::ReverseIndex => self.terminal.reverse_index(),
            Action::BackIndex => self.terminal.back_index(),
            Action::ForwardIndex => self.terminal.forward_index(),
            Action::CursorUp(v) => self.terminal.cursor_up(v.value as usize),
            Action::CursorDown(v) => self.terminal.cursor_down(v.value as usize),
            Action::CursorLeft(v) => self.terminal.cursor_left(v.value as usize),
            Action::CursorRight(v) => self.terminal.cursor_right(v.value as usize),
            Action::CursorPos(v) => self.terminal.set_cursor_pos(v.row as usize, v.col as usize),
            Action::CursorCol(v) => self.terminal.set_cursor_col(v.value as usize),
            Action::CursorRow(v) => self.terminal.set_cursor_row(v.value as usize),
            Action::CursorColRelative(v) => self.terminal.cursor_col_relative(v.value as usize),
            Action::CursorRowRelative(v) => self.terminal.cursor_row_relative(v.value as usize),
            Action::CursorStyle(v) => self.terminal.set_cursor_style(v),
            Action::EraseDisplayBelow(v) => self.terminal.erase_display(csi::EraseDisplay::Below, v),
            Action::EraseDisplayAbove(v) => self.terminal.erase_display(csi::EraseDisplay::Above, v),
            Action::EraseDisplayComplete(v) => self.terminal.erase_display(csi::EraseDisplay::Complete, v),
            Action::EraseDisplayScrollback(v) => self.terminal.erase_display(csi::EraseDisplay::Scrollback, v),
            Action::EraseDisplayScrollComplete(v) => {
                self.terminal.erase_display(csi::EraseDisplay::ScrollComplete, v)
            }
            Action::EraseLineRight(v) => self.terminal.erase_line(csi::EraseLine::RIGHT, v),
            Action::EraseLineLeft(v) => self.terminal.erase_line(csi::EraseLine::LEFT, v),
            Action::EraseLineComplete(v) => self.terminal.erase_line(csi::EraseLine::COMPLETE, v),
            Action::EraseLineRightUnlessPendingWrap(v) => {
                self.terminal.erase_line(csi::EraseLine::RIGHT_UNLESS_PENDING_WRAP, v)
            }
            Action::DeleteChars(v) => self.terminal.delete_chars(v),
            Action::EraseChars(v) => self.terminal.erase_chars(v),
            Action::InsertLines(v) => self.terminal.insert_lines(v),
            Action::InsertBlanks(v) => self.terminal.insert_blanks(v),
            Action::InsertColumns(v) => self.terminal.insert_columns(v),
            Action::DeleteColumns(v) => self.terminal.delete_columns(v),
            Action::DeleteLines(v) => self.terminal.delete_lines(v),
            Action::ScrollUp(v) => self.terminal.scroll_up(v)?,
            Action::ScrollDown(v) => self.terminal.scroll_down(v),
            Action::HorizontalTab(v) => self.horizontal_tab(v),
            Action::HorizontalTabBack(v) => self.horizontal_tab_back(v),
            Action::TabClearCurrent => self.terminal.tab_clear(csi::TabClear::CURRENT),
            Action::TabClearAll => self.terminal.tab_clear(csi::TabClear::ALL),
            Action::TabSet => self.terminal.tab_set(),
            Action::TabReset => self.terminal.tab_reset(),
            Action::SetMode(v) => self.set_mode(v.mode, true)?,
            Action::ResetMode(v) => self.set_mode(v.mode, false)?,
            Action::SaveMode(v) => self.terminal.modes.save(v.mode),
            Action::RestoreMode(v) => {
                let prev = self.terminal.modes.get(v.mode);
                let restored = self.terminal.modes.restore(v.mode);

                // Restore writes the value directly. Put the old value
                // back for synchronized output so that setMode can see
                // the change and report the render hold.
                if v.mode == Mode::SynchronizedOutput {
                    self.terminal.modes.set(v.mode, prev);
                }

                self.set_mode(v.mode, restored)?;
            }
            Action::TopAndBottomMargin(v) => self
                .terminal
                .set_top_and_bottom_margin(v.top_left as usize, v.bottom_right as usize),
            Action::LeftAndRightMargin(v) => self
                .terminal
                .set_left_and_right_margin(v.top_left as usize, v.bottom_right as usize),
            Action::LeftAndRightMarginAmbiguous => {
                if self.terminal.modes.get(Mode::EnableLeftAndRightMargin) {
                    self.terminal.set_left_and_right_margin(0, 0);
                } else {
                    self.terminal.save_cursor();
                }
            }
            Action::SaveCursor => self.terminal.save_cursor(),
            Action::RestoreCursor => self.terminal.restore_cursor(),
            Action::InvokeCharset(v) => self.terminal.invoke_charset(v.bank, v.charset, v.locking),
            Action::ConfigureCharset(v) => self.terminal.configure_charset(v.slot, v.charset),
            Action::SetAttribute(v) => match v {
                sgr::Attribute::Unknown(_) => {}
                _ => self.terminal.set_attribute(v)?,
            },
            Action::ProtectedModeOff => self.terminal.set_protected_mode(crate::terminal::ansi::ProtectedMode::Off),
            Action::ProtectedModeIso => self.terminal.set_protected_mode(crate::terminal::ansi::ProtectedMode::Iso),
            Action::ProtectedModeDec => self.terminal.set_protected_mode(crate::terminal::ansi::ProtectedMode::Dec),
            Action::MouseShiftCapture(v) => {
                self.terminal.flags.mouse_shift_capture = if v {
                    crate::terminal::terminal::MouseShiftCapture::True
                } else {
                    crate::terminal::terminal::MouseShiftCapture::False
                }
            }
            Action::XtChecksumExtension(v) => {
                if self.xt_checksum_report {
                    self.terminal.flags.xt_checksum = v.flags;
                }
            }
            Action::KittyKeyboardPush(v) => self.terminal.screens.active.kitty_keyboard.push(v.flags),
            Action::KittyKeyboardPop(v) => self.terminal.screens.active.kitty_keyboard.pop(v as usize),
            Action::KittyKeyboardSet(v) => self
                .terminal
                .screens
                .active
                .kitty_keyboard
                .set(KittyKeySetMode::Set, v.flags),
            Action::KittyKeyboardSetOr(v) => self
                .terminal
                .screens
                .active
                .kitty_keyboard
                .set(KittyKeySetMode::Or, v.flags),
            Action::KittyKeyboardSetNot(v) => self
                .terminal
                .screens
                .active
                .kitty_keyboard
                .set(KittyKeySetMode::Not, v.flags),
            Action::ModifyKeyFormat(v) => {
                self.terminal.flags.modify_other_keys_2 = false;
                if v == crate::terminal::ansi::ModifyKeyFormat::OtherKeysNumeric {
                    self.terminal.flags.modify_other_keys_2 = true;
                }
            }
            Action::ActiveStatusDisplay(v) => self.terminal.status_display = v,
            Action::Decaln => self.terminal.decaln()?,
            Action::SoftReset => self.terminal.soft_reset(),
            Action::FullReset => {
                // A reset turns off synchronized output, ending its hold.
                let sync = self.terminal.modes.get(Mode::SynchronizedOutput);
                self.terminal.full_reset();
                if sync {
                    self.render_hold(false);
                }

                // Full reset clears grants
                self.kitty_clipboard_grants = kitty_clipboard::Grants::default();

                // Clear the progress bar and program status records
                self.progress_report(osc::ProgressReport::new(osc::ProgressReportState::Remove));
                self.program_status_report(program_status::Report {
                    state: program_status::State::Clear,
                    data: b"",
                });

                if let Some(func) = self.effects.reset {
                    func(self);
                }
            }
            Action::StartHyperlink(v) => self.terminal.screens.active.start_hyperlink(v.uri, v.id)?,
            Action::EndHyperlink => self.terminal.screens.active.end_hyperlink(),
            Action::SemanticPrompt(v) => self.semantic_prompt(v)?,
            Action::MouseShape(v) => self.terminal.mouse_shape = v,
            Action::MouseShapeReset => self.terminal.mouse_shape = mouse::Shape::Text,
            Action::ColorOperation(v) => {
                if let Err(err) = self.color_operation(&v.requests, v.terminator) {
                    log::warn!(target: LOG_TARGET, "error reporting OSC color err={}", err);
                }
            }
            Action::KittyColorReport(v) => {
                if let Err(err) = self.kitty_color_operation(&v) {
                    log::warn!(target: LOG_TARGET, "error reporting Kitty colors err={}", err);
                }
            }
            Action::KittyClipboard(v) => {
                if let Err(err) = self.kitty_clipboard(v) {
                    // Clipboard operations are external effects, not terminal
                    // state; a failed transaction was already answered.
                    log::warn!(target: LOG_TARGET, "error handling kitty clipboard err={}", err);
                }
            }

            // APC
            Action::ApcStart => self.apc_handler.start(),
            Action::ApcPut(v) => self.apc_handler.feed(v),
            Action::ApcPutSlice(v) => self.apc_handler.feed_slice(v.bytes),
            Action::ApcEnd(v) => self.apc_end(v.terminated),

            // Unrecognized OSC. The OSC parser already dropped aborted
            // sequences, so everything that reaches here is reported.
            Action::OscUnknown(v) => self.unknown_sequence_(UnknownSequence::Osc(v)),

            // Effect-based handlers
            Action::Bell => self.bell(),
            Action::ShowDesktopNotification(v) => self.desktop_notification(v),
            Action::DeviceAttributes(v) => self.report_device_attributes(v),
            Action::DeviceStatus(v) => self.device_status(v.request),
            Action::Enquiry => self.report_enquiry(),
            Action::KittyKeyboardQuery => self.query_kitty_keyboard(),
            Action::RequestMode(v) => self.request_mode(v.mode),
            Action::RequestModeUnknown(v) => self.request_mode_unknown(v.mode, v.ansi),
            Action::SizeReport(v) => self.report_size(v),
            Action::WindowTitle(v) => self.window_title(v.title)?,
            Action::ReportPwd(v) => self.report_pwd(v.url)?,
            Action::ProgressReport(v) => self.progress_report(v),
            Action::ProgramStatus(v) => self.program_status(v),
            Action::ResizeWindow(v) => self.resize_window(v),
            Action::Xtversion => self.report_xtversion(),
            Action::RequestXtChecksum(v) => self.report_xt_checksum(v),
            Action::ClipboardContents(v) => {
                if let Err(err) = self.clipboard_contents(v.kind, v.data, v.terminator) {
                    // Clipboard operations are external effects, not terminal state.
                    log::warn!(target: LOG_TARGET, "error handling clipboard operation err={}", err);
                }
            }
            Action::KittyDnd(v) => {
                if let Err(err) = self.kitty_dnd(v) {
                    // Drag and drop is a self-contained subsystem: an OOM
                    // updating its state or a failure writing a response
                    // degrades it without corrupting terminal state, so we log.
                    log::warn!(target: LOG_TARGET, "error handling kitty dnd err={}", err);
                }
            }

            Action::DcsHook(v) => self.dcs_hook(v)?,
            Action::DcsPut(v) => self.dcs_put(v)?,
            Action::DcsUnhook => self.dcs_unhook()?,

            // Have no terminal-modifying effect
            Action::TitlePush(_) | Action::TitlePop(_) => {}
        }
        Ok(())
    }

    #[inline]
    fn write_pty(&mut self, data: &[u8]) {
        let Some(func) = self.effects.write_pty else {
            return;
        };
        func(self, data);
    }

    fn dcs_hook(&mut self, value: parser::Dcs<'_>) -> Result<(), VtError> {
        let Some(cmd) = self.dcs_handler.hook(value) else {
            return Ok(());
        };
        self.dcs_command(cmd)
    }

    fn dcs_put(&mut self, value: u8) -> Result<(), VtError> {
        // A tmux notification borrows the DCS handler, so only the
        // commands we act on are taken out of it (tmux is a no-op below).
        let cmd = match self.dcs_handler.put(value) {
            None | Some(dcs::Command::Tmux(_)) => return Ok(()),
            Some(dcs::Command::Decrqss(v)) => dcs::Command::Decrqss(v),
            Some(dcs::Command::Xtgettcap(v)) => dcs::Command::Xtgettcap(v),
        };
        self.dcs_command(cmd)
    }

    fn dcs_unhook(&mut self) -> Result<(), VtError> {
        let Some(cmd) = self.dcs_handler.unhook() else {
            return Ok(());
        };
        self.dcs_command(cmd)
    }

    fn dcs_command(&mut self, cmd: dcs::Command<'_>) -> Result<(), VtError> {
        match cmd {
            dcs::Command::Decrqss(request) => {
                let mut response = [0u8; dcs::Decrqss::MAX_RESPONSE_BYTES];
                let encoded = request.encode(self.terminal, &mut response)?;
                self.write_pty(encoded);
            }

            dcs::Command::Xtgettcap(mut gettcap) => {
                if self.effects.write_pty.is_none() {
                    return Ok(());
                }
                let map = xtgettcap_map();
                while let Some(key) = gettcap.next() {
                    if key == ENCODED_TN_KEY {
                        self.write_terminfo_name();
                        continue;
                    }
                    let Some(value) = map.get(key) else { continue };
                    self.write_pty(value.as_bytes());
                }
            }

            dcs::Command::Tmux(_) => {}
        }
        Ok(())
    }

    /// Answer an XTGETTCAP "TN" query from the configured terminfo name.
    /// Unset, empty, or over-long names leave the query unanswered.
    fn write_terminfo_name(&mut self) {
        let Some(name) = self.terminfo_name else { return };
        if name.is_empty() || name.len() > MAX_TERMINFO_NAME_BYTES {
            return;
        }

        // Fixed upper bound for an encoded "TN" reply calculated
        // from our max terminfo size.
        const MAX_TN_RESPONSE_BYTES: usize =
            b"\x1bP1+r".len() + ENCODED_TN_KEY.len() + b"=".len() + (MAX_TERMINFO_NAME_BYTES * 2) + b"\x1b\\".len();

        // Values are hex-encoded uppercase, matching the static map. The
        // buffer fits any name allowed above, so the print cannot fail.
        let mut buf = [0u8; MAX_TN_RESPONSE_BYTES];
        let n = write_fixed(&mut buf, |w| {
            w.write_all(b"\x1bP1+r")?;
            w.write_all(ENCODED_TN_KEY)?;
            w.write_all(b"=")?;
            for &b in name {
                write!(w, "{:02X}", b)?;
            }
            w.write_all(b"\x1b\\")
        })
        .expect("TN reply fits");
        self.write_pty(&buf[..n]);
    }

    fn bell(&mut self) {
        let Some(func) = self.effects.bell else { return };
        func(self);
    }

    fn desktop_notification(&mut self, notification: stream::ShowDesktopNotification<'_>) {
        let Some(func) = self.effects.desktop_notification else {
            return;
        };
        func(self, notification);
    }

    fn semantic_prompt(&mut self, cmd: osc::SemanticPrompt<'_>) -> Result<(), VtError> {
        self.terminal.semantic_prompt(&cmd)?;
        let Some(func) = self.effects.semantic_prompt else {
            return Ok(());
        };
        match cmd.action {
            osc_sp::Action::FreshLine => {}

            // A and N accept the same k= option as P, and the terminal
            // applies it, so report it for all three.
            osc_sp::Action::FreshLineNewPrompt | osc_sp::Action::NewCommand | osc_sp::Action::PromptStart => {
                let prompt_kind = match cmd.read_option::<osc_sp::options::PromptKind>() {
                    Some(osc_sp::PromptKind::Initial) | None => SemanticPromptPromptKind::Primary,
                    Some(osc_sp::PromptKind::Right) => SemanticPromptPromptKind::Right,
                    Some(osc_sp::PromptKind::Continuation) => SemanticPromptPromptKind::Continuation,
                    Some(osc_sp::PromptKind::Secondary) => SemanticPromptPromptKind::Secondary,
                };
                func(
                    self,
                    SemanticPrompt {
                        prompt_kind,
                        ..SemanticPrompt::new(SemanticPromptKind::PromptStart)
                    },
                );
            }

            osc_sp::Action::EndPromptStartInput | osc_sp::Action::EndPromptStartInputTerminateEol => {
                func(self, SemanticPrompt::new(SemanticPromptKind::InputStart))
            }

            osc_sp::Action::EndInputStartOutput => {
                // Decoding never makes the command line longer, so a
                // buffer the size of the raw options always fits it. If
                // the command line can't be decoded, we still report the
                // step with an empty command.
                let mut buf: Vec<u8> = Vec::with_capacity(cmd.options_unvalidated.len());
                let command: &[u8] = match cmd.write_command_line(&mut buf) {
                    Ok(()) => &buf,
                    Err(_) => b"",
                };
                func(
                    self,
                    SemanticPrompt {
                        command,
                        ..SemanticPrompt::new(SemanticPromptKind::OutputStart)
                    },
                );
            }

            osc_sp::Action::EndCommand => func(
                self,
                SemanticPrompt {
                    exit_code: cmd.read_option::<osc_sp::options::ExitCode>(),
                    err: cmd.read_option::<osc_sp::options::Err>().unwrap_or(b""),
                    ..SemanticPrompt::new(SemanticPromptKind::CommandEnd)
                },
            ),
        }
        Ok(())
    }

    fn progress_report(&mut self, report: osc::ProgressReport) {
        let Some(func) = self.effects.progress_report else {
            return;
        };
        func(self, report);
    }

    fn program_status(&mut self, cmd: osc::ProgramStatus<'_>) {
        match cmd {
            osc::ProgramStatus::Report(report) => self.program_status_report(report),

            // Only claim support when something handles the reports. The
            // reply is always the same fixed bytes: `?` and the lists of
            // states and kinds we accept. The specification never allows
            // sending report contents back to the program.
            osc::ProgramStatus::Query(terminator) => {
                if self.effects.program_status.is_some() {
                    let reply = format!("\x1b]7501;{}{}", program_status::QUERY_REPLY, terminator);
                    self.write_pty(reply.as_bytes());
                }
            }
        }
    }

    fn program_status_report(&mut self, report: program_status::Report<'_>) {
        let Some(func) = self.effects.program_status else {
            return;
        };
        func(self, report);
    }

    fn resize_window(&mut self, size: stream::ResizeWindow) {
        let Some(func) = self.effects.resize_window else {
            return;
        };
        func(self, ResizeWindow::Grid(size));
    }

    fn clipboard_contents(&mut self, kind: u8, data: &[u8], terminator: osc::Terminator) -> Result<(), OutOfMemory> {
        let location = match kind {
            b's' => clipboard::Location::SELECTION,
            b'p' => clipboard::Location::PRIMARY,
            _ => clipboard::Location::STANDARD,
        };

        // OSC 52 uses a "?" payload to request the clipboard contents.
        if data == b"?" {
            self.clipboard_read(location, terminator);
            return Ok(());
        }

        let Some(func) = self.effects.clipboard_write else {
            return Ok(());
        };

        // OSC 52 uses an empty payload to clear the selected clipboard.
        if data.is_empty() {
            func(
                self,
                clipboard::Write {
                    location,
                    contents: &[],
                    name: b"",
                    granted: false,
                    can_remember: false,
                    reply_ctx: std::ptr::null_mut(),
                    reply_fn: ignore_write_reply,
                },
            );
            return Ok(());
        }

        // Decode the base64 payload with the SIMD decoder (the same one
        // used for Kitty clipboard payloads) rather than the scalar std
        // implementation; clipboard payloads can be megabytes. The
        // Kitty clipboard spec governs OSC 52 base64 handling too: a
        // request with characters outside the base64 alphabet is
        // discarded entirely (never partially decoded), while a
        // missing-padding tail is tolerated since OSC 52 has no way
        // to report errors to the client.
        VT_TW.check(FailPoint::ClipboardDecode)?;
        let mut buf = vec![0u8; simd_base64::max_len(data)];
        let Ok(decoded) = simd_base64::decode_strict(data, &mut buf, simd_base64::Padding::Optional) else {
            log::warn!(target: LOG_TARGET, "OSC 52 clipboard write is not valid base64, ignoring");
            return Ok(());
        };

        let contents = [clipboard::Content {
            mime: b"text/plain",
            data: decoded,
        }];
        func(
            self,
            clipboard::Write {
                location,
                contents: &contents,
                name: b"",
                granted: false,
                can_remember: false,
                reply_ctx: std::ptr::null_mut(),
                reply_fn: ignore_write_reply,
            },
        );
        Ok(())
    }

    fn clipboard_read(&mut self, location: clipboard::Location, terminator: osc::Terminator) {
        let Some(func) = self.effects.clipboard_read else {
            return;
        };

        let mut state = ClipboardReadState {
            location,
            terminator,
            response: None,
        };
        func(
            self,
            clipboard::Read {
                location,
                mimes: TEXT_PLAIN_MIMES,
                list: false,
                name: b"",
                granted: false,
                can_remember: false,
                reply_ctx: &mut state as *mut ClipboardReadState as *mut c_void,
                reply_fn: ClipboardReadState::reply,
            },
        );

        // The program is waiting on us, so a callback that returned
        // without a (successful) reply gets an empty clipboard rather
        // than silence.
        let response = match state.response.take() {
            Some(v) => v,
            None => state.respond(b""),
        };
        self.write_pty(&response);
    }

    /// Handle one Kitty clipboard protocol (OSC 5522) packet.
    fn kitty_clipboard(&mut self, v: stream::KittyClipboard<'_>) -> Result<(), OutOfMemory> {
        // Decode and validate the metadata.
        let meta = match kitty_clipboard::Metadata::parse(v.metadata) {
            Ok(Some(meta)) => meta,
            Ok(None) => return Ok(()),
            Err(MetadataError::InvalidValue) => {
                if self.kitty_clipboard_write.is_none() {
                    return Ok(());
                }
                match kitty_clipboard::Metadata::operation(v.metadata) {
                    Some(kitty_clipboard::Operation::Wdata | kitty_clipboard::Operation::Walias) => {
                        let state = self.kitty_clipboard_write.take().unwrap();
                        self.kitty_clipboard_finish(&state, kitty_clipboard::Status::EINVAL, v.terminator);
                    }
                    Some(kitty_clipboard::Operation::Read | kitty_clipboard::Operation::Write) | None => {}
                }
                return Ok(());
            }
        };

        let payload = v.payload.unwrap_or(b"");
        match meta.op {
            kitty_clipboard::Operation::Read => self.kitty_clipboard_read(&meta, payload, v.terminator),
            kitty_clipboard::Operation::Write => self.kitty_clipboard_write_begin(&meta, v.terminator),
            kitty_clipboard::Operation::Wdata => self.kitty_clipboard_data(&meta, payload, v.terminator),
            kitty_clipboard::Operation::Walias => self.kitty_clipboard_alias(&meta, payload, v.terminator),
        }
    }

    fn kitty_clipboard_read(
        &mut self,
        meta: &kitty_clipboard::Metadata,
        payload: &[u8],
        terminator: osc::Terminator,
    ) -> Result<(), OutOfMemory> {
        // The payload is the requested MIME list. Kitty drops a read
        // request with an undecodable payload without any response.
        let Ok(decoded) = kitty_clipboard::Payload::init(payload) else {
            return Ok(());
        };
        if !decoded.is_valid_utf8() {
            return Ok(());
        }

        // Without a clipboard_read effect nothing can serve the read.
        // EPERM is the protocol's denial so clients degrade gracefully.
        let Some(func) = self.effects.clipboard_read else {
            self.kitty_clipboard_respond(&kitty_clipboard::Response {
                id: &meta.id,
                terminator,
                ..kitty_clipboard::Response::new(kitty_clipboard::Operation::Read, kitty_clipboard::Status::EPERM)
            });
            return Ok(());
        };

        // The targets type ('.') asks for the listing of available
        // types rather than data. Requested types beyond the cap are
        // dropped and simply never served, which is how the protocol
        // reports an unavailable type anyway.
        let mut mimes_buf: [&[u8]; kitty_clipboard::MAX_READ_MIMES] = [b""; kitty_clipboard::MAX_READ_MIMES];
        let mut targets = false;
        let mut len: usize = 0;
        for mime in decoded.mime_iterator() {
            if mime == kitty_clipboard::TARGETS_MIME {
                targets = true;
                continue;
            }
            if len == mimes_buf.len() {
                continue;
            }
            mimes_buf[len] = mime;
            len += 1;
        }
        let mimes = &mimes_buf[..len];
        let list = targets;

        // Per the spec a password without a name is no password. A
        // stored grant for it lets the embedder skip its prompt. A
        // prompt-exempt request never consults the grants: the
        // embedder serves it without a prompt anyway, and consuming a
        // one-time paste password on a listing would burn the grant
        // before the follow-up data read.
        let pw: &[u8] = if !meta.name.is_empty() { &meta.pw } else { b"" };
        let granted = !kitty_clipboard::read_prompt_exempt(mimes.len())
            && self.kitty_clipboard_grants.r#use(pw, GrantDirection::Read);

        let mut state = KittyClipboardReadState {
            primary: meta.loc == clipboard::Location::PRIMARY,
            id: meta.id.clone(),
            pw: pw.to_vec(),
            mimes: mimes.iter().map(|m| m.to_vec()).collect(),
            list,
            terminator,
            write_pty: self.effects.write_pty.is_some(),
            replied: false,
            remember: false,
            response: Vec::new(),
        };
        func(
            self,
            clipboard::Read {
                location: meta.loc,
                mimes,
                list,
                name: &meta.name,
                granted,
                can_remember: !pw.is_empty(),
                reply_ctx: &mut state as *mut KittyClipboardReadState as *mut c_void,
                reply_fn: KittyClipboardReadState::reply,
            },
        );

        // The program is waiting on us, so a callback that returned
        // without a reply is answered as a denial rather than silence.
        if !state.replied {
            state.respond_status(kitty_clipboard::Status::EPERM);
        }

        // Apply the reply. Remembering is only offered when the
        // request carried a usable password.
        if state.remember && !state.pw.is_empty() {
            self.kitty_clipboard_grants.grant(&state.pw, GrantDirection::Read, false);
        }
        if !state.response.is_empty() {
            self.write_pty(&state.response);
        }
        Ok(())
    }

    fn kitty_clipboard_write_begin(
        &mut self,
        meta: &kitty_clipboard::Metadata,
        terminator: osc::Terminator,
    ) -> Result<(), OutOfMemory> {
        // A new write silently replaces any in-flight transaction.
        self.kitty_clipboard_abort();

        // Without a clipboard_write effect a commit can never succeed,
        // so fail the transaction up front instead of spooling data
        // we'd only throw away. Later wdata packets are ignored.
        if self.effects.clipboard_write.is_none() {
            self.kitty_clipboard_respond(&kitty_clipboard::Response {
                id: &meta.id,
                terminator,
                ..kitty_clipboard::Response::new(kitty_clipboard::Operation::Write, kitty_clipboard::Status::ENOSYS)
            });
            return Ok(());
        }

        // Setup our write state
        VT_TW.check(FailPoint::KittyClipboardWriteBegin)?;
        let state = Box::new(kitty_clipboard::WriteState::init(
            meta,
            WriteStateOptions {
                max_size: self.kitty_clipboard_write_max_bytes,
            },
        ));
        self.kitty_clipboard_write = Some(state);
        Ok(())
    }

    fn kitty_clipboard_data(
        &mut self,
        meta: &kitty_clipboard::Metadata,
        payload: &[u8],
        terminator: osc::Terminator,
    ) -> Result<(), OutOfMemory> {
        // Data without a transaction is silently ignored.
        let Some(mut state) = self.kitty_clipboard_write.take() else {
            return Ok(());
        };

        // A wdata packet without a MIME type commits the transaction.
        if meta.mime.is_empty() {
            self.kitty_clipboard_commit(state, terminator);
            return Ok(());
        }

        match state.data(meta, payload) {
            Ok(()) => self.kitty_clipboard_write = Some(state),

            // Data over the write limit aborts the transaction and is
            // reported to the client.
            Err(DataError::TooLarge) => {
                self.kitty_clipboard_finish(&state, kitty_clipboard::Status::EFBIG, terminator)
            }

            // An invalid base64 payload stream aborts the transaction.
            Err(DataError::Invalid) => {
                self.kitty_clipboard_finish(&state, kitty_clipboard::Status::EINVAL, terminator)
            }
        }
        Ok(())
    }

    fn kitty_clipboard_alias(
        &mut self,
        meta: &kitty_clipboard::Metadata,
        payload: &[u8],
        terminator: osc::Terminator,
    ) -> Result<(), OutOfMemory> {
        // Aliases without a transaction are silently ignored. Once a
        // transaction exists, a missing target MIME type is invalid and
        // aborts the transaction.
        let Some(mut state) = self.kitty_clipboard_write.take() else {
            return Ok(());
        };
        if meta.mime.is_empty() {
            self.kitty_clipboard_finish(&state, kitty_clipboard::Status::EINVAL, terminator);
            return Ok(());
        }

        match state.alias(meta, payload) {
            Ok(()) => self.kitty_clipboard_write = Some(state),

            // An undecodable alias payload aborts the transaction.
            Err(_) => self.kitty_clipboard_finish(&state, kitty_clipboard::Status::EINVAL, terminator),
        }
        Ok(())
    }

    fn kitty_clipboard_commit(&mut self, mut state: Box<kitty_clipboard::WriteState>, terminator: osc::Terminator) {
        // The id echoed is the one from the transaction's opening write
        // packet, matching kitty.
        let id = state.id.clone();
        let respond = |handler: &mut Handler<'_>, status: kitty_clipboard::Status| {
            handler.kitty_clipboard_respond(&kitty_clipboard::Response {
                id: &id,
                terminator,
                ..kitty_clipboard::Response::new(kitty_clipboard::Operation::Write, status)
            });
        };

        let committed = match state.commit() {
            Ok(committed) => committed,

            // The last MIME type's payload stream was not correctly
            // padded, which aborts the transaction.
            Err(_) => {
                respond(self, kitty_clipboard::Status::EINVAL);
                return;
            }
        };

        // The effect can't be None here (checked when the transaction
        // began) but if an embedder cleared it mid-transaction that's
        // ENOSYS.
        let Some(func) = self.effects.clipboard_write else {
            respond(self, kitty_clipboard::Status::ENOSYS);
            return;
        };

        // Per the spec a password without a name is no password. A
        // stored grant for it lets the embedder skip its prompt.
        let pw: &[u8] = if !committed.name.is_empty() { committed.pw } else { b"" };
        let granted = self.kitty_clipboard_grants.r#use(pw, GrantDirection::Write);

        let mut reply_state = KittyClipboardWriteReplyState {
            status: None,
            remember: false,
        };
        func(
            self,
            clipboard::Write {
                location: committed.loc,
                contents: &committed.contents,
                name: committed.name,
                granted,
                can_remember: !pw.is_empty(),
                reply_ctx: &mut reply_state as *mut KittyClipboardWriteReplyState as *mut c_void,
                reply_fn: KittyClipboardWriteReplyState::reply,
            },
        );

        // Remembering is only offered when the request carried a
        // usable password.
        if reply_state.remember && !pw.is_empty() {
            self.kitty_clipboard_grants.grant(pw, GrantDirection::Write, false);
        }

        // The program is waiting on the commit status, so a callback
        // that returned without a reply is answered as a denial rather
        // than silence.
        respond(self, reply_state.status.unwrap_or(kitty_clipboard::Status::EPERM));
    }

    /// Answer a write transaction with its final status and drop it.
    /// The id echoed is the one from the transaction's opening write
    /// packet, matching kitty.
    fn kitty_clipboard_finish(
        &mut self,
        state: &kitty_clipboard::WriteState,
        status: kitty_clipboard::Status,
        terminator: osc::Terminator,
    ) {
        self.kitty_clipboard_respond(&kitty_clipboard::Response {
            id: &state.id,
            terminator,
            ..kitty_clipboard::Response::new(kitty_clipboard::Operation::Write, status)
        });
        self.kitty_clipboard_abort();
    }

    /// Drop any in-flight write transaction without responding.
    fn kitty_clipboard_abort(&mut self) {
        self.kitty_clipboard_write = None;
    }

    /// Encode and write a single response packet. Unlike kitty, which
    /// always terminates responses with ST, we echo the terminator of
    /// the request being answered, matching our other OSC responses.
    fn kitty_clipboard_respond(&mut self, response: &kitty_clipboard::Response<'_>) {
        if self.effects.write_pty.is_none() {
            return;
        }

        let mut buf: Vec<u8> = Vec::with_capacity(128);
        if response.encode(&mut buf).is_err() {
            return;
        }
        self.write_pty(&buf);
    }

    /// Handle an OSC 72 drag and drop command.
    fn kitty_dnd(&mut self, v: stream::KittyDnd<'_>) -> std::io::Result<()> {
        // Responses are usually small (queries, errors) but data
        // serving can produce many chunks.
        let mut aw: Vec<u8> = Vec::new();

        // The state is allocated on registration and owned by the
        // terminal.
        let event = kitty_dnd::handle_command(&mut self.terminal.kitty_dnd, &mut aw, v)?;

        if !aw.is_empty() {
            self.write_pty(&aw);
        }

        if let Some(ev) = event {
            let Some(func) = self.effects.drag_and_drop else {
                return Ok(());
            };
            func(self, ev);
        }
        Ok(())
    }

    fn report_device_attributes(&mut self, req: device_attributes::Req) {
        let Some(func) = self.effects.device_attributes else {
            return;
        };
        let attrs = func(self);

        let mut aw: Vec<u8> = Vec::with_capacity(128);
        if attrs.encode(req, &mut aw).is_err() {
            return;
        }
        self.write_pty(&aw);
    }

    /// The cursor position for a CPR/DECXCPR report, honoring origin mode
    /// (relative to the scrolling region when DECOM is set).
    fn cursor_report_pos(&self) -> (usize, usize) {
        let cursor = &self.terminal.screens.active.cursor;
        if self.terminal.modes.get(Mode::Origin) {
            (
                (cursor.x as usize).saturating_sub(self.terminal.scrolling_region.left as usize),
                (cursor.y as usize).saturating_sub(self.terminal.scrolling_region.top as usize),
            )
        } else {
            (cursor.x as usize, cursor.y as usize)
        }
    }

    fn device_status(&mut self, req: device_status::Request) {
        match req {
            device_status::Request::OperatingStatus => self.write_pty(b"\x1B[0n"),

            device_status::Request::CursorPosition => {
                let (x, y) = self.cursor_report_pos();
                let resp = format!("\x1B[{};{}R", y + 1, x + 1);
                self.write_pty(resp.as_bytes());
            }

            // DECXCPR: extended cursor position adds a page number. Ghostty has
            // no page memory, so the page is always 1.
            device_status::Request::CursorPositionExtended => {
                let (x, y) = self.cursor_report_pos();
                let resp = format!("\x1B[?{};{};1R", y + 1, x + 1);
                self.write_pty(resp.as_bytes());
            }

            device_status::Request::ColorScheme => {
                let Some(func) = self.effects.color_scheme else {
                    return;
                };
                let Some(scheme) = func(self) else { return };
                let mut buf = [0u8; device_status::MAX_COLOR_SCHEME_REPORT_ENCODE_SIZE];
                let Ok(n) = write_fixed(&mut buf, |w| device_status::encode_color_scheme_report(w, scheme)) else {
                    return;
                };
                self.write_pty(&buf[..n]);
            }

            device_status::Request::Visibility => self.send_visibility_report(),
        }
    }

    fn send_visibility_report(&mut self) {
        let Some(write_pty) = self.effects.write_pty else {
            return;
        };

        let visibility = if self.terminal.flags.visible {
            device_status::Visibility::PotentiallyVisible
        } else {
            device_status::Visibility::NotVisible
        };
        let mut buf = [0u8; device_status::MAX_VISIBILITY_REPORT_ENCODE_SIZE];
        let Ok(n) = write_fixed(&mut buf, |w| device_status::encode_visibility_report(w, visibility)) else {
            return;
        };
        write_pty(self, &buf[..n]);
    }

    fn report_enquiry(&mut self) {
        let Some(func) = self.effects.enquiry else {
            return;
        };
        let mut buf = [0u8; 256];
        let len = {
            let response = func(self);
            if response.is_empty() || response.len() >= buf.len() {
                return;
            }
            buf[..response.len()].copy_from_slice(response);
            response.len()
        };
        self.write_pty(&buf[..len]);
    }

    fn report_xtversion(&mut self) {
        let mut buf = [0u8; 288];
        let n = {
            let version: &[u8] = match self.effects.xtversion {
                Some(func) => func(self),
                None => b"",
            };
            let version: &[u8] = if !version.is_empty() { version } else { b"libghostty" };
            let Ok(n) = write_fixed(&mut buf, |w| {
                w.write_all(b"\x1BP>|")?;
                w.write_all(version)?;
                w.write_all(b"\x1B\\")
            }) else {
                return;
            };
            n
        };
        self.write_pty(&buf[..n]);
    }

    fn report_xt_checksum(&mut self, req: xt_checksum::Request) {
        if !self.xt_checksum_report {
            return;
        }
        let mut buf = [0u8; xt_checksum::MAX_ENCODE_SIZE];
        let sum = self.terminal.rect_xt_checksum(req);
        let n = write_fixed(&mut buf, |w| xt_checksum::encode(w, req.id, sum)).expect("checksum reply fits");
        self.write_pty(&buf[..n]);
    }

    fn report_size(&mut self, style: csi::SizeReportStyle) {
        // Accumulate the response.
        let mut aw: Vec<u8> = Vec::with_capacity(256);

        // Build the response.
        match style {
            csi::SizeReportStyle::Csi21T => {
                if !self.title_report {
                    return;
                }
                let title = self.terminal.get_title().unwrap_or(b"");
                aw.extend_from_slice(b"\x1b]l");
                aw.extend_from_slice(title);
                aw.extend_from_slice(b"\x1b\\");
            }

            csi::SizeReportStyle::Csi14T
            | csi::SizeReportStyle::Csi16T
            | csi::SizeReportStyle::Csi18T
            | csi::SizeReportStyle::Csi19T => {
                let Some(get_size) = self.effects.size else {
                    return;
                };
                let Some(s) = get_size(self) else { return };
                let report_style = match style {
                    csi::SizeReportStyle::Csi14T => size_report::Style::Csi14T,
                    csi::SizeReportStyle::Csi16T => size_report::Style::Csi16T,
                    csi::SizeReportStyle::Csi18T => size_report::Style::Csi18T,
                    csi::SizeReportStyle::Csi19T => size_report::Style::Csi19T,
                    csi::SizeReportStyle::Csi21T => unreachable!(),
                };
                if let Err(err) = size_report::encode(&mut aw, report_style, s) {
                    log::warn!(target: LOG_TARGET, "error encoding size report err={}", err);
                    return;
                }
            }
        }

        self.write_pty(&aw);
    }

    fn report_mode_2048(&mut self) {
        let Some(get_size) = self.effects.size else {
            return;
        };
        let Some(current) = get_size(self) else { return };

        let mut buf = [0u8; 128];
        let Ok(n) = write_fixed(&mut buf[..127], |w| {
            size_report::encode(w, size_report::Style::Mode2048, current)
        }) else {
            return;
        };
        self.write_pty(&buf[..n]);
    }

    fn window_title(&mut self, title_raw: &[u8]) -> Result<(), VtError> {
        // Prevent DoS attacks by limiting title length.
        const MAX_TITLE_LEN: usize = 1024;
        let title = if title_raw.len() > MAX_TITLE_LEN {
            log::warn!(
                target: LOG_TARGET,
                "title length {} exceeds max length {}, truncating",
                title_raw.len(),
                MAX_TITLE_LEN
            );
            &title_raw[..MAX_TITLE_LEN]
        } else {
            title_raw
        };

        VT_TW.check(FailPoint::WindowTitle)?;
        self.terminal.set_title(title)?;

        let Some(func) = self.effects.title_changed else {
            return Ok(());
        };
        func(self);
        Ok(())
    }

    fn report_pwd(&mut self, url_raw: &[u8]) -> Result<(), VtError> {
        // Prevent DoS attacks by limiting url length. Headroom for
        // Linux PATH_MAX (4096) plus URI scheme/host and percent-encoding.
        const MAX_URL_LEN: usize = 4096;
        let url = if url_raw.len() > MAX_URL_LEN {
            log::warn!(
                target: LOG_TARGET,
                "pwd url length {} exceeds max length {}, truncating",
                url_raw.len(),
                MAX_URL_LEN
            );
            &url_raw[..MAX_URL_LEN]
        } else {
            url_raw
        };

        // We store the raw payload unparsed. Embedders read it via
        // get_pwd() and are responsible for decoding any URI scheme.
        self.terminal.set_pwd(url)?;

        let Some(func) = self.effects.pwd_changed else {
            return Ok(());
        };
        func(self);
        Ok(())
    }

    fn request_mode(&mut self, mode: Mode) {
        let mut report = self.terminal.modes.get_report(modes::ReportTag::from_mode(mode));

        // Kitty paste events (mode 5522) can't work without a clipboard
        // read effect, so if that isn't set mark it as unrecognized.
        if mode == Mode::KittyPasteEvents && self.effects.clipboard_read.is_none() {
            report.state = modes::ReportState::NotRecognized;
        }

        self.send_mode_report(report);
    }

    fn request_mode_unknown(&mut self, mode_raw: u16, ansi: bool) {
        let report = self.terminal.modes.get_report(modes::ReportTag {
            value: mode_raw,
            ansi,
        });
        self.send_mode_report(report);
    }

    fn send_mode_report(&mut self, report: modes::Report) {
        let mut buf = [0u8; modes::Report::MAX_SIZE];
        let n = match write_fixed(&mut buf, |w| report.encode(w)) {
            Ok(n) => n,
            Err(err) => {
                log::warn!(target: LOG_TARGET, "error encoding mode report err={}", err);
                return;
            }
        };
        self.write_pty(&buf[..n]);
    }

    fn query_kitty_keyboard(&mut self) {
        // Max response is "\x1b[?31u" (6 bytes): the flags are a u5 (max 31).
        let resp = format!(
            "\x1b[?{}u",
            self.terminal.screens.active.kitty_keyboard.current().int()
        );
        self.write_pty(resp.as_bytes());
    }

    #[inline]
    fn horizontal_tab(&mut self, count: u16) {
        for _ in 0..count {
            let x = self.terminal.screens.active.cursor.x;
            self.terminal.horizontal_tab();
            if x == self.terminal.screens.active.cursor.x {
                break;
            }
        }
    }

    #[inline]
    fn horizontal_tab_back(&mut self, count: u16) {
        for _ in 0..count {
            let x = self.terminal.screens.active.cursor.x;
            self.terminal.horizontal_tab_back();
            if x == self.terminal.screens.active.cursor.x {
                break;
            }
        }
    }

    /// Report that a render hold began or ended. See `Effects::render_hold`.
    #[inline]
    fn render_hold(&mut self, held: bool) {
        let Some(func) = self.effects.render_hold else {
            return;
        };
        func(self, held);
    }

    fn set_mode(&mut self, mode: Mode, enabled: bool) -> Result<(), VtError> {
        // Synchronized output is reported as a render hold. We only report
        // real changes. Reporting a set during a hold would be harmful
        // because the screen is half-drawn at that point and the callback
        // is expected to capture it.
        if mode == Mode::SynchronizedOutput {
            if self.terminal.modes.get(mode) == enabled {
                return Ok(());
            }
            self.terminal.modes.set(mode, enabled);
            self.render_hold(enabled);
            return Ok(());
        }

        // Set the mode on the terminal
        self.terminal.modes.set(mode, enabled);

        // Some modes require additional processing
        match mode {
            Mode::Autorepeat | Mode::ReverseColors => {}

            Mode::Origin => self.terminal.set_cursor_pos(1, 1),

            Mode::EnableLeftAndRightMargin => {
                if !enabled {
                    self.terminal.scrolling_region.left = 0;
                    self.terminal.scrolling_region.right = self.terminal.cols - 1;
                }
            }

            Mode::AltScreenLegacy => self.terminal.switch_screen_mode(SwitchScreenMode::Mode47, enabled)?,
            Mode::AltScreen => self.terminal.switch_screen_mode(SwitchScreenMode::Mode1047, enabled)?,
            Mode::AltScreenSaveCursorClearEnter => {
                self.terminal.switch_screen_mode(SwitchScreenMode::Mode1049, enabled)?
            }

            Mode::SaveCursor => {
                if enabled {
                    self.terminal.save_cursor();
                } else {
                    self.terminal.restore_cursor();
                }
            }

            Mode::EnableMode3 => {}

            Mode::Column132 => self.terminal.deccolm(if enabled {
                DeccolmMode::Cols132
            } else {
                DeccolmMode::Cols80
            })?,

            // Handled above
            Mode::SynchronizedOutput => unreachable!(),

            Mode::Linefeed | Mode::FocusEvent => {}

            // Enabling mode 2048 reports already-committed pixel geometry.
            // Waiting for the next resize leaves late-enabling clients without
            // the dimensions they need for their first image frame.
            Mode::InBandSizeReports => {
                if enabled {
                    self.report_mode_2048();
                }
            }

            Mode::ReportVisibility => {
                if enabled {
                    self.send_visibility_report();
                }
            }

            Mode::MouseEventX10 => {
                self.terminal.flags.mouse_event = if enabled { mouse::Event::X10 } else { mouse::Event::None };
            }
            Mode::MouseEventNormal => {
                self.terminal.flags.mouse_event = if enabled { mouse::Event::Normal } else { mouse::Event::None };
            }
            Mode::MouseEventButton => {
                self.terminal.flags.mouse_event = if enabled { mouse::Event::Button } else { mouse::Event::None };
            }
            Mode::MouseEventAny => {
                self.terminal.flags.mouse_event = if enabled { mouse::Event::Any } else { mouse::Event::None };
            }

            Mode::MouseFormatUtf8 => {
                self.terminal.flags.mouse_format = if enabled { mouse::Format::Utf8 } else { mouse::Format::X10 };
            }
            Mode::MouseFormatSgr => {
                self.terminal.flags.mouse_format = if enabled { mouse::Format::Sgr } else { mouse::Format::X10 };
            }
            Mode::MouseFormatUrxvt => {
                self.terminal.flags.mouse_format = if enabled { mouse::Format::Urxvt } else { mouse::Format::X10 };
            }
            Mode::MouseFormatSgrPixels => {
                self.terminal.flags.mouse_format = if enabled {
                    mouse::Format::SgrPixels
                } else {
                    mouse::Format::X10
                };
            }

            _ => {}
        }
        Ok(())
    }

    fn color_operation(
        &mut self,
        requests: &osc::color::List,
        terminator: osc::Terminator,
    ) -> std::io::Result<()> {
        if requests.is_empty() {
            return Ok(());
        }

        let mut response: Vec<u8> = Vec::with_capacity(64);

        for req in requests {
            match *req {
                osc::color::Request::Set(set) => match set.target {
                    osc::color::Target::Palette(i) => {
                        self.terminal.flags.dirty.palette = true;
                        self.terminal.colors.palette.set(i, set.color);
                    }
                    osc::color::Target::Dynamic(dynamic) => match dynamic {
                        color::Dynamic::Foreground => self.terminal.colors.foreground.set(set.color),
                        color::Dynamic::Background => self.terminal.colors.background.set(set.color),
                        color::Dynamic::Cursor => self.terminal.colors.cursor.set(set.color),
                        color::Dynamic::PointerForeground
                        | color::Dynamic::PointerBackground
                        | color::Dynamic::TektronixForeground
                        | color::Dynamic::TektronixBackground
                        | color::Dynamic::HighlightBackground
                        | color::Dynamic::TektronixCursor
                        | color::Dynamic::HighlightForeground => {}
                    },
                    osc::color::Target::Special(_) => {}
                },

                osc::color::Request::Reset(target) => match target {
                    osc::color::Target::Palette(i) => {
                        self.terminal.flags.dirty.palette = true;
                        self.terminal.colors.palette.reset(i);
                    }
                    osc::color::Target::Dynamic(dynamic) => match dynamic {
                        color::Dynamic::Foreground => self.terminal.colors.foreground.reset(),
                        color::Dynamic::Background => self.terminal.colors.background.reset(),
                        color::Dynamic::Cursor => self.terminal.colors.cursor.reset(),
                        color::Dynamic::PointerForeground
                        | color::Dynamic::PointerBackground
                        | color::Dynamic::TektronixForeground
                        | color::Dynamic::TektronixBackground
                        | color::Dynamic::HighlightBackground
                        | color::Dynamic::TektronixCursor
                        | color::Dynamic::HighlightForeground => {}
                    },
                    osc::color::Target::Special(_) => {}
                },

                osc::color::Request::ResetPalette => {
                    let mask = self.terminal.colors.palette.mask;
                    for i in mask.iter() {
                        self.terminal.flags.dirty.palette = true;
                        self.terminal.colors.palette.reset(i as u8);
                    }
                    self.terminal.colors.palette.mask = color::PaletteMask::EMPTY;
                }

                osc::color::Request::Query(target) => {
                    if self.effects.write_pty.is_none() {
                        continue;
                    }
                    let Some(c) = self.terminal.color_for_xterm(target) else {
                        continue;
                    };
                    write_xterm_color_report(&mut response, target, c, terminator)?;
                }

                osc::color::Request::ResetSpecial => {}
            }
        }

        if !response.is_empty() {
            self.write_pty(&response);
        }
        Ok(())
    }

    fn kitty_color_operation(&mut self, request: &kitty_color::Osc) -> std::io::Result<()> {
        let mut response: Vec<u8> = Vec::with_capacity(64);

        for item in &request.list {
            match *item {
                kitty_color::Request::Set { key, color } => match key {
                    kitty_color::Kind::Palette(palette) => {
                        self.terminal.flags.dirty.palette = true;
                        self.terminal.colors.palette.set(palette, color);
                    }
                    kitty_color::Kind::Special(special) => match special {
                        kitty_color::Special::Foreground => self.terminal.colors.foreground.set(color),
                        kitty_color::Special::Background => self.terminal.colors.background.set(color),
                        kitty_color::Special::Cursor => self.terminal.colors.cursor.set(color),
                        _ => {}
                    },
                },
                kitty_color::Request::Reset(key) => match key {
                    kitty_color::Kind::Palette(palette) => {
                        self.terminal.flags.dirty.palette = true;
                        self.terminal.colors.palette.reset(palette);
                    }
                    kitty_color::Kind::Special(special) => match special {
                        kitty_color::Special::Foreground => self.terminal.colors.foreground.reset(),
                        kitty_color::Special::Background => self.terminal.colors.background.reset(),
                        kitty_color::Special::Cursor => self.terminal.colors.cursor.reset(),
                        _ => {}
                    },
                },
                kitty_color::Request::Query(key) => {
                    if self.effects.write_pty.is_none() {
                        continue;
                    }
                    let Some(c) = self.terminal.color_for_kitty(key) else {
                        if !key.has_terminal_query_color() {
                            continue;
                        }
                        if response.is_empty() {
                            response.extend_from_slice(b"\x1b]21");
                        }
                        write!(response, ";{}=", key)?;
                        continue;
                    };

                    if response.is_empty() {
                        response.extend_from_slice(b"\x1b]21");
                    }
                    write!(response, ";{}=", key)?;
                    c.encode_rgb8(&mut response)?;
                }
            }
        }

        if !response.is_empty() {
            write!(response, "{}", request.terminator)?;
            self.write_pty(&response);
        }
        Ok(())
    }

    /// Finish the current APC and run its command. `terminated` is false
    /// when the APC was cancelled or aborted, and then nothing runs.
    fn apc_end(&mut self, terminated: bool) {
        let Some(result) = self.apc_handler.end() else {
            return;
        };

        // ECMA-48 says a cancelled or aborted string is thrown away, so
        // its command must not run. We still call end() above so the APC
        // handler resets and frees what it collected.
        if !terminated {
            return;
        }

        match result {
            apc::Command::Unknown(unknown) => {
                self.unknown_sequence_(UnknownSequence::Apc(UnknownSequenceString {
                    content: &unknown.content,
                    truncated: unknown.truncated,
                }));
            }

            apc::Command::Kitty(kitty_cmd) => {
                if build_options::KITTY_GRAPHICS {
                    self.apc_kitty_graphics(&kitty_cmd);
                }
            }

            apc::Command::Glyph(glyph_req) => {
                if build_options::GLYPH_PROTOCOL {
                    let resp = self.terminal.glyph_protocol(&glyph_req);
                    let Some(r) = resp else { return };

                    // Don't waste time encoding if we can't write responses
                    // anyways.
                    if self.effects.write_pty.is_none() {
                        return;
                    }

                    // Glyph responses are short and bounded by the protocol
                    // fields we emit. The final byte is reserved for the
                    // callback's sentinel, as in Zig.
                    let mut buf = [0u8; apc::glyph::Response::MAX_WIRE_BYTES];
                    let Ok(n) = write_fixed(&mut buf[..apc::glyph::Response::MAX_WIRE_BYTES - 1], |w| r.format_wire(w))
                    else {
                        return;
                    };
                    self.write_pty(&buf[..n]);
                }
            }
        }
    }

    /// Execute a Kitty graphics command and write its response.
    fn apc_kitty_graphics(&mut self, kitty_cmd: &crate::terminal::kitty::graphics::Command) {
        let Some(resp) = self.terminal.kitty_graphics(kitty_cmd) else {
            return;
        };

        // Don't waste time encoding if we can't write responses
        // anyways.
        if self.effects.write_pty.is_none() {
            return;
        }

        // Encode and write the response if we have one. The final byte
        // is reserved for the callback's sentinel, as in Zig.
        let mut buf = [0u8; 1024];
        let Ok(n) = write_fixed(&mut buf[..1023], |w| resp.encode(w)) else {
            return;
        };
        if n > 2 {
            self.write_pty(&buf[..n]);
        }
    }
}

impl stream::Handler for Handler<'_> {
    #[inline]
    fn vt(&mut self, action: Action<'_>) {
        let key = action.key();
        if let Err(err) = self.vt_fallible(action) {
            self.semantic_failure = true;
            log::warn!(
                target: LOG_TARGET,
                "error handling VT action action={} err={}",
                key.as_str(),
                err
            );
        }
    }

    fn deinit(&mut self) {
        Handler::deinit(self);
    }
}

fn write_xterm_color_report(
    writer: &mut Vec<u8>,
    target: osc::color::Target,
    c: color::Rgb,
    terminator: osc::Terminator,
) -> std::io::Result<()> {
    match target {
        osc::color::Target::Palette(i) => {
            write!(writer, "\x1b]4;{};", i)?;
            c.encode_rgb16(writer)?;
            write!(writer, "{}", terminator)?;
        }
        osc::color::Target::Dynamic(dynamic) => match dynamic {
            color::Dynamic::Foreground | color::Dynamic::Background | color::Dynamic::Cursor => {
                write!(writer, "\x1b]{};", dynamic as u8)?;
                c.encode_rgb16(writer)?;
                write!(writer, "{}", terminator)?;
            }
            color::Dynamic::PointerForeground
            | color::Dynamic::PointerBackground
            | color::Dynamic::TektronixForeground
            | color::Dynamic::TektronixBackground
            | color::Dynamic::HighlightBackground
            | color::Dynamic::TektronixCursor
            | color::Dynamic::HighlightForeground => {}
        },
        osc::color::Target::Special(_) => {}
    }
    Ok(())
}

/// Reply target for clipboard writes on protocols without a write
/// acknowledgement (OSC 52, OSC 1337 Copy): the reply is accepted
/// and discarded.
fn ignore_write_reply(_: *mut c_void, _: clipboard::WriteResult) {}

/// Reply state for one synchronous clipboard read. This lives on the
/// clipboard_read stack frame, so it is only valid during the callback.
struct ClipboardReadState {
    location: clipboard::Location,
    terminator: osc::Terminator,

    /// The encoded reply, set by the first reply.
    response: Option<Vec<u8>>,
}

impl ClipboardReadState {
    fn reply(ctx: *mut c_void, result: clipboard::ReadResult<'_>) {
        // SAFETY: `clipboard_read` passes a pointer to its live state, and
        // the reply is only valid during the callback.
        let state = unsafe { &mut *(ctx as *mut ClipboardReadState) };
        if state.response.is_some() {
            log::warn!(target: LOG_TARGET, "clipboard read replied more than once, ignoring");
            return;
        }

        // OSC 52 carries a single text value.
        let data: &[u8] = match result {
            clipboard::ReadResult::Denied
            | clipboard::ReadResult::Unsupported
            | clipboard::ReadResult::Busy
            | clipboard::ReadResult::IoError => b"",
            clipboard::ReadResult::Success(s) => s
                .contents
                .iter()
                .find(|c| clipboard::is_text_mime(c.mime))
                .map(|c| c.data)
                .unwrap_or(b""),
        };

        state.response = Some(state.respond(data));
    }

    fn respond(&self, data: &[u8]) -> Vec<u8> {
        let kind = match self.location {
            clipboard::Location::SELECTION => 's',
            clipboard::Location::PRIMARY => 'p',
            _ => 'c',
        };
        let mut aw: Vec<u8> = Vec::with_capacity(16 + data.len() * 4 / 3);
        let _ = write!(aw, "\x1b]52;{};", kind);
        aw.extend_from_slice(base64::engine::general_purpose::STANDARD.encode(data).as_bytes());
        let _ = write!(aw, "{}", self.terminator);
        aw
    }
}

/// Reply state for one synchronous Kitty clipboard read. This lives
/// on the kitty_clipboard_read stack frame, so it is only valid during
/// the callback.
struct KittyClipboardReadState {
    primary: bool,
    id: Vec<u8>,

    /// The effective password, empty when the request had none.
    pw: Vec<u8>,

    /// The requested types; only these are served from a reply.
    mimes: Vec<Vec<u8>>,
    list: bool,
    terminator: osc::Terminator,

    /// Whether the handler has a write_pty effect (responses are only
    /// encoded when they can be written).
    write_pty: bool,

    replied: bool,

    /// The reply asked to remember the decision.
    remember: bool,

    /// The encoded response packets.
    response: Vec<u8>,
}

impl KittyClipboardReadState {
    fn reply(ctx: *mut c_void, result: clipboard::ReadResult<'_>) {
        // SAFETY: `kitty_clipboard_read` passes a pointer to its live
        // state, and the reply is only valid during the callback.
        let state = unsafe { &mut *(ctx as *mut KittyClipboardReadState) };
        if state.replied {
            log::warn!(target: LOG_TARGET, "clipboard read replied more than once, ignoring");
            return;
        }
        state.replied = true;

        let success = match result {
            clipboard::ReadResult::Denied => return state.respond_status(kitty_clipboard::Status::EPERM),
            clipboard::ReadResult::Unsupported => return state.respond_status(kitty_clipboard::Status::ENOSYS),
            clipboard::ReadResult::Busy => return state.respond_status(kitty_clipboard::Status::EBUSY),
            clipboard::ReadResult::IoError => return state.respond_status(kitty_clipboard::Status::EIO),
            clipboard::ReadResult::Success(s) => s,
        };

        // Remembering is only offered when the request carried a
        // usable password (checked when the reply is applied).
        if success.remember {
            state.remember = true;
        }

        if let Err(err) = state.respond_success(&success) {
            log::warn!(target: LOG_TARGET, "error replying to clipboard read err={}", err);
            state.respond_status(kitty_clipboard::Status::EIO);
        }
    }

    /// Answer with a single status packet.
    fn respond_status(&mut self, status: kitty_clipboard::Status) {
        if !self.write_pty {
            return;
        }
        let _ = kitty_clipboard::Response {
            id: &self.id,
            terminator: self.terminator,
            ..kitty_clipboard::Response::new(kitty_clipboard::Operation::Read, status)
        }
        .encode(&mut self.response);
    }

    /// Answer with the full success sequence (OK, listing, DATA
    /// chunks, DONE), serving only the requested representations
    /// in request order.
    fn respond_success(&mut self, success: &clipboard::ReadSuccess<'_>) -> std::io::Result<()> {
        if !self.write_pty {
            return Ok(());
        }

        let mut served: Vec<clipboard::Content<'_>> = Vec::with_capacity(self.mimes.len());
        for mime in &self.mimes {
            if let Some(content) = success.contents.iter().find(|c| c.mime == &mime[..]) {
                served.push(*content);
            }
        }

        let mut out: Vec<u8> = Vec::with_capacity(1024);
        kitty_clipboard::ReadSuccess {
            primary: self.primary,
            id: &self.id,
            pw: None,
            list: self.list,
            available: success.available,
            contents: &served,
            terminator: self.terminator,
        }
        .encode(&mut out)?;
        self.response.extend_from_slice(&out);
        Ok(())
    }
}

/// Reply state for one synchronous Kitty clipboard write. This lives
/// on the kitty_clipboard_commit stack frame, so it is only valid
/// during the callback.
struct KittyClipboardWriteReplyState {
    /// The replied commit status, mapped 1:1 from the reply result;
    /// None until the callback replies.
    status: Option<kitty_clipboard::Status>,

    /// The reply asked to remember the decision.
    remember: bool,
}

impl KittyClipboardWriteReplyState {
    fn reply(ctx: *mut c_void, result: clipboard::WriteResult) {
        // SAFETY: `kitty_clipboard_commit` passes a pointer to its live
        // state, and the reply is only valid during the callback.
        let state = unsafe { &mut *(ctx as *mut KittyClipboardWriteReplyState) };
        if state.status.is_some() {
            log::warn!(target: LOG_TARGET, "clipboard write replied more than once, ignoring");
            return;
        }
        state.status = Some(match result {
            clipboard::WriteResult::Denied => kitty_clipboard::Status::EPERM,
            clipboard::WriteResult::Unsupported => kitty_clipboard::Status::ENOSYS,
            clipboard::WriteResult::Busy => kitty_clipboard::Status::EBUSY,
            clipboard::WriteResult::InvalidData => kitty_clipboard::Status::EINVAL,
            clipboard::WriteResult::IoError => kitty_clipboard::Status::EIO,
            clipboard::WriteResult::Success(success) => {
                // Remembering is only offered when the request
                // carried a usable password (checked when applied).
                if success.remember {
                    state.remember = true;
                }
                kitty_clipboard::Status::DONE
            }
        });
    }
}

