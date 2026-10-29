//! Port of `src/termio.zig`.
//!
//! STATUS: partial — `Termio` and `StreamHandler` are stand-ins in
//! [`standin`] until termio/{Termio,stream_handler}.zig are ported
//! (TODO(port-dedupe)).
//!
//! Termio is responsible for "terminal IO." Specifically, this is the
//! reading and writing of bytes for the underlying pty or pty-like device.
//!
//! Termio is constructed of a few components:
//!   - Termio - The main shared struct that has common logic across all
//!     backends and mailboxes (defined below).
//!   - Backend - Responsible for the actual physical IO. For example, one
//!     implementation creates a subprocess, allocates and assigns a pty,
//!     and sets up a read thread on the pty.
//!   - Mailbox - Responsible for storing/dispensing event messages to
//!     the backend. This exists separately from backends because termio
//!     is built to be both single and multi-threaded.
//!
//! Termio supports (and recommends) multi-threaded operation. Multi-threading
//! enables the read/writes to generally happen on separate threads and
//! almost always improves throughput and latency under heavy IO load. To
//! enable threading, use the Thread struct. This wraps a Termio, requires
//! specific backend/mailbox capabilities, and sets up the necessary threads.

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "termio/backend.rs"]
pub mod backend;
#[path = "termio/exec.rs"]
pub mod exec;
#[path = "termio/mailbox.rs"]
pub mod mailbox;
#[path = "termio/message.rs"]
pub mod message;
#[path = "termio/options.rs"]
pub mod options;
#[path = "termio/shell_integration.rs"]
pub mod shell_integration;
#[path = "termio/stream_handler.rs"]
pub mod stream_handler;
#[path = "termio/termio.rs"]
pub mod termio;
#[path = "termio/thread.rs"]
pub mod thread;
// @generated-mods-end

pub use self::backend::Backend;
pub use self::exec::Exec;
pub use self::mailbox::Mailbox;
pub use self::message::Message;
pub use self::options::Options;
pub use self::thread::Thread;

/// TODO(port-dedupe): replace with `stream_handler::StreamHandler`.
pub use self::standin::StreamHandler;
/// TODO(port-dedupe): replace with `termio::Termio`.
pub use self::standin::Termio;
/// TODO(port-dedupe): replace with `termio::DerivedConfig`.
pub use self::standin::TermioDerivedConfig as DerivedConfig;

/// Stand-ins for types whose Zig files are not ported yet.
///
/// The `Termio` stand-in mirrors the API of `termio/Termio.zig` that the
/// IO thread ([`Thread`]) and the exec backend ([`Exec`]) use, with the
/// same method names and arguments. Where the real methods operate on the
/// terminal (which is not ported yet), the stand-in records what it was
/// asked to do so that tests can observe it (a test double).
pub mod standin {
    use std::fmt;

    use crate::apprt;
    use crate::crash::sentry::{CrashSurface, SurfaceDimensions};
    use crate::global::xev;
    use crate::lib::tinyio::TinyIo;
    use crate::pty::{ProcessInfo, ProcessInfoValue};
    use crate::renderer;
    use crate::termio::backend;
    use crate::termio::exec;
    use crate::termio::message::{ScrollViewport, SizeReport};
    use crate::termio::{Backend, Mailbox, Message, Options, Thread};

    /// The errors from termio operations (Zig: the inferred error sets of
    /// the `Termio` methods, which include the backend errors).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Error {
        /// An error from the exec backend.
        Exec(exec::Error),

        /// An error from the event loop.
        Xev(xev::Error),

        /// A configured `input` path was not found.
        InputNotFound,

        /// A configured `input` could not be read or written.
        InputFailed,

