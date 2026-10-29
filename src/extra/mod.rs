//! Module grouping (no Zig counterpart file; mirrors a directory).
//!
//! The Zig generators in `extra/` run at comptime and inspect the types of
//! `Config`'s fields and of each CLI action's `Options` with `@typeInfo`.
//! Rust has no such reflection, so this module provides the subset they
//! need: [`config_fields`] (from `Config::FIELDS` plus the field types via
//! the [`Value!`](crate::config::key::Value) macro) and [`ACTIONS`] (the
//! CLI actions with their options and help text).

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "bash.rs"]
pub mod bash;
#[path = "fish.rs"]
pub mod fish;
#[path = "pkg_config.rs"]
pub mod pkg_config;
#[path = "sublime.rs"]
pub mod sublime;
#[path = "vim.rs"]
pub mod vim;
#[path = "zsh.rs"]
pub mod zsh;
// @generated-mods-end

use std::marker::PhantomData;
use std::sync::LazyLock;

use crate::cli::args::{CliEnum, Fields, PackedStruct};
use crate::config::path::RepeatablePath;
use crate::config::Config;

/// The parts of Zig's `@typeInfo` of a struct field type that the
/// generators inspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// `bool`
    Bool,
    /// `?bool`
    OptionalBool,
    /// An `enum` (with its tag names).
    Enum(&'static [&'static str]),
    /// An optional `enum` (with the child's tag names).
    OptionalEnum(&'static [&'static str]),
    /// A `packed struct` of bools (with its field names). Zig only treats
    /// packed structs without a `parseCLI` decl as flags; every Rust
    /// [`PackedStruct`] in `Config` uses the default flags parser.
    PackedStruct(&'static [&'static str]),
    /// `Config.RepeatablePath`
    RepeatablePath,
    /// Any other optional type.
    Optional,
    /// Anything else (ints, floats, strings, other structs, unions, ...).
    Other,
}

impl FieldType {
    /// Zig `T == bool` (`?bool` is `.optional`).
    pub fn is_bool(self) -> bool {
        self == FieldType::Bool
    }

    /// Zig `switch (T) { bool, ?bool => ... }`.
    pub fn is_bool_or_optional_bool(self) -> bool {
        matches!(self, FieldType::Bool | FieldType::OptionalBool)
    }
}

/// A struct field: its (Zig) name and type information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub ty: FieldType,
}

// Autoref-based specialization to compute a [`FieldType`] from a concrete
// type in a macro (the most specific impl has the most references).
#[doc(hidden)]
pub struct Probe<T>(pub PhantomData<T>);

#[doc(hidden)]
pub trait ProbeConcrete {
    fn field_type(&self) -> FieldType;
}
impl ProbeConcrete for &&&&&&Probe<bool> {
    fn field_type(&self) -> FieldType {
        FieldType::Bool
    }
}
impl ProbeConcrete for &&&&&&Probe<Option<bool>> {
    fn field_type(&self) -> FieldType {
        FieldType::OptionalBool
    }
}
impl ProbeConcrete for &&&&&&Probe<RepeatablePath> {
    fn field_type(&self) -> FieldType {
        FieldType::RepeatablePath
    }
}

#[doc(hidden)]
pub trait ProbeOptionalEnum {
    fn field_type(&self) -> FieldType;
}
impl<E: CliEnum> ProbeOptionalEnum for &&&&&Probe<Option<E>> {
    fn field_type(&self) -> FieldType {
        FieldType::OptionalEnum(E::FIELD_NAMES)
    }
}

#[doc(hidden)]
pub trait ProbeOptional {
    fn field_type(&self) -> FieldType;
}
impl<T> ProbeOptional for &&&&Probe<Option<T>> {
    fn field_type(&self) -> FieldType {
        FieldType::Optional
    }
}

#[doc(hidden)]
pub trait ProbeEnum {
    fn field_type(&self) -> FieldType;
}
impl<E: CliEnum> ProbeEnum for &&&Probe<E> {
    fn field_type(&self) -> FieldType {
        FieldType::Enum(E::FIELD_NAMES)
    }
}

#[doc(hidden)]
pub trait ProbePacked {
    fn field_type(&self) -> FieldType;
}
impl<P: PackedStruct> ProbePacked for &&Probe<P> {
    fn field_type(&self) -> FieldType {
        FieldType::PackedStruct(P::FIELD_NAMES)
    }
}

#[doc(hidden)]
pub trait ProbeOther {
    fn field_type(&self) -> FieldType;
}
impl<T> ProbeOther for &Probe<T> {
    fn field_type(&self) -> FieldType {
        FieldType::Other
    }
}

/// The [`FieldType`] of a concrete type (Zig `@typeInfo(T)`).
macro_rules! field_type_of {
    ($t:ty) => {{
        #[allow(unused_imports)]
        use $crate::extra::{
            ProbeConcrete as _, ProbeEnum as _, ProbeOptional as _, ProbeOptionalEnum as _,
            ProbeOther as _, ProbePacked as _,
        };
        (&&&&&&&$crate::extra::Probe::<$t>(::std::marker::PhantomData)).field_type()
    }};
}
#[allow(unused_imports)]
pub(crate) use field_type_of;

macro_rules! config_field_types {
    ($($key:tt)*) => {
        vec![$( ($key, field_type_of!($crate::config::key::Value!($key))) ),*]
    };
}

