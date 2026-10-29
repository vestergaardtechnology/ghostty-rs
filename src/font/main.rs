//! Port of `src/font/main.zig`.
//!
//! STATUS: partial — re-exports of ported modules, `OPTIONS`, `Style`, `Presentation`; `sprite_index`, `default_fallback_adjustment` and remaining re-exports missing.

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "atlas.rs"]
pub mod atlas;
#[path = "backend.rs"]
pub mod backend;
#[path = "codepoint_map.rs"]
pub mod codepoint_map;
#[path = "codepoint_resolver.rs"]
pub mod codepoint_resolver;
#[path = "collection.rs"]
pub mod collection;
#[path = "deferred_face.rs"]
pub mod deferred_face;
#[path = "discovery.rs"]
pub mod discovery;
#[path = "embedded.rs"]
pub mod embedded;
#[path = "face.rs"]
pub mod face;
#[path = "glyf_rasterize.rs"]
pub mod glyf_rasterize;
#[path = "glyf_rasterize_png_test.rs"]
pub mod glyf_rasterize_png_test;
#[path = "glyph.rs"]
pub mod glyph;
#[path = "library.rs"]
pub mod library;
#[path = "metrics.rs"]
pub mod metrics;
#[path = "nerd_font_attributes.rs"]
pub mod nerd_font_attributes;
#[path = "nerd_font_tables.rs"]
pub mod nerd_font_tables;
#[path = "opentype.rs"]
pub mod opentype;
#[path = "shape.rs"]
pub mod shape;
#[path = "shaper/mod.rs"]
pub mod shaper;
#[path = "shared_grid.rs"]
pub mod shared_grid;
#[path = "shared_grid_set.rs"]
pub mod shared_grid_set;
#[path = "sprite.rs"]
pub mod sprite;
// @generated-mods-end

pub use self::atlas::Atlas;
pub use self::backend::Backend;
pub use self::codepoint_map::CodepointMap;
pub use self::glyph::Glyph;
pub use self::library::Library;
pub use self::metrics::Metrics;
pub use self::sprite::Face as SpriteFace;
pub use self::sprite::Sprite;

/// Build options (Zig `options`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub backend: Backend,
}

/// Build options. The backend is the default backend for the target
/// (the Ghostty application build).
// TODO(port-dedupe): use crate::build_config font_backend once ported.
pub const OPTIONS: Options = Options {
    backend: Backend::default(backend::Target::current(), backend::WasmTarget::Browser),
};

/// The styles that a family can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Style {
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

/// The presentation for an emoji.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Presentation {
    Text = 0,  // U+FE0E
    Emoji = 1, // U+FEOF
}
