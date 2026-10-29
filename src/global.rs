//! Port of `src/global.zig`.
//!
//! STATUS: ported
//!
//! Global process state. Rust has no allocator or `std.Io` objects to hand
//! out, so the Zig `gpa`/`alloc`/`io_impl` fields and the `io()`/`alloc()`
//! accessors have no counterpart; everything else is kept. The state lives
//! in a process-wide `RwLock` and accessors return copies.

use std::ffi::OsString;
use std::fmt;
use std::sync::RwLock;

use crate::build_config;
use crate::os;
use crate::os::{EnvironMap, ResourcesDir};

/// We export the xev backend we want to use so that the rest of
/// Ghostty can import this once and have access to the proper
/// backend.
pub use crate::pkg::xev;

/// Global process state. This is initialized in main() for exe artifacts and
/// by ghostty_init() for lib artifacts. Most other methods in this file will
/// retrieve items stored in this state.
static STATE: RwLock<Option<GlobalState>> = RwLock::new(None);

/// The process environment (Zig: `std.process.Environ`). The Rust port
/// represents it as a snapshot map.
pub type Environ = EnvironMap;

/// Zig: `std.process.Init.Minimal`.
#[derive(Debug, Clone, Default)]
pub struct Minimal {
    pub args: Vec<OsString>,
    pub environ: Environ,
}

impl Minimal {
    /// The current process's arguments and environment.
    pub fn from_process() -> Self {
        Self {
            args: std::env::args_os().collect(),
            environ: os::environ_map(),
        }
    }
}

pub enum InitOpts {
    Main(Minimal),

    /// Same as `main` but for auxiliary tool binaries (e.g. ghostty-bench
    /// and ghostty-gen) that have their own CLI action namespace. This
    /// skips detection of ghostty app CLI actions, since tool actions
    /// (e.g. `+terminal-stream`) are not valid app actions and would
    /// otherwise cause init to fail with InvalidAction.
    Tool(Minimal),

    C {
        argc: usize,
        argv: *const *const std::ffi::c_char,
        environ: Environ,
    },
}

/// Errors from [`init`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitError {
    /// Zig: `error.UnsupportedOSForCApi`.
    UnsupportedOSForCApi,
    /// Multiple actions were detected on the CLI.
    MultipleActions,
    /// An unknown action was specified on the CLI.
    InvalidAction,
    /// Oniguruma failed to initialize.
    Oniguruma(crate::pkg::oniguruma::errors::Error),
    /// The resources directory path was too long.
    NoSpaceLeft,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for InitError {}

/// Initialize the global state.
pub fn init(opts: InitOpts) -> Result<(), InitError> {
    let (environ, args) = match opts {
        InitOpts::Main(ref m) | InitOpts::Tool(ref m) => (m.environ.clone(), m.args.clone()),
        InitOpts::C {
            argc,
            argv,
            ref environ,
        } => {
            // TODO: Using the C API from Windows is unsupported at this time.
            if cfg!(windows) {
                return Err(InitError::UnsupportedOSForCApi);
            }
            (environ.clone(), c_args(argc, argv))
        }
    };

    // Initialize ourself to nothing so we don't have any extra state.
    // IMPORTANT: this MUST be initialized before any log output because
    // the log function uses the global state.
    *STATE.write().unwrap() = Some(GlobalState {
        environ,
        args,
        tmp_dir_path: None,
        action: None,
        logging: Logging::default(),
        rlimits: ResourceLimits::default(),
        resources_dir: ResourcesDir::default(),
    });

    let res = init_inner(&opts);
    if res.is_err() {
        deinit();
    }
    res
}

/// Convert C `argc`/`argv` to owned arguments.
fn c_args(argc: usize, argv: *const *const std::ffi::c_char) -> Vec<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        (0..argc)
            .map(|i| {
                // SAFETY: the C API caller passes `argc` valid C strings.
                let s = unsafe { std::ffi::CStr::from_ptr(*argv.add(i)) };
                std::ffi::OsStr::from_bytes(s.to_bytes()).to_os_string()
            })
            .collect()
    }

    #[cfg(not(unix))]
    {
        let _ = (argc, argv);
        Vec::new()
    }
}