/// The types of all `Config` fields by key, in field order. Checked
/// against `Config::FIELDS` in the tests.
fn config_field_types() -> &'static [(&'static str, FieldType)] {
    static TYPES: LazyLock<Vec<(&'static str, FieldType)>> = LazyLock::new(|| {
        config_field_types!(
            "language"
            "font-family"
            "font-family-bold"
            "font-family-italic"
            "font-family-bold-italic"
            "font-style"
            "font-style-bold"
            "font-style-italic"
            "font-style-bold-italic"
            "font-synthetic-style"
            "font-feature"
            "font-size"
            "font-variation"
            "font-variation-bold"
            "font-variation-italic"
            "font-variation-bold-italic"
            "font-codepoint-map"
            "clipboard-codepoint-map"
            "font-thicken"
            "font-thicken-strength"
            "font-shaping-break"
            "alpha-blending"
            "adjust-cell-width"
            "adjust-cell-height"
            "adjust-font-baseline"
            "adjust-underline-position"
            "adjust-underline-thickness"
            "adjust-strikethrough-position"
            "adjust-strikethrough-thickness"
            "adjust-overline-position"
            "adjust-overline-thickness"
            "adjust-cursor-thickness"
            "adjust-cursor-height"
            "adjust-box-thickness"
            "adjust-icon-height"
            "grapheme-width-method"
            "freetype-load-flags"
            "theme"
            "background"
            "foreground"
            "background-image"
            "background-image-opacity"
            "background-image-position"
            "background-image-fit"
            "background-image-repeat"
            "selection-foreground"
            "selection-background"
            "selection-clear-on-typing"
            "selection-clear-on-copy"
            "selection-word-chars"
            "minimum-contrast"
            "palette"
            "palette-generate"
            "palette-harmonious"
            "cursor-color"
            "cursor-opacity"
            "cursor-style"
            "cursor-style-blink"
            "cursor-text"
            "cursor-click-to-move"
            "mouse-hide-while-typing"
            "scroll-to-bottom"
            "mouse-shift-capture"
            "mouse-reporting"
            "mouse-scroll-multiplier"
            "background-opacity"
            "background-opacity-cells"
            "background-blur"
            "unfocused-split-opacity"
            "unfocused-split-fill"
            "split-divider-color"
            "split-preserve-zoom"
            "search-foreground"
            "search-background"
            "search-selected-foreground"
            "search-selected-background"
            "command"
            "initial-command"
            "notify-on-command-finish"
            "notify-on-command-finish-action"
            "notify-on-command-finish-after"
            "env"
            "input"
            "wait-after-command"
            "abnormal-command-exit-runtime"
            "scrollback-limit-bytes"
            "scrollback-limit-lines"
            "scrollback-compression"
            "scrollbar"
            "link"
            "link-url"
            "link-osc8"
            "link-previews"
            "maximize"
            "fullscreen"
            "title"
            "class"
            "x11-instance-name"
            "working-directory"
            "keybind"
            "key-remap"
            "window-padding-x"
            "window-padding-y"
            "window-padding-balance"
            "window-padding-color"
            "window-vsync"
            "window-inherit-working-directory"
            "tab-inherit-working-directory"
            "split-inherit-working-directory"
            "window-inherit-font-size"
            "window-decoration"
            "window-title-font-family"
            "window-subtitle"
            "window-theme"
            "window-colorspace"
            "window-height"
            "window-width"
            "window-position-x"
            "window-position-y"
            "window-save-state"
            "window-step-resize"
            "window-new-tab-position"
            "window-show-tab-bar"
            "window-titlebar-background"
            "window-titlebar-foreground"
            "drag-handle"
            "resize-overlay"
            "resize-overlay-position"
            "resize-overlay-duration"
            "focus-follows-mouse"
            "clipboard-read"
            "clipboard-write"
            "clipboard-write-limit-bytes"
            "clipboard-trim-trailing-spaces"
            "clipboard-paste-protection"
            "clipboard-paste-bracketed-safe"
            "title-report"
            "image-storage-limit"
            "copy-on-select"
            "right-click-action"
            "middle-click-action"
            "click-repeat-interval"
            "config-file"
            "config-default-files"
            "confirm-close-surface"
            "quit-after-last-window-closed"
            "quit-after-last-window-closed-delay"
            "initial-window"
            "undo-timeout"
            "quick-terminal-position"
            "quick-terminal-size"
            "gtk-quick-terminal-layer"
            "gtk-quick-terminal-namespace"
            "quick-terminal-screen"
            "quick-terminal-animation-duration"
            "quick-terminal-autohide"
            "quick-terminal-space-behavior"
            "quick-terminal-keyboard-interactivity"
            "shell-integration"
            "shell-integration-features"
            "command-palette-entry"
            "osc-color-report-format"
            "vt-kam-allowed"
            "vt-window-resize-allowed"
            "vt-xt-checksum-report"
            "vt-xt-checksum-extension"
            "custom-shader"
            "custom-shader-animation"
            "bell-features"
            "bell-audio-path"
            "bell-audio-volume"
            "app-notifications"
            "macos-non-native-fullscreen"
            "macos-window-buttons"
            "macos-titlebar-style"
            "macos-titlebar-proxy-icon"
            "macos-dock-drop-behavior"
            "macos-option-as-alt"
            "macos-window-shadow"
            "macos-hidden"
            "macos-auto-secure-input"
            "macos-secure-input-indication"
            "macos-applescript"
            "macos-icon"
            "macos-custom-icon"
            "macos-icon-frame"
            "macos-icon-ghost-color"
            "macos-icon-screen-color"
            "macos-shortcuts"
            "linux-cgroup"
            "linux-cgroup-memory-limit"
            "linux-cgroup-processes-limit"
            "linux-cgroup-hard-fail"
            "gtk-opengl-debug"
            "gtk-single-instance"
            "gtk-titlebar"
            "gtk-tabs-location"
            "gtk-tabs-inverted"
            "gtk-titlebar-hide-when-maximized"
            "gtk-toolbar-style"
            "gtk-titlebar-style"
            "gtk-wide-tabs"
            "gtk-horizontal-tab-scroll"
            "gtk-custom-css"
            "desktop-notifications"
            "progress-style"
            "bold-color"
            "faint-opacity"
            "term"
            "enquiry-response"
            "async-backend"
            "auto-update"
            "auto-update-channel"
        )
    });
    &TYPES
}

