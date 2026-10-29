//! Port of `src/cli/new_window.zig`.
//!
//! STATUS: ported
//!
//! The GTK apprt's `App` is not ported yet; [`apprt_ipc`] provides its
//! `performIpc` (TODO(port-dedupe)) shared with `+new-tab` and
//! `+toggle-quick-terminal`. With the `gtk` feature it dispatches to the
//! ported `apprt/gtk/ipc/*`; without it, it performs the same D-Bus calls
//! through the `gdbus` command line tool.

use std::ffi::CStr;
use std::io::{self, BufWriter, Write};

use super::args::{self, cli_struct, ArgIterator, Error, Fields, ParseHooks, StringZ};
use super::diagnostics::{DiagnosticList, LocationKey};
use super::ghostty::{global_args, Action, RunResult};
use crate::global;
use crate::os::homedir;

cli_struct! {
    #[derive(Debug, Clone)]
    pub struct Options {
        /// If set, open up a new window in a custom instance of Ghostty.
        pub class: Option<StringZ> = None,
    }
    private {
        /// Did the user specify a `--working-directory` argument on the command line?
        pub _working_directory_seen: bool = false,

        /// All of the arguments after `+new-window`. They will be sent to Ghosttty
        /// for processing.
        pub _arguments: Vec<StringZ> = Vec::new(),

        /// Enable arg parsing diagnostics so that we don't get an error if
        /// there is a "normal" config setting on the cli.
        pub _diagnostics: DiagnosticList = DiagnosticList::default(),
    }
    with { custom_hooks }
}

impl ParseHooks for Options {
    fn diagnostics_mut(&mut self) -> Option<&mut DiagnosticList> {
        Some(&mut self._diagnostics)
    }

    /// Enables "-h" and "--help" to work.
    fn help(&mut self) -> Option<Result<(), Error>> {
        Action::help_result()
    }

    /// Manual parse hook, collect all of the arguments after `+new-window`.
    fn parse_manually_hook<I: ArgIterator + ?Sized>(&mut self, arg: &str, iter: &mut I) -> Result<bool, Error> {
        let mut e_seen = arg == "-e";

        // Include the argument that triggered the manual parse hook.
        if let Some(a) = self.check_arg(arg)? {
            self._arguments.push(a);
        }

        // Gather up the rest of the arguments to use as the command.
        while let Some(param) = iter.next() {
            if e_seen {
                self._arguments.push(StringZ::from(param.as_str()));
                continue;
            }
            if param == "-e" {
                e_seen = true;
                self._arguments.push(StringZ::from(param.as_str()));
                continue;
            }
            if let Some(a) = self.check_arg(&param)? {
                self._arguments.push(a);
            }
        }

        Ok(false)
    }
}

/// The Zig error name of an I/O error from `realPathFile`.
pub(crate) fn realpath_error_name(err: &io::Error) -> &'static str {
    match err.kind() {
        io::ErrorKind::NotFound => "FileNotFound",
        io::ErrorKind::PermissionDenied => "AccessDenied",
        io::ErrorKind::NotADirectory => "NotDir",
        io::ErrorKind::InvalidFilename => "NameTooLong",
        _ => "Unexpected",
    }
}

/// Resolve a `--working-directory` value (expand `~/`, make absolute).
pub(crate) fn resolve_working_directory(stripped: &str) -> Result<String, Error> {
    let mut expandhome_buf = vec![0u8; libc::PATH_MAX as usize];
    let env = global::environ_map();
    let expanded = homedir::expand_home(&env, stripped, &mut expandhome_buf).map_err(|e| match e {
        homedir::Error::BufferTooSmall => Error::Other("BufferTooSmall"),
        homedir::Error::HomeDetectionFailed => Error::Other("HomeDetectionFailed"),
    })?;
    let realpath = std::fs::canonicalize(expanded).map_err(|e| Error::Other(realpath_error_name(&e)))?;
    Ok(realpath.to_string_lossy().into_owned())
}

/// Zig `std.ascii.whitespace`.
pub(crate) const ASCII_WHITESPACE: &[char] = &[' ', '\t', '\n', '\r', '\x0b', '\x0c'];