fn with_state_mut<R>(f: impl FnOnce(&mut GlobalState) -> R) -> R {
    let mut guard = STATE.write().unwrap();
    f(guard.as_mut().expect("global state not initialized"))
}

fn with_state<R>(f: impl FnOnce(&GlobalState) -> R) -> R {
    let guard = STATE.read().unwrap();
    f(guard.as_ref().expect("global state not initialized"))
}

fn init_inner(opts: &InitOpts) -> Result<(), InitError> {
    // Discover and save the temporary directory path
    with_state_mut(|s| s.tmp_dir_path = Some(os::file::alloc_tmp_dir(&s.environ)));

    // We first try to parse any action that we may be executing.
    // Tool binaries (ghostty-bench, ghostty-gen) have their own action
    // namespace and detect their own actions, so we skip detection here.
    let action = match opts {
        InitOpts::Main(_) | InitOpts::C { .. } => {
            let args = with_state(|s| s.args.clone());
            detect_args(&args)?
        }
        InitOpts::Tool(_) => None,
    };
    with_state_mut(|s| {
        s.action = action;

        // If we have an action executing, we disable logging by default
        // since we write to stderr we don't want logs messing up our
        // output.
        if s.action.is_some() {
            s.logging.stderr = false;
        }

        // I don't love the env var name but I don't have it in my heart
        // to parse CLI args 3 times (once for actions, once for config,
        // maybe once for logging) so for now this is an easy way to do
        // this. Env vars are useful for logging too because they are
        // easy to set.
        if let Some(v) = s.environ.get("GHOSTTY_LOG") {
            s.logging = parse_packed_logging(v).unwrap_or_default();
        }
    });

    // Setup our signal handlers before logging
    GlobalState::init_signals();

    // Setup our Xev backend if we're dynamic
    if xev::dynamic() {
        if let Err(err) = xev::detect() {
            log::warn!(
                "failed to detect xev backend, falling back to most compatible backend err={err}"
            );
        }
    }

    // Output some debug information right away
    log::info!("ghostty version={}", build_config::VERSION_STRING);
    log::info!("ghostty build optimize={}", build_config::MODE_STRING);
    log::info!("runtime={}", build_config::APP_RUNTIME);
    log::info!("font_backend={}", build_config::FONT_BACKEND);
    if build_config::FONT_BACKEND.has_harfbuzz() {
        log::info!(
            "dependency harfbuzz={}",
            crate::pkg::harfbuzz::version_string().to_string_lossy()
        );
    }
    if build_config::FONT_BACKEND.has_fontconfig() {
        log::info!(
            "dependency fontconfig={}",
            crate::pkg::fontconfig::version()
        );
    }
    // TODO(port-dedupe): log `crate::renderer::Renderer` once the renderer
    // is ported (Zig prints the renderer type name).
    log::info!("renderer={}", build_config::RENDERER);
    log::info!("libxev default backend={}", xev::backend());

    // As early as possible, initialize our resource limits.
    with_state_mut(|s| s.rlimits = ResourceLimits::init());

    if cfg!(feature = "sentry") {
        // Initialize our crash reporting. The environ map snapshot is
        // owned by crash.init.
        let environ_map = environ_map();
        if let Err(err) = crate::crash::init(environ_map) {
            log::warn!("sentry init failed, no crash capture available err={err}");
        }
    }

    // We need to make sure the process locale is set properly. Locale
    // affects a lot of behaviors in a shell.
    //
    // We need to re-sync the environment after this completes.
    os::ensure_locale();
    sync_environ();

    // Initialize glslang for shader compilation
    glslang_init();

    // Initialize oniguruma for regex
    crate::pkg::oniguruma::init(&[crate::pkg::oniguruma::Encoding::utf8()])
        .map_err(InitError::Oniguruma)?;

    // Find our resources directory once for the app so every launch
    // hereafter can use this cached value.
    let resources_dir = apprt_resources_dir()?;
    with_state_mut(|s| s.resources_dir = resources_dir.clone());

    // Setup i18n
    if let Some(v) = resources_dir.app() {
        if let Err(err) = os::i18n::init(v) {
            log::warn!("failed to init i18n, translations will not be available err={err}");
        }
    }

    Ok(())
}

