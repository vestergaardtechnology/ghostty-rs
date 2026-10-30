//! Port of `src/apprt/gtk/App.zig`.
//!
//! STATUS: ported
//!
//! This is the main entrypoint to the apprt for Ghostty. Ghostty will
//! initialize this in main to start the application..
//!
//! Rust notes: Zig initializes the runtime app in place (`init(self: *App,
//! ...)`) because the GObject `Application` keeps a pointer back to it.
//! [`App::init`] returns the runtime app boxed (as [`crate::apprt::App`],
//! which wraps this type in test builds) so that pointer is stable.
#![cfg(feature = "gtk")]

use std::fmt;

use crate::app::App as CoreApp;
use crate::apprt;
use crate::apprt::action::{Action, Target};
use crate::apprt::gtk::class::application::{self, Application};
use crate::apprt::gtk::ipc::new_tab::new_tab as ipc_new_tab;
use crate::apprt::gtk::ipc::new_window::new_window as ipc_new_window;
use crate::apprt::gtk::ipc::toggle_quick_terminal::toggle_quick_terminal as ipc_toggle_quick_terminal;
use crate::apprt::gtk::Surface;
use crate::apprt::ipc;

/// GTK application ID
pub const APPLICATION_ID: &str = crate::apprt::gtk::build::info::APPLICATION_ID;

/// GTK object path
pub const OBJECT_PATH: &str = crate::apprt::gtk::build::info::OBJECT_PATH;

/// Options for [`App::init`]. Required by the apprt interface but we don't
/// use it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {}

/// See the module documentation.
pub struct App {
    /// The GObject Application instance
    pub app: Application,
}

// SAFETY: the GObject application is only touched on the main thread. The
// only method other threads call is `wakeup`, which only wakes the
// (thread-safe) default main context.
unsafe impl Send for App {}
unsafe impl Sync for App {}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("gtk::App").finish_non_exhaustive()
    }
}

impl App {
    /// Create the runtime app for `core_app`. The returned box must stay
    /// alive (and in place) for as long as the application runs.
    pub fn init(
        core_app: &mut CoreApp,
        _opts: Options,
    ) -> Result<Box<apprt::App>, application::NewError> {
        let app = Application::new(core_app)?;
        let mut rt = Box::new(super::wrap_app(App { app: app.clone() }));
        app.set_rt(&mut *rt);
        Ok(rt)
    }

    pub fn run(&self) -> Result<(), application::RunError> {
        self.app.run()
    }

    pub fn terminate(&self) {
        // We force deinitialize the app. We don't unref because other things
        // tend to have a reference at this point, so this just forces the
        // disposal now.
        self.app.deinit();
    }

    /// Called by CoreApp to wake up the event loop.
    pub fn wakeup(&self) {
        Application::wakeup();
    }

    pub fn perform_action(&self, target: Target, action: Action<'_>) -> Result<bool, apprt::Error> {
        self.app.perform_action(target, action)
    }

    /// Send the given IPC to a running Ghostty. Returns `true` if the action
    /// was able to be performed, `false` otherwise.
    ///
    /// Note that this is a static function. Since this is called from a CLI
    /// app (or some other process that is not Ghostty) there is no
    /// full-featured apprt App to use.
    pub fn perform_ipc(target: &ipc::Target<'_>, action: &ipc::Action<'_>) -> Result<bool, ipc::Errors> {
        match action {
            ipc::Action::NewWindow(value) => ipc_new_window(*target, value),
            ipc::Action::NewTab(value) => ipc_new_tab(*target, value),
            ipc::Action::ToggleQuickTerminal => ipc_toggle_quick_terminal(*target),
        }
    }

    /// Redraw the inspector for the given surface.
    pub fn redraw_inspector(&self, surface: &Surface) {
        surface.redraw_inspector();
    }
}
