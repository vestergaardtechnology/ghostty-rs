//! Port of `src/input/Link.zig`.
//!
//! STATUS: ported
//!
//! A link is a clickable element that can be used to trigger some action.
//! A link is NOT just a URL that opens in a browser. A link is any generic
//! regular expression match over terminal text that can trigger various
//! action types.

use std::borrow::Cow;

use super::key::Mods;

/// A clickable link definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The regular expression that will be used to match the link. Ownership
    /// of this memory is up to the caller. The link will never free this memory.
    ///
    /// (Rust: a `Cow` so static default links don't allocate while
    /// [`Link::clone_link`] produces an owned deep copy.)
    pub regex: Cow<'static, [u8]>,

    /// The action that will be triggered when the link is clicked.
    pub action: Action,

    /// The situations in which the link will be highlighted. A link is only
    /// clickable by the mouse when it is highlighted, so this also controls
    /// when the link is clickable.
    pub highlight: Highlight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Open the full matched value using the default open program.
    /// For example, on macOS this is "open" and on Linux this is "xdg-open".
    Open,

    /// Open the OSC8 hyperlink under the mouse position. _-prefixed means
    /// this can't be user-specified, it's only used internally.
    OpenOsc8,
}

impl Action {
    /// The Zig tag name (`@tagName`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Action::Open => "open",
            Action::OpenOsc8 => "_open_osc8",
        }
    }

    /// Exact tag name match (`std.meta.stringToEnum`).
    pub fn parse(s: &[u8]) -> Option<Action> {
        match s {
            b"open" => Some(Action::Open),
            b"_open_osc8" => Some(Action::OpenOsc8),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Highlight {
    /// Always highlight the link.
    Always,

    /// Only highlight the link when the mouse is hovering over it.
    Hover,

    /// Highlight anytime the given mods are pressed, either when
    /// hovering or always. For always, all links will be highlighted
    /// when the mods are pressed regardless of if the mouse is hovering
    /// over them.
    ///
    /// Note that if "shift" is specified here, this will NEVER match in
    /// TUI programs that capture mouse events. "Shift" with mouse capture
    /// escapes the mouse capture but strips the "shift" so it can't be
    /// detected.
    AlwaysMods(Mods),
    HoverMods(Mods),
}

impl Highlight {
    /// The Zig tag name (`@tagName`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Highlight::Always => "always",
            Highlight::Hover => "hover",
            Highlight::AlwaysMods(_) => "always_mods",
            Highlight::HoverMods(_) => "hover_mods",
        }
    }
}

impl Link {
    /// Returns a new oni.Regex that can be used to match the link.
    pub fn oni_regex(&self) -> Result<oni::Regex, oni::Error> {
        oni::Regex::init(
            &self.regex,
            oni::OPTION_NONE,
            oni::Encoding::utf8(),
            oni::Syntax::default(),
        )
    }

    /// Deep clone the link.
    pub fn clone_link(&self) -> Link {
        Link {
            regex: Cow::Owned(self.regex.to_vec()),
            action: self.action,
            highlight: self.highlight,
        }
    }

    /// Check if two links are equal.
    pub fn equal(&self, other: &Link) -> bool {
        self.action == other.action
            && self.highlight == other.highlight
            && self.regex[..] == other.regex[..]
    }
}

/// Minimal stand-in for the oniguruma bindings.
///
/// TODO(port-dedupe): replace with the `pkg/oniguruma` port
/// (`oni.Regex`, `oni.Encoding.utf8`, `oni.Syntax.default`, `oni.Error`).
pub mod oni {
    use std::ffi::{c_int, c_uint, c_void};
    use std::ptr;

    #[repr(C)]
    pub struct OnigEncodingType {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct OnigSyntaxType {
        _private: [u8; 0],
    }

    type OnigRegex = *mut c_void;