/// All non-internal `Config` fields (Zig: the fields of `Config` whose name
/// doesn't start with `_`) with their types, in declaration order.
pub fn config_fields() -> &'static [Field] {
    static FIELDS: LazyLock<Vec<Field>> = LazyLock::new(|| {
        let types = config_field_types();
        <Config as Fields>::FIELDS
            .iter()
            .map(|f| Field {
                name: f.name,
                ty: types
                    .iter()
                    .find(|(k, _)| *k == f.name)
                    .map(|(_, t)| *t)
                    .unwrap_or(FieldType::Other),
            })
            .collect()
    });
    &FIELDS
}

/// The help string for a `Config` field (`help_strings.Config.<field>`), or
/// `None` if the field has no doc comment.
pub fn config_help(name: &str) -> Option<String> {
    let field = <Config as Fields>::FIELDS.iter().find(|f| f.name == name)?;
    if !field.has_help() {
        return None;
    }
    Some(field.help())
}

// ---------------------------------------------------------------------------
// CLI actions
// ---------------------------------------------------------------------------

// TODO(port-dedupe): replace with crate::cli::ghostty::Action (its
// `options()` types and the `help_strings.Action` docs, i.e. the doc
// comments of each action's `run` function) once `src/cli/*` is ported.

/// A CLI action (`+<name>`) as seen by the generators.
#[derive(Debug, Clone, Copy)]
pub struct Action {
    /// The Zig tag name.
    pub name: &'static str,
    /// All fields of the action's `Options` struct, including internal
    /// (`_`-prefixed) ones.
    pub options: &'static [Field],
    /// The doc comment lines (text after `///`) of the action's `run`
    /// function.
    pub doc_lines: &'static [&'static str],
}

impl Action {
    /// Zig `action.file()`: the source file of the action relative to the
    /// root `src/` directory.
    pub fn file(&self) -> String {
        format!("cli/{}.zig", self.name.replace('-', "_"))
    }

