//! Port of `src/font/face.zig`.
//!
//! STATUS: partial — only `Variation` (+ its tests) is ported; the `Face` alias, `DesiredSize`, etc. are missing.

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "face/coretext.rs"]
pub mod coretext;
#[path = "face/freetype.rs"]
pub mod freetype;
#[path = "face/web_canvas.rs"]
pub mod web_canvas;
// @generated-mods-end

/// A font variation setting. The best documentation for this I know of
/// is actually the CSS font-variation-settings property on MDN:
/// https://developer.mozilla.org/en-US/docs/Web/CSS/font-variation-settings
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Variation {
    pub id: VariationId,
    pub value: f64,
}

/// Zig `Variation.Id`: a `packed struct(u32)` of four bytes `d, c, b, a`
/// (so `a` is the most significant byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct VariationId {
    pub d: u8,
    pub c: u8,
    pub b: u8,
    pub a: u8,
}

impl VariationId {
    pub fn init(v: &[u8; 4]) -> VariationId {
        VariationId {
            a: v[0],
            b: v[1],
            c: v[2],
            d: v[3],
        }
    }

    /// Converts the ID to a string. The return value is only valid
    /// for the lifetime of the self pointer.
    pub fn str(self) -> [u8; 4] {
        [self.a, self.b, self.c, self.d]
    }

    /// The packed `u32` representation (Zig `@bitCast(id)`).
    pub fn to_bits(self) -> u32 {
        (self.d as u32) | ((self.c as u32) << 8) | ((self.b as u32) << 16) | ((self.a as u32) << 24)
    }

    /// Inverse of [`VariationId::to_bits`].
    pub fn from_bits(v: u32) -> VariationId {
        VariationId {
            d: v as u8,
            c: (v >> 8) as u8,
            b: (v >> 16) as u8,
            a: (v >> 24) as u8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variation_id_wght_should_be_2003265652() {
        let id = VariationId::init(b"wght");
        assert_eq!(2003265652u32, id.to_bits());
        assert_eq!(b"wght", &id.str());
    }

    #[test]
    fn variation_id_slnt_should_be_1936486004() {
        let id = VariationId {
            a: b's',
            b: b'l',
            c: b'n',
            d: b't',
        };
        assert_eq!(1936486004u32, id.to_bits());
        assert_eq!(b"slnt", &id.str());
        assert_eq!(id, VariationId::from_bits(1936486004));
    }
}
