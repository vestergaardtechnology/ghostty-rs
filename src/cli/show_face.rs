//! Port of `src/cli/show_face.zig`.
//!
//! STATUS: partial — the font grid (`font.SharedGridSet`, `SharedGrid`,
//! `CodepointResolver`, `Collection`) is not ported yet; [`grid`] is a
//! stand-in resolver (TODO(port-dedupe)) built on the ported font discovery
//! that checks Ghostty's sprites, then the configured `font-family*` faces,
//! then fallback discovery. It does not know the embedded default fonts
//! (JetBrains Mono, Symbols Nerd Font, emoji) or codepoint-map overrides, so
//! codepoints Ghostty would render from them are reported from the system
//! font that provides them instead.

use std::io::{self, BufWriter, Write};

use super::args::{self, cli_enum, cli_struct, ArgIterator, Error, Fields, ParseHooks};
use super::diagnostics::{DiagnosticList, LocationKey};
use super::ghostty::{global_args, Action, RunResult};
use crate::config::Config;
use crate::font;

cli_enum! {
    /// The font style to search for (Zig `font.Style`).
    // TODO(port-dedupe): use crate::font::Style directly once it implements
    // CliEnum.
    pub enum StyleArg {
        Regular => "regular",
        Bold => "bold",
        Italic => "italic",
        BoldItalic => "bold_italic",
    }
}

impl From<StyleArg> for font::Style {
    fn from(s: StyleArg) -> font::Style {
        match s {
            StyleArg::Regular => font::Style::Regular,
            StyleArg::Bold => font::Style::Bold,
            StyleArg::Italic => font::Style::Italic,
            StyleArg::BoldItalic => font::Style::BoldItalic,
        }
    }
}

cli_enum! {
    /// The presentation to force (Zig `font.Presentation`).
    // TODO(port-dedupe): use crate::font::Presentation directly once it
    // implements CliEnum.
    pub enum PresentationArg {
        Text => "text",
        Emoji => "emoji",
    }
}

impl From<PresentationArg> for font::Presentation {
    fn from(p: PresentationArg) -> font::Presentation {
        match p {
            PresentationArg::Text => font::Presentation::Text,
            PresentationArg::Emoji => font::Presentation::Emoji,
        }
    }
}

cli_struct! {
    #[derive(Debug, Clone)]
    pub struct Options {
        /// The codepoint to search for.
        pub cp: Option<u32> = None,

        /// Search for all of the codepoints in the string.
        pub string: Option<String> = None,

        /// Font style to search for.
        pub style: StyleArg = StyleArg::Regular,

        /// If specified, force text or emoji presentation.
        pub presentation: Option<PresentationArg> = None,
    }
    private {
        // Enable arg parsing diagnostics so that we don't get an error if
        // there is a "normal" config setting on the cli.
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
}

/// The help text of this action (Zig `help_strings.Action.@"show-face"`).
pub const HELP: &str = include_str!("show_face_help.txt");

/// The `show-face` command shows what font face Ghostty will use to render a
/// specific codepoint. See [`HELP`].
pub fn run() -> RunResult {
    let mut iter = args::args_iterator(global_args());

    let mut stdout = BufWriter::with_capacity(1024, io::stdout().lock());
    // Note: like the Zig version, errors go to stdout as well.
    let mut stderr: Vec<u8> = Vec::new();

    let result = run_args(&mut iter, &mut stdout, &mut stderr);
    let _ = stdout.write_all(&stderr);
    let _ = stdout.flush();
    result
}

fn run_args<I: ArgIterator + ?Sized, O: Write + ?Sized, E: Write + ?Sized>(
    args_iter: &mut I,
    stdout: &mut O,
    stderr: &mut E,
) -> RunResult {
    // Its possible to build Ghostty without font discovery!
    if !super::list_fonts::DISCOVERY_AVAILABLE {
        write!(
            stderr,
            "Ghostty was built without a font discovery mechanism. This is a compile-time
option. Please review how Ghostty was built from source, contact the
maintainer to enable a font discovery mechanism, and try again."
        )?;
        return Ok(1);
    }

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
    if !opts._diagnostics.empty() {
        let mut exit = false;
        for diagnostic in opts._diagnostics.items() {
            if diagnostic.location.key() != LocationKey::Cli {
                continue;
            }
            for field in Options::FIELDS {
                if field.name == diagnostic.key.as_str() {
                    writeln!(stderr, "config error: {}", diagnostic)?;
                    exit = true;
                }
            }
        }
        if exit {
            return Ok(1);
        }
    }

    if opts.cp.is_none() && opts.string.is_none() {
        writeln!(stderr, "You must specify a codepoint with --cp or a string with --string")?;
        return Ok(1);
    }

    let config = match Config::load() {
        Ok(config) => config,
        Err(err) => {
            write!(stderr, "Unable to load config: {}", err)?;
            return Ok(1);
        }
    };

    // Print out any diagnostics generated from parsing the config, unless
    // the diagnostic might have been generated because it's actually an
    // action-specific argument.
    if !config._diagnostics.empty() {
        'outer: for diagnostic in config._diagnostics.items() {
            for field in Options::FIELDS {
                if field.name == diagnostic.key.as_str()
                    && matches!(diagnostic.location.key(), LocationKey::None | LocationKey::Cli)
                {
                    continue 'outer;
                }
            }
            writeln!(stderr, "config error: {}", diagnostic)?;
        }
    }

    let font_grid = match grid::SharedGrid::init(&config) {
        Ok(grid) => grid,
        Err(err) => {
            write!(stderr, "Unable to initialize font grid set: {}", err)?;
            return Ok(1);
        }
    };

    let style: font::Style = opts.style.into();
    let presentation: Option<font::Presentation> = opts.presentation.map(Into::into);

    if let Some(cp) = opts.cp {
        if let Some(rc) = lookup(stdout, stderr, &font_grid, style, presentation, cp)? {
            return Ok(rc);
        }
    }
    if let Some(string) = &opts.string {
        for c in string.chars() {
            if let Some(rc) = lookup(stdout, stderr, &font_grid, style, presentation, c as u32)? {
                return Ok(rc);
            }
        }
    }

    Ok(0)
}