impl Options {
    fn check_arg(&mut self, arg: &str) -> Result<Option<StringZ>, Error> {
        if let Some(rest) = arg.strip_prefix("--class=") {
            self.class = Some(StringZ::from(rest.trim_matches(ASCII_WHITESPACE)));
            return Ok(None);
        }

        if let Some(rest) = arg.strip_prefix("--working-directory=") {
            let stripped = rest.trim_matches(ASCII_WHITESPACE);
            if stripped == "home" {
                return Ok(Some(StringZ::from(arg)));
            }
            if stripped == "inherit" {
                return Ok(Some(StringZ::from(arg)));
            }
            let realpath = resolve_working_directory(stripped)?;
            self._working_directory_seen = true;
            return Ok(Some(StringZ::from(format!("--working-directory={}", realpath).as_str())));
        }

        Ok(Some(StringZ::from(arg)))
    }
}

/// The help text of this action (Zig `help_strings.Action.@"new-window"`).
pub const HELP: &str = include_str!("new_window_help.txt");

/// The `new-window` will use native platform IPC to open up a new window in a
/// running instance of Ghostty. See [`HELP`].
pub fn run() -> RunResult {
    let mut iter = args::args_iterator(global_args());

    let mut stderr = BufWriter::with_capacity(1024, io::stderr().lock());

    let result = run_args(&mut iter, &mut stderr);
    let _ = stderr.flush();
    result
}

/// Print the diagnostics that are about our own options. Returns true if
/// any were printed (Zig: the `exit` flag).
pub(crate) fn print_own_diagnostics<W: Write + ?Sized>(
    diagnostics: &DiagnosticList,
    fields: &[args::FieldInfo],
    stderr: &mut W,
) -> io::Result<bool> {
    let mut exit = false;
    for diagnostic in diagnostics.items() {
        if diagnostic.location.key() != LocationKey::Cli {
            continue;
        }
        for field in fields {
            if field.name.starts_with('_') {
                continue;
            }
            if field.name == diagnostic.key.as_str() {
                writeln!(stderr, "config error: {}", diagnostic)?;
                exit = true;
            }
        }
    }
    Ok(exit)
}

fn run_args<I: ArgIterator + ?Sized, W: Write + ?Sized>(args_iter: &mut I, stderr: &mut W) -> RunResult {
    let mut opts = Options::default();

    match args::parse(&mut opts, args_iter) {
        Ok(()) => {}
        Err(err) if err == Action::HELP_ERROR => return Err(Box::new(err)),
        Err(err) => {
            writeln!(stderr, "Error parsing args: {}", err)?;
            return Ok(1);
        }
    }

    // Print out any diagnostics, unless it's likely that the diagnostic was
    // generated trying to parse a "normal" configuration setting. Exit with an
    // error code if any diagnostics were printed.
    if !opts._diagnostics.empty() && print_own_diagnostics(&opts._diagnostics, Options::FIELDS, stderr)? {
        return Ok(1);
    }

    if !opts._working_directory_seen {
        let wd = std::fs::canonicalize(".")?;
        // This should be inserted at the beginning of the list, just in case `-e` was used.
        opts._arguments.insert(
            0,
            StringZ::from(format!("--working-directory={}", wd.to_string_lossy()).as_str()),
        );
    }

    let arguments: Vec<&CStr> = opts._arguments.iter().map(StringZ::as_c_str).collect();
    let class = opts.class.as_ref().map(StringZ::as_c_str);
    let target = match class {
        Some(class) => apprt_ipc::Target::Class(class),
        None => apprt_ipc::Target::Detect,
    };

    match apprt_ipc::App::perform_ipc(
        &target,
        &apprt_ipc::Action::NewWindow(apprt_ipc::NewWindow {
            arguments: if arguments.is_empty() { None } else { Some(&arguments) },
        }),
    ) {
        Ok(true) => return Ok(0),
        Ok(false) => {}
        Err(apprt_ipc::Errors::IpcFailed) => {
            // The apprt should have printed a more specific error message
            // already.
            return Ok(1);
        }
    }

    // If we get here, the platform is not supported.
    write!(stderr, "+new-window is not supported on this platform.\n")?;
    Ok(1)
}