/// TODO(port-dedupe): replace with `crate::pkg::glslang::init()` once
/// `pkg/glslang` is ported (glslang is not linked yet; it is only needed
/// for custom shaders).
fn glslang_init() {}

/// Zig: `apprt.runtime.resourcesDir`. The GTK runtime's version only
/// differs for Flatpak builds (`build_config::FLATPAK`, false here), and
/// the `none` runtime uses `internal_os.resourcesDir` directly.
///
/// TODO(port-dedupe): replace with `crate::apprt::gtk::flatpak::resources_dir`
/// once the GTK apprt is ported.
fn apprt_resources_dir() -> Result<ResourcesDir, InitError> {
    os::resources_dir().map_err(|_| InitError::NoSpaceLeft)
}

/// Cleans up the global state. This doesn't _need_ to be called but
/// doing so in dev modes will check for memory leaks.
pub fn deinit() {
    // Flush our crash logs
    crate::crash::deinit();

    // Release the state (resources dir, tmp dir path, ...).
    *STATE.write().unwrap() = None;
}

/// Helper to return either the state's environment, or one from testing.
///
/// Asserts that the global state is initialized when not running as a test.
pub fn environ() -> Environ {
    if cfg!(test) {
        return os::environ_map();
    }

    with_state(|s| s.environ.clone())
}

/// Helper to create an environment map off of the state's environment, or one
/// from testing.
///
/// Asserts that the global state is initialized when not running as a test.
pub fn environ_map() -> EnvironMap {
    environ()
}

/// Re-synchronizes the global Environ from the process. It is not valid to
/// run this within any code that needs to be run through tests. For any of
/// these, re-factor the code to take an environment map instead, where you
/// can modify the environment as needed.
///
/// NOTE: Be cognizant of where you are calling this! There is no
/// concurrency control on this function (or for that matter, `environ` or
/// `environMap`). Direct modification of the system environment is
/// discouraged, and this should be kept in mind when resorting to
/// lower-level `setenv` or `unsetenv` - as a rule, beyond initialization,
/// favor environment maps whenever possible.
pub fn sync_environ() {
    if cfg!(windows) {
        return;
    }
    debug_assert!(!cfg!(test));
    let new_environ = os::environ_map();
    with_state_mut(|s| s.environ = new_environ);
}

/// Helper to return either the state's args, or one from testing.
///
/// Asserts that the global state is initialized when not running as a test.
pub fn args() -> Vec<OsString> {
    if cfg!(test) {
        return Vec::new();
    }

    with_state(|s| s.args.clone())
}

/// Returns the temporary directory discovered from the global environment
/// (saves allocation of the temporary directory where you can get at global
/// state).
///
/// Asserts that the global state is initialized.
pub fn tmp_dir_path() -> String {
    with_state(|s| s.tmp_dir_path.clone().expect("tmp dir path not set"))
}

/// Returns the global state resources_dir, or an empty one when testing.
///
/// Asserts that the global state is initialized when not running as a test.
pub fn resources_dir() -> ResourcesDir {
    if cfg!(test) {
        return ResourcesDir::default();
    }

    with_state(|s| s.resources_dir.clone())
}