/// Zig `{X:0>2}` and `{u}` of a codepoint.
fn cp_strings(cp: u32) -> (String, String) {
    (
        format!("{:02X}", cp),
        char::from_u32(cp).map(String::from).unwrap_or_else(|| "\u{FFFD}".to_owned()),
    )
}

fn lookup<O: Write + ?Sized, E: Write + ?Sized>(
    stdout: &mut O,
    stderr: &mut E,
    font_grid: &grid::SharedGrid,
    style: font::Style,
    presentation: Option<font::Presentation>,
    cp: u32,
) -> io::Result<Option<u8>> {
    let (hex, ch) = cp_strings(cp);
    let Some(idx) = font_grid.get_index(cp, style, presentation) else {
        writeln!(stdout, "U+{} « {} » not found.", hex, ch)?;
        return Ok(None);
    };

    let name = match idx {
        grid::Index::Sprite => {
            writeln!(stdout, "U+{} « {} » is handled by Ghostty's internal sprites.", hex, ch)?;
            return Ok(None);
        }
        grid::Index::Face(name) => name,
        grid::Index::Error(err) => {
            write!(stderr, "Unable to get name of face: {}", err)?;
            return Ok(Some(1));
        }
    };

    writeln!(stdout, "U+{} « {} » found in face “{}”.", hex, ch, name)?;

    Ok(None)
}

/// Stand-in for the font grid used by `show-face`.
///
/// TODO(port-dedupe): replace with `crate::font::SharedGridSet` /
/// `SharedGrid.resolver.getIndex` / `collection.getFace` once ported.
mod grid {
    use crate::config::Config;
    use crate::font::{self, DeferredFace, Descriptor, Discover, Presentation, Style};

    /// The result of a lookup (Zig `Collection.Index` resolved to a face
    /// name, or `error.SpecialHasNoFace` for sprites).
    pub enum Index {
        Sprite,
        Face(String),
        Error(String),
    }

    pub struct SharedGrid {
        lib: font::Library,
        /// The configured families per style (Zig: the families loaded
        /// into the collection by `SharedGridSet.DerivedConfig`).
        families: [Vec<String>; 4],
        sprite: font::SpriteFace,
    }

    impl Drop for SharedGrid {
        fn drop(&mut self) {
            self.lib.deinit();
        }
    }

    fn face_index(face: &DeferredFace) -> Index {
        let mut buf = [0u8; 1024];
        match face.name(&mut buf) {
            Ok(name) => Index::Face(String::from_utf8_lossy(name).into_owned()),
            Err(err) => Index::Error(err.to_string()),
        }
    }