/// Stand-in for `apprt.ipc` and the GTK apprt's `App.performIpc`
/// (`apprt/gtk/ipc/*.zig`).
///
/// TODO(port-dedupe): replace with `crate::apprt::App::perform_ipc` once
/// `apprt/gtk/App.zig` is ported (the IPC types are `crate::apprt::ipc`).
/// With the `gtk` feature this dispatches to `crate::apprt::gtk::ipc`
/// exactly like `App.performIpc`. Without it (GIO not linked), the same
/// D-Bus `org.gtk.Actions.Activate` call is made through the `gdbus`
/// command line tool (the exact commands are documented in the Zig sources).
pub mod apprt_ipc {
    #![cfg_attr(feature = "gtk", allow(dead_code))]

    use std::ffi::CStr;
    use std::io::Write;
    use std::process::{Command, Stdio};

    pub use crate::apprt::ipc::{Action, Errors, NewTab, NewWindow, Target};

    /// GTK `App.application_id` / `App.object_path`.
    const APPLICATION_ID: &str = if cfg!(debug_assertions) {
        "com.mitchellh.ghostty-debug"
    } else {
        "com.mitchellh.ghostty"
    };
    const OBJECT_PATH: &str = if cfg!(debug_assertions) {
        "/com/mitchellh/ghostty_debug"
    } else {
        "/com/mitchellh/ghostty"
    };

    pub struct App;

    impl App {
        /// Returns Ok(false) if the platform doesn't support IPC.
        pub fn perform_ipc(target: &Target<'_>, action: &Action<'_>) -> Result<bool, Errors> {
            if !cfg!(any(target_os = "linux", target_os = "freebsd")) {
                return Ok(false);
            }

            #[cfg(feature = "gtk")]
            return {
                use crate::apprt::gtk::ipc;
                match action {
                    Action::NewWindow(v) => ipc::new_window::new_window(*target, v),
                    Action::NewTab(v) => ipc::new_tab::new_tab(*target, v),
                    Action::ToggleQuickTerminal => {
                        ipc::toggle_quick_terminal::toggle_quick_terminal(*target)
                    }
                }
            };

            #[cfg(not(feature = "gtk"))]
            Self::perform_ipc_gdbus(target, action)
        }

        /// `performIpc` through the `gdbus` command line tool.
        fn perform_ipc_gdbus(target: &Target<'_>, action: &Action<'_>) -> Result<bool, Errors> {
            match action {
                Action::NewWindow(v) => {
                    // `(as)`: the arguments.
                    let param = format!("<@(as) ({})>", string_array(v.arguments));
                    dbus_activate(target, "new-window", &format!("[{}]", param))
                }
                Action::NewTab(v) => {
                    // `(tas)`: the target surface ID and the arguments.
                    let param = format!("<@(tas) ({}, {})>", v.surface_id, string_array(v.arguments));
                    dbus_activate(target, "new-tab", &format!("[{}]", param))
                }
                Action::ToggleQuickTerminal => dbus_activate(target, "toggle-quick-terminal", "[]"),
            }
        }
    }

