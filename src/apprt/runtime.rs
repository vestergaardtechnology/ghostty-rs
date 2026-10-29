//! Port of `src/apprt/runtime.zig`.
//!
//! STATUS: ported

/// Runtime is the runtime to use for Ghostty. All runtimes do not provide
/// equivalent feature sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Runtime {
    /// Will not produce an executable at all when `zig build` is called.
    /// This is only useful if you're only interested in the lib only (macOS).
    None,

    /// GTK4. Rich windowed application. This uses a full GObject-based
    /// approach to building the application.
    Gtk,
}

impl Runtime {
    /// The tag name as spelled in Zig (used for build options).
    pub const fn as_str(self) -> &'static str {
        match self {
            Runtime::None => "none",
            Runtime::Gtk => "gtk",
        }
    }

    /// Zig: `default(target: std.Target)`. The Rust port takes the target
    /// OS name as spelled by `cfg(target_os = "...")`.
    #[allow(clippy::should_implement_trait)]
    pub fn default(target_os: &str) -> Runtime {
        match target_os {
            // The Linux and FreeBSD default is GTK because it is a full
            // featured application.
            "linux" | "freebsd" => Runtime::Gtk,
            // Otherwise, we do NONE so we don't create an exe and we create
            // libghostty. On macOS, Xcode is used to build the app that links
            // to libghostty.
            _ => Runtime::None,
        }
    }
}

impl std::str::FromStr for Runtime {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Runtime::None),
            "gtk" => Ok(Runtime::Gtk),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_runtime() {
        assert_eq!(Runtime::Gtk, Runtime::default("linux"));
        assert_eq!(Runtime::Gtk, Runtime::default("freebsd"));
        assert_eq!(Runtime::None, Runtime::default("macos"));
        assert_eq!(Ok(Runtime::Gtk), "gtk".parse());
        assert_eq!("none", Runtime::None.as_str());
    }
}
