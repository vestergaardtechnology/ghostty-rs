//! Port of `src/font/discovery.zig`.
//!
//! STATUS: partial — only `Descriptor` (struct, hash, hashcode; clone via `Clone`) is ported; `Discover` backends, `toFcPattern`/CoreText conversion and tests are missing.

use super::face::Variation;
use crate::terminal::hash_map::Wyhash;

/// Descriptor is used to search for fonts. The only required field
/// is "family". The rest are ignored unless they're set to a non-zero
/// value.
///
/// NOTE: the Zig struct borrows its strings and variations (memory owned
/// by the caller); here they are owned so `clone` is a plain deep copy.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Descriptor {
    /// Font family to search for. This can be a fully qualified font
    /// name such as "Fira Code", "monospace", "serif", etc. Memory is
    /// owned by the caller and should be freed when this descriptor
    /// is no longer in use. The discovery structs will never store the
    /// descriptor.
    ///
    /// On systems that use fontconfig (Linux), this can be a full
    /// fontconfig pattern, such as "Fira Code-14:bold".
    pub family: Option<String>,

    /// Specific font style to search for. This will filter the style
    /// string the font advertises. The "bold/italic" booleans later in this
    /// struct filter by the style trait the font has, not the string, so
    /// these can be used in conjunction or not.
    pub style: Option<String>,

    /// A codepoint that this font must be able to render.
    pub codepoint: u32,

    /// Font size in points that the font should support. For conversion
    /// to pixels, we will use 72 DPI for Mac and 96 DPI for everything else.
    /// (If pixel conversion is necessary, i.e. emoji fonts)
    pub size: f32,

    /// True if we want to search specifically for a font that supports
    /// specific styles.
    pub bold: bool,
    pub italic: bool,
    pub monospace: bool,

    /// Variation axes to apply to the font. This also impacts searching
    /// for fonts since fonts with the ability to set these variations
    /// will be preferred, but not guaranteed.
    pub variations: Vec<Variation>,
}

impl Descriptor {
    /// Hash the descriptor with the given hasher.
    pub fn hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
        // autoHashStrat(.Deep) on an optional string hashes its bytes
        // only when present.
        if let Some(family) = &self.family {
            hasher.write(family.as_bytes());
        }
        if let Some(style) = &self.style {
            hasher.write(style.as_bytes());
        }
        hasher.write(&self.codepoint.to_le_bytes());
        hasher.write(&self.size.to_bits().to_le_bytes());
        hasher.write(&[self.bold as u8]);
        hasher.write(&[self.italic as u8]);
        hasher.write(&[self.monospace as u8]);
        hasher.write(&(self.variations.len() as u64).to_le_bytes());
        for variation in &self.variations {
            hasher.write(&variation.id.to_bits().to_le_bytes());

            // This is not correct, but we don't currently depend on the
            // hash value being different based on decimal values of variations.
            hasher.write(&(variation.value as i64).to_le_bytes());
        }
    }

    /// Returns a hash code that can be used to uniquely identify this
    /// action.
    pub fn hashcode(&self) -> u64 {
        let mut hasher = Wyhash::init(0);
        self.hash(&mut hasher);
        hasher.final_()
    }
}