/// Returns the global state rlimits, or an empty one when testing.
///
/// Asserts that the global state is initialized when not running as a test.
pub fn rlimits() -> ResourceLimits {
    if cfg!(test) {
        return ResourceLimits::default();
    }

    with_state(|s| s.rlimits)
}

/// Returns the global state logging configuration.
///
/// Asserts that the global state is initialized.
pub fn logging() -> Logging {
    with_state(|s| s.logging)
}

/// Returns the global state action.
///
/// Asserts that the global state is initialized.
pub fn action() -> Option<Action> {
    with_state(|s| s.action)
}

/// This represents the global process state. There should only
/// be one of these at any given moment. This is extracted into a dedicated
/// struct because it is reused by main and the static C lib.
#[derive(Debug, Clone)]
pub struct GlobalState {
    pub environ: Environ,
    pub args: Vec<OsString>,
    pub tmp_dir_path: Option<String>,
    pub action: Option<Action>,
    pub logging: Logging,
    pub rlimits: ResourceLimits,

    /// The app resources directory, equivalent to zig-out/share when we build
    /// from source. This is null if we can't detect it.
    pub resources_dir: ResourcesDir,
}

/// Where logging should go (Zig: `GlobalState.Logging`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Logging {
    /// Whether to log to stderr. For lib mode we always disable stderr
    /// logging by default. Otherwise it's enabled by default.
    pub stderr: bool,
    /// Whether to log to macOS's unified logging. Enabled by default
    /// on macOS.
    pub macos: bool,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            stderr: build_config::APP_RUNTIME != crate::apprt::runtime::Runtime::None,
            macos: cfg!(target_vendor = "apple"),
        }
    }
}

impl GlobalState {
    fn init_signals() {
        // Only posix systems.
        #[cfg(unix)]
        // SAFETY: installing SIG_IGN with an empty mask.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;

            // We ignore SIGPIPE because it is a common signal we may get
            // due to how we implement termio. When a terminal is closed we
            // often write to a broken pipe to exit the read thread. This should
            // be fixed one day but for now this helps make this a bit more
            // robust.
            libc::sigaction(libc::SIGPIPE, &sa, std::ptr::null_mut());
        }
    }
}

/// Maintains the Unix resource limits that we set for our process. This
/// can be used to restore the limits to their original values.
#[derive(Clone, Copy, Default)]
pub struct ResourceLimits {
    pub nofile: Option<os::rlimit>,
}

impl fmt::Debug for ResourceLimits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResourceLimits")
            .field("nofile", &self.nofile.is_some())
            .finish()
    }
}

impl ResourceLimits {
    pub fn init() -> ResourceLimits {
        ResourceLimits {
            // Maximize the number of file descriptors we can have open
            // because we can consume a lot of them if we make many terminals.
            nofile: os::fix_max_files(),
        }
    }

    /// Restore the limits. This is async-signal-safe (it is called in the
    /// forked child by `Command`).
    pub fn restore(&self) {
        if let Some(lim) = self.nofile {
            os::restore_max_files(lim);
        }
    }
}

/// The ghostty CLI actions (Zig: `cli.ghostty.Action`).
///
/// TODO(port-dedupe): replace with `crate::cli::ghostty::Action` once
/// `src/cli/ghostty.zig` is ported (owned by the CLI port).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Version,
    Help,
    ListFonts,
    ListKeybinds,
    ListThemes,
    ListColors,
    ListActions,
    Ssh,
    SshCache,
    EditConfig,
    ShowConfig,
    ExplainConfig,
    ValidateConfig,
    ShowFace,
    CrashReport,
    Boo,
    NewWindow,
    NewTab,
    ToggleQuickTerminal,
}