        Unexpected,
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Error::Exec(e) => fmt::Display::fmt(e, f),
                Error::Xev(e) => write!(f, "{e:?}"),
                other => fmt::Debug::fmt(other, f),
            }
        }
    }

    impl std::error::Error for Error {}

    impl From<exec::Error> for Error {
        fn from(e: exec::Error) -> Self {
            Error::Exec(e)
        }
    }

    impl From<xev::Error> for Error {
        fn from(e: xev::Error) -> Self {
            Error::Xev(e)
        }
    }

    /// Zig: `Termio.MutexState`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MutexState {
        Locked,
        Unlocked,
    }

    /// TODO(port-dedupe): replace with
    /// `crate::terminal::kitty::clipboard::grants::Direction`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum KittyClipboardDirection {
        Read,
        Write,
    }

    /// TODO(port-dedupe): replace with `crate::termio::termio::Termio`.
    ///
    /// Like Zig, the termio is shared by raw pointer between the IO thread
    /// (which owns the backend and drains the mailbox) and the backend's
    /// read thread (which only calls [`Termio::process_output`]); every
    /// piece of state both touch is protected by the renderer state mutex.
    pub struct Termio {
        /// The backend for IO.
        pub backend: Backend,

        /// The derived configuration for this termio implementation.
        pub config: TermioDerivedConfig,

        /// The shared render state
        pub renderer_state: *mut renderer::State,

        /// A handle to wake up the renderer. This hints to the renderer that
        /// a repaint should happen.
        pub renderer_wakeup: xev::Async,

        /// The mailbox for communicating with the surface.
        pub surface_mailbox: apprt::surface::Mailbox,

        /// The cached size info
        pub size: renderer::Size,

        /// The mailbox implementation to use.
        pub mailbox: Mailbox,

        /// Test double (stand-in for the terminal stream): every byte handed
        /// to [`Termio::process_output`], in order. Protected by the renderer
        /// state mutex like the terminal it stands in for.
        pub processed: Vec<u8>,

        /// Test double: every size passed to [`Termio::resize`].
        pub resizes: Vec<renderer::Size>,

        /// Test double: how often [`Termio::reset_synchronized_output`] ran.
        pub synchronized_output_resets: usize,
    }

    // SAFETY: see the struct docs; the raw pointers reference objects that
    // outlive the termio and are synchronized like in Zig.
    unsafe impl Send for Termio {}

    impl fmt::Debug for Termio {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Termio")
                .field("backend", &self.backend)
                .field("size", &self.size)
                .field("mailbox", &self.mailbox)
                .finish_non_exhaustive()
        }
    }

    impl Termio {
        /// Initialize the termio state.
        pub fn init(opts: Options) -> Termio {
            let mut backend = opts.backend;
            // SAFETY: the surface keeps the render state (and the terminal
            // it points to) alive for the lifetime of the termio.
            unsafe {
                let state = &mut *opts.renderer_state;
                backend.init_terminal(state.terminal.as_mut());
            }

            Termio {
                backend,
                config: opts.config,
                renderer_state: opts.renderer_state,
                renderer_wakeup: opts.renderer_wakeup,
                surface_mailbox: opts.surface_mailbox,
                size: opts.size,
                mailbox: opts.mailbox,
                processed: Vec::new(),
                resizes: Vec::new(),
                synchronized_output_resets: 0,
            }
        }

        pub fn deinit(&mut self) {
            self.backend.deinit();
            self.config.deinit();
            self.mailbox.deinit();
        }

        fn state(&self) -> &renderer::State {
            // SAFETY: the render state outlives the termio.
            unsafe { &*self.renderer_state }
        }

        fn lock(&self) {
            // SAFETY: the render state's mutex outlives the termio.
            unsafe { self.state().mutex.as_ref() }.lock_uncancelable(TinyIo);
        }

        fn unlock(&self) {
            // SAFETY: as in `lock`.
            unsafe { self.state().mutex.as_ref() }.unlock(TinyIo);
        }

        pub fn thread_enter(
            &mut self,
            thread: *mut Thread,
            data: &mut TermioThreadData,
        ) -> Result<(), Error> {
            *data = TermioThreadData {
                // SAFETY: the thread outlives its thread data.
                r#loop: unsafe { &mut (*thread).r#loop },
                renderer_state: self.renderer_state,
                surface_mailbox: self.surface_mailbox,
                mailbox: &mut self.mailbox,
                // Backend must replace this on threadEnter
                backend: backend::ThreadData::Exec(exec::ThreadData::default()),
            };

            // Setup our backend
            let io: *mut Termio = self;
            self.backend.thread_enter(io, data)?;
            Ok(())
        }

        pub fn thread_exit(&mut self, data: &mut TermioThreadData) {
            self.backend.thread_exit(data);
        }

        /// Send a message to the mailbox. Depending on the mailbox type in use
        /// this may process now or it may just enqueue and process later.
        ///
        /// This will also notify the mailbox thread to process the message. If
        /// you're sending a lot of messages, it may be more efficient to use
        /// the mailbox directly and then call notify separately.
        pub fn queue_message(&mut self, msg: Message, mutex: MutexState) {
            let m = match mutex {
                // SAFETY: the render state's mutex outlives the termio.
                MutexState::Locked => Some(unsafe { self.state().mutex.as_ref() }),
                MutexState::Unlocked => None,
            };
            self.mailbox.send(msg, m);
            self.mailbox.notify();
        }

        /// Queue a write directly to the pty.
        ///
        /// If you're using termio.Thread, this must ONLY be called from the
        /// mailbox thread. If you're not on the thread, use queueMessage with
        /// mailbox messages instead.
        pub fn queue_write(
            &mut self,
            td: &mut TermioThreadData,
            data: &[u8],
            linefeed: bool,
        ) -> Result<(), Error> {
            self.backend.queue_write(td, data, linefeed)?;
            Ok(())
        }

        /// Update the configuration.
        pub fn change_config(
            &mut self,
            td: &mut TermioThreadData,
            config: &mut TermioDerivedConfig,
        ) -> Result<(), Error> {
            self.lock();
            self.config.deinit();
            self.config = std::mem::take(config);
            td.backend.change_config(&mut self.config);
            self.unlock();
            Ok(())
        }

        /// Resize the terminal.
        pub fn resize(
            &mut self,
            _td: &mut TermioThreadData,
            size: renderer::Size,
        ) -> Result<(), Error> {
            self.size = size;
            let grid_size = size.grid();

            // Enter the critical area that we want to keep small
            self.lock();
            let res = self.backend.resize(grid_size, size.terminal());
            self.resizes.push(size);
            self.unlock();
            res?;

            let _ = self.renderer_wakeup.notify();
            Ok(())
        }

        /// Make a size report.
        pub fn size_report(
            &mut self,
            _td: &mut TermioThreadData,
            _style: SizeReport,
        ) -> Result<(), Error> {
            Ok(())
        }

        /// Reset the synchronized output mode. This is usually called by timer
        /// expiration from the termio thread.
        pub fn reset_synchronized_output(&mut self) {
            self.lock();
            self.synchronized_output_resets += 1;
            self.unlock();
            let _ = self.renderer_wakeup.notify();
        }

        /// Clear the screen.
        pub fn clear_screen(
            &mut self,
            _td: &mut TermioThreadData,
            _history: bool,
        ) -> Result<(), Error> {
            Ok(())
        }

        /// Scroll the viewport
        pub fn scroll_viewport(&mut self, _scroll: ScrollViewport) {}

        /// Jump the viewport to the prompt.
        pub fn jump_to_prompt(&mut self, _delta: isize) -> Result<(), Error> {
            self.renderer_wakeup.notify()?;
            Ok(())
        }

        /// Called when focus is gained or lost (when focus events are enabled)
        pub fn focus_gained(
            &mut self,
            td: &mut TermioThreadData,
            focused: bool,
        ) -> Result<(), Error> {
            // We always notify our backend of focus changes.
            self.backend.focus_gained(td, focused)?;
            Ok(())
        }

        /// Process output from the pty. This is the manual API that users can
        /// call with pty data but it is also called by the read thread when using
        /// an exec subprocess.
        pub fn process_output(&mut self, buf: &[u8]) {
            // We are modifying terminal state from here on out and we need
            // the lock to grab our read data.
            self.lock();
            self.process_output_locked(buf);
            self.unlock();
        }

        /// Process output from readdata but the lock is already held.
        fn process_output_locked(&mut self, buf: &[u8]) {
            self.processed.extend_from_slice(buf);
        }

        /// Record a Kitty clipboard protocol session grant so future requests
        /// carrying the password skip the permission prompt.
        pub fn kitty_clipboard_grant(
            &mut self,
            _pw: &[u8],
            _dir: KittyClipboardDirection,
        ) -> Result<(), Error> {
            Ok(())
        }

        /// Sends a DSR response for the current color scheme to the pty.
        pub fn color_scheme_report(
            &mut self,
            _td: &mut TermioThreadData,
            _force: bool,
        ) -> Result<(), Error> {
            Ok(())
        }

        /// Sends a visibility report to the pty.
        pub fn visibility_report(
            &mut self,
            _td: &mut TermioThreadData,
            _visible: bool,
            _force: bool,
        ) -> Result<(), Error> {
            Ok(())
        }

        /// Get information about the process(es) attached to the backend. Returns
        /// `null` if there was an error getting the information or the information is
        /// not available on a particular platform.
        pub fn get_process_info(&mut self, info: ProcessInfo) -> Option<ProcessInfoValue<'_>> {
            self.backend.get_process_info(info)
        }
    }

    /// TODO(port-dedupe): replace with `crate::termio::termio::ThreadData`.
    ///
    /// The data the termio thread passes to the termio/backend callbacks.
    /// It lives on the IO thread's stack for the lifetime of the thread and
    /// must not move while the thread runs.
    pub struct TermioThreadData {
        /// The event loop associated with this thread. This is owned by
        /// the Thread but we have a pointer so we can queue new work to it.
        pub r#loop: *mut xev::Loop,

        /// The shared render state
        pub renderer_state: *mut renderer::State,

        /// Mailboxes for different threads
        pub surface_mailbox: apprt::surface::Mailbox,

        /// Data associated with the backend implementation (i.e. pty/exec state)
        pub backend: backend::ThreadData,
        pub mailbox: *mut Mailbox,
    }

    impl fmt::Debug for TermioThreadData {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("TermioThreadData")
                .field("backend", &self.backend)
                .finish_non_exhaustive()
        }
    }

    /// The placeholder value before `Termio::thread_enter` fills it in
    /// (Zig: `undefined`).
    impl Default for TermioThreadData {
        fn default() -> Self {
            TermioThreadData {
                r#loop: std::ptr::null_mut(),
                renderer_state: std::ptr::null_mut(),
                surface_mailbox: apprt::surface::Mailbox {
                    surface: std::ptr::null_mut(),
                    app: apprt::surface::standin::AppMailbox {
                        rt_app: std::ptr::null(),
                        mailbox: std::ptr::null(),
                    },
                },
                backend: backend::ThreadData::Exec(exec::ThreadData::default()),
                mailbox: std::ptr::null_mut(),
            }
        }
    }

    impl TermioThreadData {
        pub fn deinit(&mut self) {
            self.backend.deinit();
        }
    }

    /// TODO(port-dedupe): replace with `crate::termio::termio::DerivedConfig`.
    #[derive(Debug, Clone, Default)]
    pub struct TermioDerivedConfig {}

    impl TermioDerivedConfig {
        pub fn deinit(&mut self) {}
    }

    /// TODO(port-dedupe): replace with
    /// `crate::termio::stream_handler::StreamHandler`.
    #[derive(Debug, Default)]
    pub struct StreamHandler {}

    /// The surface recorded in the crash metadata of the IO threads.
    ///
    /// TODO(port-dedupe): Zig records `io.surface_mailbox.surface` (a
    /// `*Surface`); until `Surface` is ported (and implements
    /// [`CrashSurface`]) the IO threads record this placeholder.
    struct NoCrashSurface;

    impl CrashSurface for NoCrashSurface {
        fn crash_dimensions(&self) -> SurfaceDimensions {
            SurfaceDimensions::default()
        }
    }

    static NO_CRASH_SURFACE: NoCrashSurface = NoCrashSurface;

    /// The crash-metadata surface for a surface mailbox (Zig:
    /// `surface_mailbox.surface`).
    pub fn crash_surface(_mailbox: &apprt::surface::Mailbox) -> *const dyn CrashSurface {
        &NO_CRASH_SURFACE as &dyn CrashSurface as *const dyn CrashSurface
    }
}