    #[link(name = "onig")]
    extern "C" {
        static mut OnigEncodingUTF8: OnigEncodingType;
        static mut OnigSyntaxOniguruma: OnigSyntaxType;

        fn onig_new(
            reg: *mut OnigRegex,
            pattern: *const u8,
            pattern_end: *const u8,
            option: c_uint,
            enc: *mut OnigEncodingType,
            syntax: *mut OnigSyntaxType,
            einfo: *mut c_void,
        ) -> c_int;
        fn onig_free(reg: OnigRegex);
        fn onig_initialize(encodings: *mut *mut OnigEncodingType, n: c_int) -> c_int;
    }

    /// `ONIG_OPTION_NONE`
    pub const OPTION_NONE: c_uint = 0;

    /// An oniguruma error code (always negative).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Error(pub c_int);

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "oniguruma error {}", self.0)
        }
    }

    impl std::error::Error for Error {}

    pub struct Encoding;

    impl Encoding {
        /// `ONIG_ENCODING_UTF8`
        pub fn utf8() -> *mut OnigEncodingType {
            // Taking the address of an extern static is safe; never read here.
            ptr::addr_of_mut!(OnigEncodingUTF8)
        }
    }

    pub struct Syntax;

    impl Syntax {
        /// `ONIG_SYNTAX_ONIGURUMA`
        pub fn default() -> *mut OnigSyntaxType {
            // Taking the address of an extern static is safe; never read here.
            ptr::addr_of_mut!(OnigSyntaxOniguruma)
        }
    }

    /// Initialize oniguruma with the given encodings (`oni.init`).
    pub fn init(encs: &[*mut OnigEncodingType]) -> Result<(), Error> {
        let mut encs = encs.to_vec();
        // SAFETY: the pointers are valid encoding statics.
        let rc = unsafe { onig_initialize(encs.as_mut_ptr(), encs.len() as c_int) };
        if rc < 0 {
            return Err(Error(rc));
        }
        Ok(())
    }

    /// A compiled regular expression; freed on drop.
    pub struct Regex {
        value: OnigRegex,
    }

    impl Regex {
        pub fn init(
            pattern: &[u8],
            options: c_uint,
            enc: *mut OnigEncodingType,
            syntax: *mut OnigSyntaxType,
        ) -> Result<Regex, Error> {
            let mut value: OnigRegex = ptr::null_mut();
            // SAFETY: pattern range is valid for the duration of the call;
            // enc/syntax are valid oniguruma statics; einfo may be null.
            let rc = unsafe {
                onig_new(
                    &mut value,
                    pattern.as_ptr(),
                    pattern.as_ptr().add(pattern.len()),
                    options,
                    enc,
                    syntax,
                    ptr::null_mut(),
                )
            };
            if rc < 0 {
                return Err(Error(rc));
            }
            Ok(Regex { value })
        }
    }

    impl Drop for Regex {
        fn drop(&mut self) {
            // SAFETY: value was created by onig_new and is freed once.
            unsafe { onig_free(self.value) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Port-specific: Link.zig has no tests; exercise the API.
    #[test]
    fn clone_and_equal() {
        let link = Link {
            regex: Cow::Borrowed(b"https?://\\S+"),
            action: Action::Open,
            highlight: Highlight::HoverMods(Mods {
                ctrl: true,
                ..Mods::EMPTY
            }),
        };
        let cloned = link.clone_link();
        assert!(link.equal(&cloned));
        assert!(matches!(cloned.regex, Cow::Owned(_)));

        let other = Link {
            highlight: Highlight::Hover,
            ..link.clone_link()
        };
        assert!(!link.equal(&other));
    }

    #[test]
    fn oni_regex() {
        oni::init(&[oni::Encoding::utf8()]).unwrap();
        let link = Link {
            regex: Cow::Borrowed(b"https?://\\S+"),
            action: Action::Open,
            highlight: Highlight::Always,
        };
        let _re = link.oni_regex().unwrap();

        let bad = Link {
            regex: Cow::Borrowed(b"(unclosed"),
            ..link.clone_link()
        };
        assert!(bad.oni_regex().is_err());
    }
}
