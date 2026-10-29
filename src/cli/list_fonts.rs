//! Port of `src/cli/list_fonts.zig`.
//!
//! STATUS: ported
//!
//! Font discovery (`font.Discover`, `font/discovery.zig`) is not ported on
//! `main` yet; [`discover`] is a Fontconfig stand-in with the same
//! behavior for this command (TODO(port-dedupe)).

use std::collections::HashMap;
use std::io::{self, BufWriter, Write};

use super::args::{self, cli_struct, ArgIterator, Error, ParseHooks, StringZ};
use super::ghostty::{global_args, Action, RunResult};

const LOG: &str = "list_fonts";

cli_struct! {
    #[derive(Debug, Clone)]
    pub struct Options {
        /// The font family to search for. If this is set, then only fonts
        /// matching this family will be listed.
        pub family: Option<StringZ> = None,

        /// The style name to search for.
        pub style: Option<StringZ> = None,

        /// Font styles to search for. If this is set, then only fonts that
        /// match the given styles will be listed.
        pub bold: bool = false,
        pub italic: bool = false,
    }
    with { custom_hooks }
}

impl ParseHooks for Options {
    /// Enables "-h" and "--help" to work.
    fn help(&mut self) -> Option<Result<(), Error>> {
        Action::help_result()
    }
}

/// The help text of this action (Zig `help_strings.Action.@"list-fonts"`).
pub const HELP: &str = include_str!("list_fonts_help.txt");

/// The `list-fonts` command is used to list all the available fonts for
/// Ghostty. See [`HELP`].
pub fn run() -> RunResult {
    let mut iter = args::args_iterator(global_args());
    run_args(&mut iter)
}

fn run_args<I: ArgIterator + ?Sized>(args_iter: &mut I) -> RunResult {
    let mut config = Options::default();
    args::parse(&mut config, args_iter)?;

    // Its possible to build Ghostty without font discovery!
    if !discover::AVAILABLE {
        let mut stderr = io::stderr().lock();
        write!(
            stderr,
            "Ghostty was built without a font discovery mechanism. This is a compile-time
option. Please review how Ghostty was built from source, contact the
maintainer to enable a font discovery mechanism, and try again."
        )?;
        stderr.flush()?;
        return Ok(1);
    }

    let mut stdout = BufWriter::with_capacity(2048, io::stdout().lock());

    // We'll be putting our fonts into a list categorized by family
    // so it is easier to read the output.
    let mut families: Vec<String> = Vec::new();
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    // Look up all available fonts.
    let disco = discover::Discover::init();
    let mut disco_it = disco.discover(&discover::Descriptor {
        family: config.family.as_deref(),
        style: config.style.as_deref(),
        bold: config.bold,
        italic: config.italic,
        monospace: config.family.is_none(),
        ..Default::default()
    })?;
    while let Some(face) = disco_it.next() {
        let family = match face.family_name() {
            Ok(family) => family,
            Err(err) => {
                log::error!(target: LOG, "failed to get font family name: {}", err);
                continue;
            }
        };

        let full_name = match face.name() {
            Ok(name) => name,
            Err(err) => {
                log::error!(target: LOG, "failed to get font name: {}", err);
                continue;
            }
        };

        let list = map.entry(family.clone()).or_insert_with(|| {
            families.push(family);
            Vec::new()
        });
        list.push(full_name);
    }

    // Sort our keys.
    if config.family.is_none() {
        families.sort_unstable_by(|l, r| l.as_bytes().cmp(r.as_bytes()));
    }

    // Output each
    for family in &families {
        let Some(list) = map.get_mut(family) else {
            continue;
        };
        if list.is_empty() {
            continue;
        }
        if config.family.is_none() {
            list.sort_unstable_by(|l, r| l.as_bytes().cmp(r.as_bytes()));
        }

        writeln!(stdout, "{}", family)?;
        for item in list.iter() {
            writeln!(stdout, "  {}", item)?;
        }
        writeln!(stdout)?;
    }

    stdout.flush()?;
    Ok(0)
}

/// Stand-in for `font.Discover` (Fontconfig backend) and the parts of
/// `DeferredFace` used here.
///
/// TODO(port-dedupe): replace with `crate::font::Discover`,
/// `crate::font::discovery::Descriptor` and `crate::font::DeferredFace`
/// once `font/discovery.zig` is ported on `main`.
pub(crate) mod discover {
    use std::ffi::CString;
    use std::fmt;

    use crate::pkg::fontconfig::{self as fc, Property, Value};

    /// Whether a discovery mechanism exists (Zig `font.Discover != void`).
    pub const AVAILABLE: bool = cfg!(not(target_os = "macos"));

    /// The subset of `font.discovery.Descriptor` used by the CLI.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Descriptor<'a> {
        pub family: Option<&'a str>,
        pub style: Option<&'a str>,
        pub codepoint: u32,
        pub bold: bool,
        pub italic: bool,
        pub monospace: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DiscoverError {
        FontConfigFailed,
    }