impl Action {
    const ALL: [(&'static str, Action); 19] = [
        ("version", Action::Version),
        ("help", Action::Help),
        ("list-fonts", Action::ListFonts),
        ("list-keybinds", Action::ListKeybinds),
        ("list-themes", Action::ListThemes),
        ("list-colors", Action::ListColors),
        ("list-actions", Action::ListActions),
        ("ssh", Action::Ssh),
        ("ssh-cache", Action::SshCache),
        ("edit-config", Action::EditConfig),
        ("show-config", Action::ShowConfig),
        ("explain-config", Action::ExplainConfig),
        ("validate-config", Action::ValidateConfig),
        ("show-face", Action::ShowFace),
        ("crash-report", Action::CrashReport),
        ("boo", Action::Boo),
        ("new-window", Action::NewWindow),
        ("new-tab", Action::NewTab),
        ("toggle-quick-terminal", Action::ToggleQuickTerminal),
    ];

    /// Zig: `std.meta.stringToEnum`.
    pub fn from_name(name: &[u8]) -> Option<Action> {
        Self::ALL
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .map(|&(_, a)| a)
    }

    /// The Zig tag name.
    pub fn as_str(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|&&(_, a)| a == self)
            .map(|&(n, _)| n)
            .unwrap()
    }
}

/// Zig: `cli.action.SpecialCase(Action)`.
enum SpecialCase {
    Action(Action),
    Fallback(Action),
    AbortIfNoAction,
}

/// Zig: `cli.ghostty.Action.detectSpecialCase`.
fn detect_special_case(arg: &[u8]) -> Option<SpecialCase> {
    // If we see a "-e" and we haven't seen a command yet, then
    // we are done looking for commands. This special case enables
    // `ghostty -e ghostty +command`. If we've seen a command we
    // still want to keep looking because
    // `ghostty +command -e +command` is invalid.
    if arg == b"-e" {
        return Some(SpecialCase::AbortIfNoAction);
    }

    // Special case, --version always outputs the version no
    // matter what, no matter what other args exist.
    if arg == b"--version" {
        return Some(SpecialCase::Action(Action::Version));
    }

    // --help matches "help" but if a subcommand is specified
    // then we match the subcommand.
    if arg == b"--help" || arg == b"-h" {
        return Some(SpecialCase::Fallback(Action::Help));
    }

    None
}

/// Detect the action from CLI args (Zig: `cli.action.detectArgs`, which
/// skips argv[0] via the args iterator).
///
/// TODO(port-dedupe): replace with `crate::cli::action::detect_args` once
/// `src/cli/action.zig` is ported (owned by the CLI port).
fn detect_args(args: &[OsString]) -> Result<Option<Action>, InitError> {
    let mut fallback: Option<Action> = None;
    let mut pending: Option<Action> = None;
    for arg in args.iter().skip(1) {
        let arg = arg.as_encoded_bytes();

        // Allow handling of special cases.
        if let Some(special) = detect_special_case(arg) {
            match special {
                SpecialCase::Action(a) => return Ok(Some(a)),
                SpecialCase::Fallback(a) => fallback = Some(a),
                SpecialCase::AbortIfNoAction => {
                    if pending.is_none() {
                        return Ok(None);
                    }
                }
            }
        }

        // Commands must start with "+"
        if arg.first() != Some(&b'+') {
            continue;
        }
        if pending.is_some() {
            return Err(InitError::MultipleActions);
        }
        pending = Some(Action::from_name(&arg[1..]).ok_or(InitError::InvalidAction)?);
    }

    // If we have an action, we always return that action, even if we've
    // seen "--help" or "-h" because the action may have its own help text.
    if pending.is_some() {
        return Ok(pending);
    }

    // If we have no action but we have a fallback, then we return that.
    Ok(fallback)
}