    impl SharedGrid {
        pub fn init(config: &Config) -> Result<SharedGrid, String> {
            let list = |v: &crate::config::RepeatableString| -> Vec<String> {
                v.list.iter().map(|s| s.as_str().to_owned()).collect()
            };
            Ok(SharedGrid {
                lib: font::Library::init().map_err(|e| format!("{:?}", e))?,
                families: [
                    list(&config.font_family),
                    list(&config.font_family_bold),
                    list(&config.font_family_italic),
                    list(&config.font_family_bold_italic),
                ],
                // Only `has_codepoint` is used, which doesn't depend on the
                // metrics.
                sprite: font::SpriteFace {
                    metrics: font::metrics::Metrics::calc(font::metrics::FaceMetrics {
                        px_per_em: 16.0,
                        cell_width: 8.0,
                        ascent: 12.0,
                        descent: -4.0,
                        line_gap: 0.0,
                        ..Default::default()
                    }),
                },
            })
        }

        fn style_index(style: Style) -> usize {
            match style {
                Style::Regular => 0,
                Style::Bold => 1,
                Style::Italic => 2,
                Style::BoldItalic => 3,
            }
        }

        /// Search the configured families of a style.
        fn collection_get(&self, cp: u32, style: Style, p: Option<Presentation>) -> Option<Index> {
            let disco = Discover::init(&self.lib);
            let families = &self.families[Self::style_index(style)];
            for family in families {
                let Ok(mut it) = disco.discover(&Descriptor {
                    family: Some(family.clone()),
                    bold: matches!(style, Style::Bold | Style::BoldItalic),
                    italic: matches!(style, Style::Italic | Style::BoldItalic),
                    ..Default::default()
                }) else {
                    continue;
                };
                // The collection holds the best match of each family.
                if let Ok(Some(face)) = it.next() {
                    if face.has_codepoint(cp, p) {
                        return Some(face_index(&face));
                    }
                }
            }
            None
        }

        /// `CodepointResolver.getIndex`.
        pub fn get_index(&self, cp: u32, style: Style, p: Option<Presentation>) -> Option<Index> {
            // If the style is not regular and has no configured faces, use
            // regular.
            if style != Style::Regular && self.families[Self::style_index(style)].is_empty() {
                return self.get_index(cp, Style::Regular, p);
            }

            // If we have sprite drawing enabled, check if our sprite face can
            // handle this.
            if self.sprite.has_codepoint(cp, p) {
                return Some(Index::Sprite);
            }

            if let Some(idx) = self.collection_get(cp, style, p) {
                return Some(idx);
            }

            // If we're not a regular font style, try looking for a regular
            // font that will satisfy this request.
            if style != Style::Regular {
                if let Some(idx) = self.get_index(cp, Style::Regular, p) {
                    return Some(idx);
                }
            }

            // If we are regular, try looking for a fallback using discovery.
            if style == Style::Regular {
                let disco = Discover::init(&self.lib);
                if let Ok(mut it) = disco.discover(&Descriptor {
                    codepoint: cp,
                    ..Default::default()
                }) {
                    while let Ok(Some(face)) = it.next() {
                        if !face.has_codepoint(cp, p) {
                            continue;
                        }
                        return Some(face_index(&face));
                    }
                }
            }

            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::ArgIteratorGeneral;

    #[test]
    fn parse_options() {
        let mut opts = Options::default();
        let mut iter = ArgIteratorGeneral::init("--cp=0x41 --style=bold_italic --presentation=emoji --font-size=12");
        args::parse(&mut opts, &mut iter).unwrap();
        assert_eq!(Some(0x41), opts.cp);
        assert_eq!(StyleArg::BoldItalic, opts.style);
        assert_eq!(Some(PresentationArg::Emoji), opts.presentation);
        // The unknown (config) field becomes a diagnostic, not an error.
        assert_eq!(1, opts._diagnostics.items().len());
    }

    #[test]
    fn requires_cp_or_string() {
        let mut iter = ArgIteratorGeneral::init("");
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        assert_eq!(1, run_args(&mut iter, &mut out, &mut err).unwrap());
        assert!(String::from_utf8_lossy(&err).contains("You must specify"));
    }

    #[test]
    fn sprite_codepoints() {
        let grid = grid::SharedGrid::init(&Config::default()).unwrap();
        // Box drawing is handled by the sprite face.
        assert!(matches!(
            grid.get_index(0x2500, font::Style::Regular, None),
            Some(grid::Index::Sprite)
        ));
    }
}