    /// `help_strings.Action.<name>`: the doc comment with the common
    /// leading-space prefix removed.
    pub fn help(&self) -> String {
        let prefix = find_common_prefix(self.doc_lines);
        self.doc_lines
            .iter()
            .map(|l| &l[prefix.min(l.len())..])
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `helpgen.zig` `findCommonPrefix`: the smallest number of leading spaces
/// over all lines (lines of only spaces don't count).
pub fn find_common_prefix(lines: &[&str]) -> usize {
    let mut m = usize::MAX;
    for line in lines {
        let n = line.bytes().position(|c| c != b' ').unwrap_or(usize::MAX);
        m = m.min(n);
    }
    m
}

const fn f(name: &'static str, ty: FieldType) -> Field {
    Field { name, ty }
}

const COLOR_SCHEME: &[&str] = &["all", "dark", "light"];
const FONT_STYLE: &[&str] = &["regular", "bold", "italic", "bold_italic"];
const FONT_PRESENTATION: &[&str] = &["text", "emoji"];

/// The CLI actions in `cli/ghostty.zig` `Action` order.
pub const ACTIONS: &[Action] = &[
    Action {
        name: "version",
        options: &[],
        doc_lines: DOC_VERSION,
    },
    Action {
        name: "help",
        options: &[f("help", FieldType::Bool)],
        doc_lines: DOC_HELP,
    },
    Action {
        name: "list-fonts",
        options: &[
            f("_arena", FieldType::Optional),
            f("family", FieldType::Optional),
            f("style", FieldType::Optional),
            f("bold", FieldType::Bool),
            f("italic", FieldType::Bool),
        ],
        doc_lines: DOC_LIST_FONTS,
    },
    Action {
        name: "list-keybinds",
        options: &[
            f("default", FieldType::Bool),
            f("docs", FieldType::Bool),
            f("plain", FieldType::Bool),
        ],
        doc_lines: DOC_LIST_KEYBINDS,
    },
    Action {
        name: "list-themes",
        options: &[
            f("path", FieldType::Bool),
            f("plain", FieldType::Bool),
            f("color", FieldType::Enum(COLOR_SCHEME)),
        ],
        doc_lines: DOC_LIST_THEMES,
    },
    Action {
        name: "list-colors",
        options: &[f("plain", FieldType::Bool)],
        doc_lines: DOC_LIST_COLORS,
    },
    Action {
        name: "list-actions",
        options: &[f("docs", FieldType::Bool)],
        doc_lines: DOC_LIST_ACTIONS,
    },
    Action {
        name: "ssh",
        options: &[
            f("_arena", FieldType::Optional),
            f("forward-env", FieldType::Bool),
            f("terminfo", FieldType::Bool),
            f("cache", FieldType::Bool),
            f("ssh", FieldType::Other),
            f("verbose", FieldType::Bool),
            f("_ssh_args", FieldType::Other),
            f("_diagnostics", FieldType::Other),
        ],
        doc_lines: DOC_SSH,
    },
    Action {
        name: "ssh-cache",
        options: &[
            f("clear", FieldType::Bool),
            f("add", FieldType::Optional),
            f("remove", FieldType::Optional),
            f("prune", FieldType::Optional),
        ],
        doc_lines: DOC_SSH_CACHE,
    },
    Action {
        name: "edit-config",
        options: &[],
        doc_lines: DOC_EDIT_CONFIG,
    },
    Action {
        name: "show-config",
        options: &[
            f("default", FieldType::Bool),
            f("changes-only", FieldType::Bool),
            f("docs", FieldType::Bool),
            f("no-pager", FieldType::Bool),
        ],
        doc_lines: DOC_SHOW_CONFIG,
    },
    Action {
        name: "explain-config",
        options: &[
            f("option", FieldType::Optional),
            f("keybind", FieldType::Optional),
        ],
        doc_lines: DOC_EXPLAIN_CONFIG,
    },
    Action {
        name: "validate-config",
        options: &[f("config-file", FieldType::Optional)],
        doc_lines: DOC_VALIDATE_CONFIG,
    },
    Action {
        name: "show-face",
        options: &[
            f("_arena", FieldType::Optional),
            f("cp", FieldType::Optional),
            f("string", FieldType::Optional),
            f("style", FieldType::Enum(FONT_STYLE)),
            f("presentation", FieldType::OptionalEnum(FONT_PRESENTATION)),
            f("_diagnostics", FieldType::Other),
        ],
        doc_lines: DOC_SHOW_FACE,
    },
    Action {
        name: "crash-report",
        options: &[],
        doc_lines: DOC_CRASH_REPORT,
    },
    Action {
        name: "boo",
        options: &[],
        doc_lines: DOC_BOO,
    },
    Action {
        name: "new-window",
        options: &[
            f("_io", FieldType::Other),
            f("_arena", FieldType::Optional),
            f("class", FieldType::Optional),
            f("_working_directory_seen", FieldType::Bool),
            f("_arguments", FieldType::Other),
            f("_diagnostics", FieldType::Other),
        ],
        doc_lines: DOC_NEW_WINDOW,
    },
    Action {
        name: "new-tab",
        options: &[
            f("_io", FieldType::Other),
            f("_arena", FieldType::Optional),
            f("class", FieldType::Optional),
            f("surface-id", FieldType::Optional),
            f("_working_directory_seen", FieldType::Bool),
            f("_arguments", FieldType::Other),
            f("_diagnostics", FieldType::Other),
        ],
        doc_lines: DOC_NEW_TAB,
    },
    Action {
        name: "toggle-quick-terminal",
        options: &[f("class", FieldType::Optional)],
        doc_lines: DOC_TOGGLE_QUICK_TERMINAL,
    },
];

/// `version`
const DOC_VERSION: &[&str] = &[
    " The `version` command is used to display information about Ghostty. Recognized as",
    " either `+version` or `--version`.",
];

/// `help`
const DOC_HELP: &[&str] = &[
    " The `help` command shows general help about Ghostty. Recognized as either",
    " `-h`, `--help`, or like other actions `+help`.",
    "",
    " You can also specify `--help` or `-h` along with any action such as",
    " `+list-themes` to see help for a specific action.",
];

/// `list-fonts`
const DOC_LIST_FONTS: &[&str] = &[
    " The `list-fonts` command is used to list all the available fonts for",
    " Ghostty. This uses the exact same font discovery mechanism Ghostty uses to",
    " find fonts to use.",
    "",
    " When executed with no arguments, this will list all available fonts, sorted",
    " by family name, then font name. If a family name is given with `--family`,",
    " the sorting will be disabled and the results instead will be shown in the",
    " same priority order Ghostty would use to pick a font.",
    "",
    " Flags:",
    "",
    "   * `--bold`: Filter results to specific bold styles. It is not guaranteed",
    "     that only those styles are returned. They are only prioritized.",
    "",
    "   * `--italic`: Filter results to specific italic styles. It is not guaranteed",
    "     that only those styles are returned. They are only prioritized.",
    "",
    "   * `--style`: Filter results based on the style string advertised by a font.",
    "     It is not guaranteed that only those styles are returned. They are only",
    "     prioritized.",
    "",
    "   * `--family`: Filter results to a specific font family. The family handling",
    "     is identical to the `font-family` set of Ghostty configuration values, so",
    "     this can be used to debug why your desired font may not be loading.",
];

/// `list-keybinds`
const DOC_LIST_KEYBINDS: &[&str] = &[
    " The `list-keybinds` command is used to list all the available keybinds for",
    " Ghostty.",
    "",
    " When executed without any arguments this will list the current keybinds",
    " loaded by the config file. If no config file is found or there aren't any",
    " changes to the keybinds it will print out the default ones configured for",
    " Ghostty",
    "",
    " Flags:",
    "",
    "   * `--default`: will print out all the default keybinds",
    "",
    "   * `--docs`: currently does nothing, intended to print out documentation",
    "     about the action associated with the keybinds",
    "",
    "   * `--plain`: will disable formatting and make the output more",
    "     friendly for Unix tooling. This is default when not printing to a tty.",
];

/// `list-themes`
const DOC_LIST_THEMES: &[&str] = &[
    " The `list-themes` command is used to preview or list all the available",
    " themes for Ghostty.",
    "",
    " If this command is run from a TTY, a TUI preview of the themes will be",
    " shown. While in the preview, `F1` will bring up a help screen and `ESC` will",
    " exit the preview. Other keys that can be used to navigate the preview are",
    " listed in the help screen.",
    "",
    " If this command is not run from a TTY, or the output is piped to another",
    " command, a plain list of theme names will be printed to the screen. A plain",
    " list can be forced using the `--plain` CLI flag.",
    "",
    " Two different directories will be searched for themes.",
    "",
    " The first directory is the `themes` subdirectory of your Ghostty",
    " configuration directory. This is `$XDG_CONFIG_HOME/ghostty/themes` or",
    " `~/.config/ghostty/themes`.",
    "",
    " The second directory is the `themes` subdirectory of the Ghostty resources",
    " directory. Ghostty ships with a multitude of themes that will be installed",
    " into this directory. On macOS, this directory is the",
    " `Ghostty.app/Contents/Resources/ghostty/themes`. On Linux, this directory",
    " is the `share/ghostty/themes` (wherever you installed the Ghostty \"share\"",
    " directory). If you're running Ghostty from the source, this is the",
    " `zig-out/share/ghostty/themes` directory.",
    "",
    " You can also set the `GHOSTTY_RESOURCES_DIR` environment variable to point",
    " to the resources directory.",
    "",
    " Flags:",
    "",
    "   * `--path`: Show the full path to the theme.",
    "",
    "   * `--plain`: Force a plain listing of themes.",
    "",
    "   * `--color`: Specify the color scheme of the themes included in the list.",
    "                This can be `dark`, `light`, or `all`. The default is `all`.",
];

/// `list-colors`
const DOC_LIST_COLORS: &[&str] = &[
    " The `list-colors` command is used to list all the named RGB colors in",
    " Ghostty.",
    "",
    " Flags:",
    "",
    "   * `--plain`: will disable formatting and make the output more",
    "     friendly for Unix tooling. This is default when not printing to a tty.",
];

/// `list-actions`
const DOC_LIST_ACTIONS: &[&str] = &[
    " The `list-actions` command is used to list all the available keybind",
    " actions for Ghostty. These are distinct from the CLI Actions which can",
    " be listed via `+help`",
    "",
    " Flags:",
    "",
    "   * `--docs`: will print out the documentation for each action.",
];

/// `ssh`
const DOC_SSH: &[&str] = &[
    " Wrap `ssh` to automatically configure Ghostty terminal integration on",
    " remote hosts.",
    "",
    " Any arguments that aren't recognized as `+ssh` flags are passed to",
    " the real `ssh` binary unchanged. You can use `--` as an explicit",
    " disambiguator if needed, though it's almost never required: `ssh`",
    " has no long flags, and `+ssh` defines no short flags, so there's",
    " nothing to collide.",
    "",
    " This is typically called via Ghostty's shell integration. When",
    " `shell-integration-features` includes `ssh-env` or `ssh-terminfo`,",
    " each shell defines an `ssh` function that runs:",
    "",
    "     ghostty +ssh <flags> -- \"$@\"",
    "",
    " You can also run `ghostty +ssh` directly, or alias it yourself (e.g.",
    " `alias ssh='ghostty +ssh --'`) if you prefer not to use the shell",
    " integration.",
    "",
    " Before it starts `ssh`, `+ssh` can do two pieces of setup. Both are",
    " on by default, and each has its own flag:",
    "",
    "   1. **Forward Ghostty's environment** (`--forward-env`). Asks `ssh` to",
    "      send `COLORTERM`, `TERM_PROGRAM`, and `TERM_PROGRAM_VERSION` to the",
    "      remote host using `SendEnv`. Programs on the remote host can use",
    "      these to tell that they're running inside Ghostty. The remote",
    "      server only accepts them if its `sshd_config` allows them:",
    "",
    "          AcceptEnv COLORTERM TERM_PROGRAM TERM_PROGRAM_VERSION",
    "",
    "   2. **Install Ghostty's terminfo** (`--terminfo`). The first time you",
    "      connect to a host, `+ssh` copies Ghostty's terminfo entry to it and",
    "      compiles it with `tic`. This teaches programs on the remote host",
    "      how to use the `xterm-ghostty` terminal type. Successful installs",
    "      are cached (see `ghostty +ssh-cache`), so later connections to the",
    "      same host skip this step.",
    "",
    " When either flag is enabled, `+ssh` also sets `TERM` for the remote",
    " session:",
    "",
    "   * `xterm-ghostty` if the terminfo entry was just installed, or if the",
    "     host is already in the cache.",
    "",
    "   * `xterm-256color` in every other case. This includes",
    "     `--terminfo=false` and installs that fail, for example because the",
    "     remote host has no `tic`. A failed install prints a warning, and the",
    "     connection continues. Almost every system knows `xterm-256color`,",
    "     so it's a safe fallback.",
    "",
    " `+ssh` sets `TERM` in the environment of the `ssh` process instead of",
    " passing `-o SetEnv=TERM=...`. `ssh` always sends `TERM` when it opens a",
    " remote terminal, so the server doesn't need to allow it with",
    " `AcceptEnv`. This also keeps any `SetEnv` lines in your `~/.ssh/config`",
    " working. `ssh` only uses the first `SetEnv` it finds, so a `SetEnv` on",
    " the command line would quietly replace yours.",
    "",
    " When both flags are disabled, `+ssh` leaves `TERM` alone and runs",
    " `ssh` with your environment unchanged.",
    "",
    " Flags:",
    "",
    "   * `--forward-env=<bool>`: Send `COLORTERM`, `TERM_PROGRAM`, and",
    "     `TERM_PROGRAM_VERSION` to the remote host. Default: `true`.",
    "",
    "   * `--terminfo=<bool>`: Install Ghostty's terminfo on the first",
    "     connection to each host, and use `xterm-ghostty` as `TERM` when it's",
    "     available. Default: `true`.",
    "",
    "   * `--cache=<bool>`: Use the terminfo install cache. Default: `true`.",
    "     When `false`, both the cache read (skip-if-installed) and the",
    "     cache write (record-on-success) are bypassed, and every",
    "     connection performs the install. To one-shot reinstall a single",
    "     host while keeping the cache in use, prefer `ghostty +ssh-cache",
    "     --remove=<host>` followed by a normal connection.",
    "",
    "   * `--ssh=<path>`: Path to the `ssh` binary to execute. Default: the",
    "     first `ssh` found on `PATH`.",
    "",
    "   * `--verbose`: Print +ssh status lines to stderr, and surface",
    "     remote stderr during the terminfo install.",
    "",
    " Examples:",
    "",
    "     # Basic invocation using defaults:",
    "     ghostty +ssh user@example.com",
    "",
    "     # Forward Ghostty env vars but skip the terminfo install.",
    "     # The remote session uses TERM=xterm-256color:",
    "     ghostty +ssh --terminfo=false user@example.com",
    "",
    "     # Install terminfo but don't send any extra env vars:",
    "     ghostty +ssh --forward-env=false user@example.com",
    "",
    "     # SetEnv lines in your ssh config still apply. With this entry in",
    "     # ~/.ssh/config:",
    "     #",
    "     #   Host dev",
    "     #       SetEnv EDITOR=vim",
    "     #",
    "     # ssh still sends EDITOR=vim, along with the TERM chosen by +ssh:",
    "     ghostty +ssh dev",
    "",
    "     # `ssh` flags (short-form `-p`, etc.) pass through unchanged:",
    "     ghostty +ssh -p 2222 -i ~/.ssh/id_ed25519 user@example.com",
    "",
    "     # Use `--` explicitly if your ssh args might collide with our flags:",
    "     ghostty +ssh -- --some-rare-ssh-arg user@example.com",
    "",
    " Pass `--verbose` to see what `+ssh` is doing. For cache inspection",
    " and management, see `ghostty +ssh-cache`.",
    "",
    " Available since: 1.4.0",
];

/// `ssh-cache`
const DOC_SSH_CACHE: &[&str] = &[
    " Manage the SSH terminfo cache for automatic remote host setup.",
    "",
    " The `+ssh` action installs Ghostty's terminfo on remote hosts and records",
    " each success in this cache so it doesn't re-upload on later connections.",
    " (`+ssh` runs automatically from the shell integration when",
    " `shell-integration-features` includes `ssh-terminfo`.) This command",
    " inspects and maintains that cache.",
    "",
    " The cache stores destinations (a hostname or user@hostname) along with",
    " timestamps. A destination on a port other than 22 carries it as a",
    " `:port` suffix (`user@hostname:2222`, or `user@[2001:db8::1]:2222` for an",
    " IPv6 address), since each port can be a different machine.",
    "",
    " A positional destination queries the cache: `user@hostname` shows that",
    " exact entry, while a bare `hostname` shows every cached entry for that",
    " host regardless of user and port, and a bare `hostname:port` every entry",
    " for that port. With no destination and no action, the entire cache is",
    " listed. A query that matches nothing exits 1.",
    "",
    " At most one action (`--clear`, `--add`, `--remove`, or `--prune`) may be",
    " specified, and not together with a positional destination; combining them",
    " is an error.",
    "",
    " `--prune` takes a duration with unit suffixes (`s`, `m`, `h`, `d`, `w`,",
    " `y`) and removes every entry older than it, e.g. `--prune=30d`,",
    " `--prune=6h`, `--prune=1y`.",
    "",
    " Examples:",
    "",
    "     ghostty +ssh-cache                           # List all cached destinations",
    "     ghostty +ssh-cache user@example.com          # Show that destination",
    "     ghostty +ssh-cache example.com               # Show all users on that host",
    "     ghostty +ssh-cache user@example.com:2222     # Show that destination on port 2222",
    "     ghostty +ssh-cache --add=user@example.com    # Manually add a destination",
    "     ghostty +ssh-cache --remove=user@example.com # Remove a destination",
    "     ghostty +ssh-cache --prune=30d               # Remove entries older than 30 days",
    "     ghostty +ssh-cache --clear                   # Clear entire cache",
];

/// `edit-config`
const DOC_EDIT_CONFIG: &[&str] = &[
    " The `edit-config` command opens the Ghostty configuration file in the",
    " editor specified by the `$VISUAL` or `$EDITOR` environment variables.",
    "",
    " IMPORTANT: This command will not reload the configuration after",
    " editing. You will need to manually reload the configuration using the",
    " application menu, configured keybind, or by restarting Ghostty. We",
    " plan to auto-reload in the future, but Ghostty isn't capable of",
    " this yet.",
    "",
    " The filepath opened is the default user-specific configuration",
    " file, which is typically located at `$XDG_CONFIG_HOME/ghostty/config.ghostty`.",
    " On macOS, this may also be located at",
    " `~/Library/Application Support/com.mitchellh.ghostty/config.ghostty`.",
    " On macOS, whichever path exists and is non-empty will be prioritized,",
    " prioritizing the Application Support directory if neither are",
    " non-empty.",
    "",
    " This command prefers the `$VISUAL` environment variable over `$EDITOR`,",
    " if both are set. If neither are set, it will print an error",
    " and exit.",
];

/// `show-config`
const DOC_SHOW_CONFIG: &[&str] = &[
    " The `show-config` command shows the current configuration in a valid Ghostty",
    " configuration file format.",
    "",
    " When executed without any arguments this will output the current",
    " configuration that is different from the default configuration. If you're",
    " using the default configuration this will output nothing.",
    "",
    " If you are a new user and want to see all available options with",
    " documentation, run `ghostty +show-config --default --docs`.",
    "",
    " The output is not in any specific order, but the order should be consistent",
    " between runs. The output is not guaranteed to be exactly match the input",
    " configuration files, but it will result in the same behavior. Comments,",
    " whitespace, and other formatting is not preserved from user configuration",
    " files.",
    "",
    " Flags:",
    "",
    "   * `--default`: Show the default configuration instead of loading",
    "     the user configuration.",
    "",
    "   * `--changes-only`: Only show the options that have been changed",
    "     from the default. This has no effect if `--default` is specified.",
    "",
    "   * `--docs`: Print the documentation above each option as a comment,",
    "     This is very noisy but is very useful to learn about available",
    "     options, especially paired with `--default`.",
    "",
    "   * `--no-pager`: Disable automatic paging of output.",
];

/// `explain-config`
const DOC_EXPLAIN_CONFIG: &[&str] = &[
    " The `explain-config` command prints the documentation for a single",
    " Ghostty configuration option or keybind action.",
    "",
    " Examples:",
    "",
    "   ghostty +explain-config font-size",
    "   ghostty +explain-config copy_to_clipboard",
    "   ghostty +explain-config --option=font-size",
    "   ghostty +explain-config --keybind=copy_to_clipboard",
    "",
    " Flags:",
    "",
    "   * `--option`: The name of the configuration option to explain.",
    "   * `--keybind`: The name of the keybind action to explain.",
    "   * `--no-pager`: Disable automatic paging of output.",
];

/// `validate-config`
const DOC_VALIDATE_CONFIG: &[&str] = &[
    " The `validate-config` command is used to validate a Ghostty config file.",
    "",
    " When executed without any arguments, this will load the config from the default",
    " location.",
    "",
    " Flags:",
    "",
    "   * `--config-file`: can be passed to validate a specific target config file in",
    "     a non-default location",
];

/// `show-face`
const DOC_SHOW_FACE: &[&str] = &[
    " The `show-face` command shows what font face Ghostty will use to render a",
    " specific codepoint. Note that this command does not take into consideration",
    " grapheme clustering or any other Unicode features that might modify the",
    " presentation of a codepoint, so this may show a different font face than",
    " Ghostty uses to render a codepoint in a terminal session.",
    "",
    " Flags:",
    "",
    "   * `--cp`: Find the face for a single codepoint. The codepoint may be specified",
    "     in decimal (`--cp=65`), hexadecimal (`--cp=0x41`), octal (`--cp=0o101`), or",
    "     binary (`--cp=0b1000001`).",
    "",
    "   * `--string`: Find the face for all of the codepoints in a string. The",
    "     string must be a valid UTF-8 sequence.",
    "",
    "   * `--style`: Search for a specific style. Valid options are `regular`, `bold`,",
    "     `italic`, and `bold_italic`.",
    "",
    "   * `--presentation`: If set, force searching for a specific presentation",
    "     style. Valid options are `text` and `emoji`. If unset, the presentation",
    "     style of a codepoint will be inferred from the Unicode standard.",
];

/// `crash-report`
const DOC_CRASH_REPORT: &[&str] = &[
    " The `crash-report` command is used to inspect and send crash reports.",
    "",
    " When executed without any arguments, this will list existing crash reports.",
    "",
    " This command currently only supports listing crash reports. Viewing",
    " and sending crash reports is unimplemented and will be added in the future.",
];

/// `boo`
const DOC_BOO: &[&str] = &[
    " The `boo` command is used to display the animation from the Ghostty website in the terminal",
];

/// `new-window`
const DOC_NEW_WINDOW: &[&str] = &[
    " The `new-window` will use native platform IPC to open up a new window in a",
    " running instance of Ghostty.",
    "",
    " If the `--class` flag is not set, the `new-window` command will try and",
    " connect to a running instance of Ghostty based on what optimizations the",
    " Ghostty CLI was compiled with. Otherwise the `new-window` command will try",
    " and contact a running Ghostty instance that was configured with the same",
    " `class` as was given on the command line.",
    "",
    " All of the arguments after the `+new-window` argument (except for the",
    " `--class` flag) will be sent to the remote Ghostty instance and will be",
    " parsed as command line flags. These flags will override certain settings",
    " when creating the first surface in the new window. Currently, only",
    " `--working-directory`, `--command`, `--shell-integration=<mode>`, and",
    " `--title` are supported. `-e` will also work as an alias for `--command`, except that if",
    " `-e` is found on the command line all following arguments will become part",
    " of the command and no more arguments will be parsed for configuration",
    " settings.",
    "",
    " If `--working-directory` is found on the command line and is a relative",
    " path (i.e. doesn't start with `/`) it will be resolved to an absolute path",
    " relative to the current working directory that the `ghostty +new-window`",
    " command is run from. `~/` prefixes will also be expanded to the user's home",
    " directory.",
    "",
    " If `--working-directory` is _not_ found on the command line, the working",
    " directory that `ghostty +new-window` is run from will be passed to Ghostty.",
    "",
    " GTK uses an application ID to identify instances of applications. If Ghostty",
    " is compiled with release optimizations, the default application ID will be",
    " `com.mitchellh.ghostty`. If Ghostty is compiled with debug optimizations,",
    " the default application ID will be `com.mitchellh.ghostty-debug`.  The",
    " `class` configuration entry can be used to set up a custom application",
    " ID. The class name must follow the requirements defined [in the GTK",
    " documentation](https://docs.gtk.org/gio/type_func.Application.id_is_valid.html)",
    " or it will be ignored and Ghostty will use the default as defined above.",
    "",
    " On GTK, D-Bus activation must be properly configured. Ghostty does not need",
    " to be running for this to open a new window, making it suitable for binding",
    " to keys in your window manager (if other methods for configuring global",
    " shortcuts are unavailable). D-Bus will handle launching a new instance",
    " of Ghostty if it is not already running. See the Ghostty website for",
    " information on properly configuring D-Bus activation.",
    "",
    " Only supported on GTK.",
    "",
    " Flags:",
    "",
    "   * `--class=<class>`: If set, open up a new window in a custom instance of",
    "     Ghostty. The class must be a valid GTK application ID.",
    "",
    "   * `--command`: The command to be executed in the first surface of the new window.",
    "",
    "   * `--shell-integration=<mode>`: Whether to enable shell integration and",
    "     which shell to use. See the main configuration documentation for",
    "     supported values.",
    "",
    "   * `--working-directory=<directory>`: The working directory to pass to Ghostty.",
    "",
    "   * `--title`: A title that will override the title of the first surface in",
    "     the new window. The title override may be edited or removed later.",
    "",
    "   * `-e`: Any arguments after this will be interpreted as a command to",
    "     execute inside the first surface of the new window instead of the",
    "     default command.",
    "",
    " Available since: 1.2.0",
];

/// `new-tab`
const DOC_NEW_TAB: &[&str] = &[
    " The `new-tab` will use native platform IPC to open up a new tab in a running",
    " instance of Ghostty.",
    "",
    " If the `--class` flag is not set, the `new-tab` command will try and connect",
    " to a running instance of Ghostty based on what optimizations the Ghostty",
    " CLI was compiled with. Otherwise the `new-tab` command will try and contact",
    " a running Ghostty instance that was configured with the same `class` as was",
    " given on the command line.",
    "",
    " All of the arguments after the `+new-tab` argument (except for the `--class`",
    " and `--surface-id` flag) will be sent to the remote Ghostty instance and",
    " will be parsed as command line flags. These flags will override certain",
    " settings when creating the first surface in the new tab. Currently, only",
    " `--working-directory`, `--command`, `--shell-integration=<mode>`, and",
    " `--title` are supported. `-e` will also work as an alias for `--command`, except that if",
    " `-e` is found on the command line all following arguments will become part",
    " of the command and no more arguments will be parsed for configuration",
    " settings.",
    "",
    " If `--working-directory` is found on the command line and is a relative",
    " path (i.e. doesn't start with `/`) it will be resolved to an absolute",
    " path relative to the current working directory that the `ghostty +new-tab`",
    " command is run from. `~/` prefixes will also be expanded to the user's home",
    " directory.",
    "",
    " If `--working-directory` is _not_ found on the command line, the working",
    " directory that `ghostty +new-tab` is run from will be passed to Ghostty.",
    "",
    " GTK uses an application ID to identify instances of applications. If Ghostty",
    " is compiled with release optimizations, the default application ID will be",
    " `com.mitchellh.ghostty`. If Ghostty is compiled with debug optimizations,",
    " the default application ID will be `com.mitchellh.ghostty-debug`.  The",
    " `class` configuration entry can be used to set up a custom application",
    " ID. The class name must follow the requirements defined [in the GTK",
    " documentation](https://docs.gtk.org/gio/type_func.Application.id_is_valid.html)",
    " or it will be ignored and Ghostty will use the default as defined above.",
    "",
    " On GTK, D-Bus activation must be properly configured. Ghostty does not need",
    " to be running for this to open a new tab, making it suitable for binding",
    " to keys in your window manager (if other methods for configuring global",
    " shortcuts are unavailable). D-Bus will handle launching a new instance",
    " of Ghostty if it is not already running. See the Ghostty website for",
    " information on properly configuring D-Bus activation.",
    "",
    " Only supported on GTK.",
    "",
    " Flags:",
    "",
    "   * `--class=<class>`: If set, open up a new window in a custom instance of",
    "     Ghostty. The class must be a valid GTK application ID.",
    "",
    "   * `--surface-id=<surface id>`: If set, specifies a Ghostty surface ID.",
    "     This is used to identify the window that the new tab will be added to.",
    "     If the surface ID is not specified or is `0` the currently focused",
    "     surface will be used.",
    "",
    "   * `--command`: The command to be executed in the first surface of the new tab.",
    "",
    "   * `--shell-integration=<mode>`: Whether to enable shell integration and",
    "     which shell to use. See the main configuration documentation for",
    "     supported values.",
    "",
    "   * `--working-directory=<directory>`: The working directory to pass to Ghostty.",
    "",
    "   * `--title`: A title that will override the title of the first surface in",
    "     the new tab. The title override may be edited or removed later.",
    "",
    "   * `-e`: Any arguments after this will be interpreted as a command to",
    "     execute inside the first surface of the new tab instead of the",
    "     default command.",
    "",
    " Available since: 1.4.0",
];

/// `toggle-quick-terminal`
const DOC_TOGGLE_QUICK_TERMINAL: &[&str] = &[
    " The `+toggle-quick-terminal` command will use native platform IPC to toggle",
    " the quick terminal in a running instance of Ghostty.",
    "",
    " If the `--class` flag is not set, the command will try and connect to the",
    " default running Ghostty instance. Otherwise it will contact a Ghostty",
    " instance configured with the given `class`.",
    "",
    " On GTK, D-Bus activation must be properly configured. Ghostty does not need",
    " to be running, as D-Bus will handle launching a new instance if it is not",
    " already running.",
    "",
    " Only supported on GTK.",
    "",
    " Flags:",
    "",
    "   * `--class=<class>`: If set, connect to a custom instance of Ghostty.",
    "     The class must be a valid GTK application ID.",
    "",
    " Available since: 1.4.0",
];


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::key::Key;

    #[test]
    fn config_field_types_match_config() {
        let types = config_field_types();
        let fields = <Config as Fields>::FIELDS;
        assert_eq!(types.len(), fields.len());
        for (t, f) in types.iter().zip(fields) {
            assert_eq!(t.0, f.name);
        }
        assert_eq!(<Key as CliEnum>::FIELD_NAMES.len(), fields.len());
    }

    fn ty(name: &str) -> FieldType {
        config_fields().iter().find(|f| f.name == name).unwrap().ty
    }

    #[test]
    fn config_field_type_kinds() {
        assert_eq!(FieldType::Other, ty("font-family"));
        assert_eq!(FieldType::Optional, ty("language"));
        assert_eq!(FieldType::RepeatablePath, ty("config-file"));
        assert_eq!(FieldType::Bool, ty("focus-follows-mouse"));
        assert_eq!(FieldType::Optional, ty("command"));
        assert!(matches!(
            ty("shell-integration-features"),
            FieldType::PackedStruct(_)
        ));
        assert!(matches!(ty("shell-integration"), FieldType::Enum(_)));
        assert!(matches!(ty("auto-update"), FieldType::OptionalEnum(_)));
    }

    #[test]
    fn action_help() {
        assert_eq!(
            "The `version` command is used to display information about Ghostty. Recognized as\n\
             either `+version` or `--version`.",
            ACTIONS[0].help()
        );
        assert_eq!("cli/list_fonts.zig", ACTIONS[2].file());
    }
}
