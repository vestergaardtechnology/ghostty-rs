//! Port of `src/termio.zig`.
//!
//! STATUS: partial — `Exec`, `Termio`, `Thread` and `StreamHandler` are
//! stand-ins in [`standin`] until termio/{Exec,Termio,Thread,
//! stream_handler}.zig are ported (TODO(port-dedupe)).
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
pub use self::mailbox::Mailbox;
pub use self::message::Message;
pub use self::options::Options;

/// TODO(port-dedupe): replace with `exec::Exec`.
pub use self::standin::Exec;
/// TODO(port-dedupe): replace with `stream_handler::StreamHandler`.
pub use self::standin::StreamHandler;
/// TODO(port-dedupe): replace with `termio::Termio`.
pub use self::standin::Termio;
/// TODO(port-dedupe): replace with `termio::DerivedConfig`.
pub use self::standin::TermioDerivedConfig as DerivedConfig;
/// TODO(port-dedupe): replace with `thread::Thread`.
pub use self::standin::Thread;

/// Stand-ins for types whose Zig files are not ported yet. The `Exec`
/// stand-in implements the backend interface used by [`Backend`] as no-ops.
pub mod standin {
    use std::fmt;

    use crate::pty::{ProcessInfo, ProcessInfoValue};
    use crate::renderer::{GridSize, ScreenSize};
    use crate::renderer::state::standin::Terminal;

    /// Errors from the stand-in backend operations.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Error {
        Unexpected,
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Debug::fmt(self, f)
        }
    }

    impl std::error::Error for Error {}

    /// TODO(port-dedupe): replace with `crate::termio::exec::Exec`.
    #[derive(Debug, Default)]
    pub struct Exec {}

    /// TODO(port-dedupe): replace with `crate::termio::exec::Config`.
    #[derive(Debug, Clone, Default)]
    pub struct ExecConfig {}

    /// TODO(port-dedupe): replace with `crate::termio::exec::ThreadData`.
    #[derive(Debug, Default)]
    pub struct ExecThreadData {}

    impl ExecThreadData {
        pub fn deinit(&mut self) {}
    }

    impl Exec {
        pub fn deinit(&mut self) {}

        pub fn init_terminal(&mut self, _t: &mut Terminal) {}

        pub fn thread_enter(
            &mut self,
            _io: &mut Termio,
            _td: &mut TermioThreadData,
        ) -> Result<(), Error> {
            Ok(())
        }

        pub fn thread_exit(&mut self, _td: &mut TermioThreadData) {}

        pub fn focus_gained(
            &mut self,
            _td: &mut TermioThreadData,
            _focused: bool,
        ) -> Result<(), Error> {
            Ok(())
        }

        pub fn resize(&mut self, _grid_size: GridSize, _screen_size: ScreenSize) -> Result<(), Error> {
            Ok(())
        }

        pub fn queue_write(
            &mut self,
            _td: &mut TermioThreadData,
            _data: &[u8],
            _linefeed: bool,
        ) -> Result<(), Error> {
            Ok(())
        }

        pub fn child_exited_abnormally(
            &mut self,
            _t: &mut Terminal,
            _exit_code: u32,
            _runtime_ms: u64,
        ) -> Result<(), Error> {
            Ok(())
        }

        pub fn get_process_info(&mut self, _info: ProcessInfo) -> Option<ProcessInfoValue<'_>> {
            None
        }
    }

    /// TODO(port-dedupe): replace with `crate::termio::termio::Termio`.
    #[derive(Debug, Default)]
    pub struct Termio {}

    /// TODO(port-dedupe): replace with `crate::termio::termio::ThreadData`.
    #[derive(Debug, Default)]
    pub struct TermioThreadData {}

    /// TODO(port-dedupe): replace with `crate::termio::termio::DerivedConfig`.
    #[derive(Debug, Clone, Default)]
    pub struct TermioDerivedConfig {}

    impl TermioDerivedConfig {
        pub fn deinit(&mut self) {}
    }

    /// TODO(port-dedupe): replace with `crate::termio::thread::Thread`.
    #[derive(Debug, Default)]
    pub struct Thread {}

    /// TODO(port-dedupe): replace with
    /// `crate::termio::stream_handler::StreamHandler`.
    #[derive(Debug, Default)]
    pub struct StreamHandler {}
}