/// Parse a `Logging` value the way `cli.args.parsePackedStruct` does: a
/// bool for all fields, or a comma-separated list of (optionally `no-`
/// prefixed) field names.
///
/// TODO(port-dedupe): replace with `crate::cli::args::parse_packed_struct`
/// once `src/cli/args.zig` is ported (owned by the CLI port).
fn parse_packed_logging(v: &str) -> Option<Logging> {
    fn parse_bool(v: &str) -> Option<bool> {
        match v {
            "1" | "t" | "T" | "true" => Some(true),
            "0" | "f" | "F" | "false" => Some(false),
            _ => None,
        }
    }

    // Allow standalone boolean values like "true" and "false" to
    // turn on or off all of the struct's fields.
    if let Some(b) = parse_bool(v) {
        return Some(Logging {
            stderr: b,
            macos: b,
        });
    }

    let mut result = Logging::default();
    for part_raw in v.split(',') {
        // Determine the field we're looking for and the value. If the
        // field is prefixed with "no-" then we set the value to false.
        let trimmed = part_raw.trim_matches([' ', '\t']);
        let (part, value) = match trimmed.strip_prefix("no-") {
            Some(rest) => (rest, false),
            None => (trimmed, true),
        };

        match part {
            "stderr" => result.stderr = value,
            "macos" => result.macos = value,
            // No field matched
            _ => return None,
        }
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<OsString> {
        std::iter::once("ghostty")
            .chain(v.iter().copied())
            .map(OsString::from)
            .collect()
    }

    #[test]
    fn detect_actions() {
        assert_eq!(detect_args(&argv(&[])), Ok(None));
        assert_eq!(detect_args(&argv(&["+version"])), Ok(Some(Action::Version)));
        assert_eq!(
            detect_args(&argv(&["+list-fonts", "--family=x"])),
            Ok(Some(Action::ListFonts))
        );
        assert_eq!(
            detect_args(&argv(&["+invalid"])),
            Err(InitError::InvalidAction)
        );
        assert_eq!(
            detect_args(&argv(&["+version", "+help"])),
            Err(InitError::MultipleActions)
        );
        assert_eq!(detect_args(&argv(&["--some-flag"])), Ok(None));
        assert_eq!(detect_args(&argv(&["--help"])), Ok(Some(Action::Help)));
        assert_eq!(
            detect_args(&argv(&["+list-themes", "--help"])),
            Ok(Some(Action::ListThemes))
        );
        assert_eq!(
            detect_args(&argv(&["+boo", "--version"])),
            Ok(Some(Action::Version))
        );
        assert_eq!(detect_args(&argv(&["-e", "+boo"])), Ok(None));
        assert_eq!(
            detect_args(&argv(&["+boo", "-e", "+help"])),
            Err(InitError::MultipleActions)
        );
        // argv[0] is never an action.
        assert_eq!(detect_args(&[OsString::from("+boo")]), Ok(None));
    }

    #[test]
    fn logging_parse() {
        assert_eq!(
            parse_packed_logging("true"),
            Some(Logging {
                stderr: true,
                macos: true
            })
        );
        assert_eq!(
            parse_packed_logging("false"),
            Some(Logging {
                stderr: false,
                macos: false
            })
        );
        let l = parse_packed_logging("no-stderr").unwrap();
        assert!(!l.stderr);
        assert_eq!(l.macos, Logging::default().macos);
        let l = parse_packed_logging("stderr, macos").unwrap();
        assert!(l.stderr && l.macos);
        assert_eq!(parse_packed_logging("bogus"), None);
    }

    #[test]
    fn logging_default() {
        let l = Logging::default();
        assert_eq!(l.stderr, cfg!(not(target_os = "macos")));
        assert_eq!(l.macos, cfg!(target_os = "macos"));
    }

    #[test]
    fn test_accessors() {
        // In tests, these don't require initialized state.
        assert!(args().is_empty());
        assert_eq!(resources_dir(), ResourcesDir::default());
        assert!(rlimits().nofile.is_none());
        assert_eq!(environ().get("PATH"), std::env::var("PATH").ok().as_ref());
    }

    #[test]
    fn action_names_roundtrip() {
        for (name, a) in Action::ALL {
            assert_eq!(Action::from_name(name.as_bytes()), Some(a));
            assert_eq!(a.as_str(), name);
        }
    }
}