    /// A GVariant text format string array.
    fn string_array(arguments: Option<&[&CStr]>) -> String {
        let mut out = String::from("@as [");
        for (i, arg) in arguments.unwrap_or(&[]).iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&gvariant_string(&arg.to_string_lossy()));
        }
        out.push(']');
        out
    }

    /// Quote a string in GVariant text format.
    pub(crate) fn gvariant_string(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// `g_application_id_is_valid`.
    pub(crate) fn application_id_is_valid(id: &str) -> bool {
        if id.is_empty() || id.len() > 255 || id.starts_with('.') || id.ends_with('.') {
            return false;
        }
        let elements: Vec<&str> = id.split('.').collect();
        if elements.len() < 2 {
            return false;
        }
        elements.iter().all(|e| {
            !e.is_empty()
                && !e.starts_with(|c: char| c.is_ascii_digit())
                && e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    }

    /// `g_variant_is_object_path`.
    pub(crate) fn is_object_path(path: &str) -> bool {
        if !path.starts_with('/') {
            return false;
        }
        if path == "/" {
            return true;
        }
        if path.ends_with('/') {
            return false;
        }
        path[1..]
            .split('/')
            .all(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    }

    fn dbus_activate(target: &Target<'_>, action: &str, parameters: &str) -> Result<bool, Errors> {
        let mut stderr = std::io::stderr();

        // Get the appropriate bus name and object path for contacting the
        // Ghostty instance we're interested in.
        let (bus_name, object_path) = match target {
            Target::Class(class) => {
                // Force the usage of the class specified on the CLI to
                // determine the bus name and object path.
                let class = class.to_string_lossy().into_owned();
                let object_path = format!("/{}", class).replace('.', "/").replace('-', "_");
                (class, object_path)
            }
            Target::Detect => (APPLICATION_ID.to_owned(), OBJECT_PATH.to_owned()),
        };

        if !application_id_is_valid(&bus_name) {
            let _ = writeln!(stderr, "D-Bus bus name is not valid: {}", bus_name);
            return Err(Errors::IpcFailed);
        }
        if !is_object_path(&object_path) {
            let _ = writeln!(stderr, "D-Bus object path is not valid: {}", object_path);
            return Err(Errors::IpcFailed);
        }

        let output = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                &bus_name,
                "--object-path",
                &object_path,
                "--method",
                "org.gtk.Actions.Activate",
                action,
                parameters,
                "{}",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        match output {
            Err(err) => {
                let _ = writeln!(
                    stderr,
                    "Unable to establish connection to D-Bus session bus: {}",
                    err
                );
                Err(Errors::IpcFailed)
            }
            Ok(output) if !output.status.success() => {
                let _ = writeln!(
                    stderr,
                    "D-Bus method call returned an error err={}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                Err(Errors::IpcFailed)
            }
            Ok(_) => Ok(true),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn validation() {
            assert!(application_id_is_valid("com.mitchellh.ghostty"));
            assert!(application_id_is_valid("com.mitchellh.ghostty-debug"));
            assert!(!application_id_is_valid("ghostty"));
            assert!(!application_id_is_valid("com..ghostty"));
            assert!(is_object_path("/com/mitchellh/ghostty"));
            assert!(!is_object_path("/com/mitchellh/ghostty-debug"));
            assert!(!is_object_path("com/x"));
        }

        #[test]
        fn gvariant_strings() {
            assert_eq!(r#""a\"b\\c""#, gvariant_string(r#"a"b\c"#));
            let a = c"-e";
            let b = c"echo";
            assert_eq!(r#"@as ["-e", "echo"]"#, string_array(Some(&[a, b])));
            assert_eq!("@as []", string_array(None));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::ArgIteratorGeneral;

    #[test]
    fn collects_arguments() {
        let mut opts = Options::default();
        let mut iter = ArgIteratorGeneral::init("--class=com.example.app --title=foo -e echo --class=x");
        args::parse(&mut opts, &mut iter).unwrap();
        assert_eq!(Some("com.example.app"), opts.class.as_deref());
        let args: Vec<&str> = opts._arguments.iter().map(StringZ::as_str).collect();
        assert_eq!(vec!["--title=foo", "-e", "echo", "--class=x"], args);
        assert!(!opts._working_directory_seen);
    }

    #[test]
    fn working_directory_is_resolved() {
        let mut opts = Options::default();
        let mut iter = ArgIteratorGeneral::init("--working-directory=. --working-directory=home");
        args::parse(&mut opts, &mut iter).unwrap();
        assert!(opts._working_directory_seen);
        let cwd = std::fs::canonicalize(".").unwrap();
        assert_eq!(
            format!("--working-directory={}", cwd.display()),
            opts._arguments[0].as_str()
        );
        assert_eq!("--working-directory=home", opts._arguments[1].as_str());
    }
}