    impl fmt::Display for DiscoverError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("FontConfigFailed")
        }
    }

    impl std::error::Error for DiscoverError {}

    fn cstring(s: &str) -> CString {
        let bytes = s.as_bytes();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        CString::new(&bytes[..end]).unwrap()
    }

    impl Descriptor<'_> {
        /// `Descriptor.toFcPattern`.
        fn to_fc_pattern(&self) -> fc::Pattern {
            let pat = fc::Pattern::create();
            if let Some(family) = self.family {
                let family = cstring(family);
                pat.add(Property::Family, Value::String(&family), false);
            }
            if let Some(style) = self.style {
                let style = cstring(style);
                pat.add(Property::Style, Value::String(&style), false);
            }
            if self.codepoint > 0 {
                let cs = fc::CharSet::create();
                cs.add_char(self.codepoint);
                pat.add(Property::Charset, Value::CharSet(cs), false);
                // SAFETY: fontconfig copied the charset into the pattern.
                unsafe { cs.destroy() };
            }
            if self.bold {
                pat.add(Property::Weight, Value::Integer(fc::Weight::Bold as i32), false);
            }
            if self.italic {
                pat.add(Property::Slant, Value::Integer(fc::Slant::Italic as i32), false);
            }

            // For fontconfig, we always add monospace in the pattern. Since
            // fontconfig sorts by closeness to the pattern, this doesn't fully
            // exclude non-monospace but helps prefer it.
            pat.add(Property::Spacing, Value::Integer(fc::Spacing::Mono as i32), false);
            pat
        }
    }

    pub struct Discover {
        config: fc::Config,
    }

    impl Discover {
        pub fn init() -> Discover {
            let _ = fc::init();
            Discover {
                config: fc::init_load_config_and_fonts(),
            }
        }

        pub fn discover(&self, desc: &Descriptor<'_>) -> Result<DiscoverIterator, DiscoverError> {
            let pat = desc.to_fc_pattern();
            self.config.substitute_with_pat(pat, fc::MatchKind::Pattern);
            pat.default_substitute();

            let res = self.config.font_sort(pat, false, None);
            if res.result != fc::Result::Match {
                // SAFETY: not used afterwards.
                unsafe {
                    res.fs.destroy();
                    pat.destroy();
                }
                return Err(DiscoverError::FontConfigFailed);
            }

            Ok(DiscoverIterator {
                config: self.config,
                pattern: pat,
                set: res.fs,
                i: 0,
            })
        }
    }

    impl Drop for Discover {
        fn drop(&mut self) {
            // SAFETY: we own this config.
            unsafe { self.config.destroy() };
        }
    }

    pub struct DiscoverIterator {
        config: fc::Config,
        pattern: fc::Pattern,
        set: fc::FontSet,
        i: usize,
    }

    impl DiscoverIterator {
        /// The next face. Faces whose render pattern can't be prepared are
        /// skipped.
        #[allow(clippy::should_implement_trait)]
        pub fn next(&mut self) -> Option<Face> {
            while self.i < self.set.fonts().len() {
                let font = self.set.fonts()[self.i];
                self.i += 1;
                if let Ok(pattern) = self.config.font_render_prepare(self.pattern, font) {
                    return Some(Face { pattern });
                }
            }
            None
        }
    }

    impl Drop for DiscoverIterator {
        fn drop(&mut self) {
            // SAFETY: we own these handles.
            unsafe {
                self.set.destroy();
                self.pattern.destroy();
            }
        }
    }

    /// A discovered face (the Fontconfig part of `DeferredFace`).
    pub struct Face {
        pattern: fc::Pattern,
    }

    impl Face {
        fn string(&self, prop: Property) -> Result<String, fc::Error> {
            match self.pattern.get(prop, 0)? {
                Value::String(s) => Ok(s.to_string_lossy().into_owned()),
                _ => Err(fc::Error::FontconfigTypeMismatch),
            }
        }

        /// `DeferredFace.familyName`.
        pub fn family_name(&self) -> Result<String, fc::Error> {
            self.string(Property::Family)
        }

        /// `DeferredFace.name`.
        pub fn name(&self) -> Result<String, fc::Error> {
            self.string(Property::Fullname)
        }

        /// The path of the font file.
        pub fn path(&self) -> Result<String, fc::Error> {
            self.string(Property::File)
        }

        /// Whether the face has a glyph for `cp` (its charset).
        pub fn has_codepoint(&self, cp: u32) -> bool {
            match self.pattern.get(Property::Charset, 0) {
                Ok(Value::CharSet(cs)) => cs.has_char(cp),
                _ => false,
            }
        }
    }

    impl Drop for Face {
        fn drop(&mut self) {
            // SAFETY: the pattern was created by font_render_prepare.
            unsafe { self.pattern.destroy() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_monospace_fonts() {
        let disco = discover::Discover::init();
        let mut it = disco
            .discover(&discover::Descriptor {
                monospace: true,
                ..Default::default()
            })
            .unwrap();
        // The test environment may have no fonts; names must be readable
        // when there are.
        if let Some(face) = it.next() {
            assert!(face.family_name().is_ok());
        }
    }
}
