//! Port of `src/terminal/kitty/graphics_unicode.zig`.
//!
//! STATUS: partial — `Placement` (with `renderPlacement`/`grid`) and the
//! three "unicode render placement" tests are pending: they need
//! `terminal.Pin` (PageList), `ImageStorage`/`Image`/`RenderPlacement`
//! (kitty/graphics_storage, graphics_image, graphics_render) and
//! `Terminal`, none of which are ported yet.
//!
//! This file contains various logic and data for working with the
//! Kitty graphics protocol unicode placeholder, virtual placement feature.

use crate::terminal::style::{Color, Style};

const LOG_TARGET: &str = "kitty_gfx";

/// Codepoint for the unicode placeholder character.
pub const PLACEHOLDER: u32 = 0x10EEEE;

/// IncompletePlacement is the placement information present in a single
/// cell. It is "incomplete" because the specification allows for missing
/// diacritics and so on that continue from previous valid placements.
///
/// The render state (see `terminal.RenderState`) uses this to group
/// placeholder cells into runs while it rebuilds rows. It doesn't store a
/// position. The caller keeps track of where each run starts.
///
/// Building a run looks like this:
///
/// ```text
/// let mut run = IncompletePlacement::init(&style, graphemes).start();
/// // For each following placeholder cell:
/// let next = IncompletePlacement::init(&next_style, next_graphemes);
/// if !run.append(&next) {
///     // `run` is finished. `next` starts a new run, which
///     // continues from `next.start()`.
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncompletePlacement {
    /// Lower 24 bits of the image ID. This is specified in the fg color
    /// and is always required. (Zig: `u24`.)
    pub image_id_low: u32,

    /// Higher 8 bits of the image ID specified using the 3rd diacritic.
    /// This is optional.
    pub image_id_high: Option<u8>,

    /// Placement ID is optionally specified in the underline color.
    /// (Zig: `?u24`.)
    pub placement_id: Option<u32>,

    /// The row/col index for the image. These are 0-indexed. These
    /// are specified using diacritics. The row is first and the col
    /// is second. Both are optional. If not specified, they can continue
    /// a previous placement under certain conditions.
    pub row: Option<u32>,
    pub col: Option<u32>,

    /// The run width so far in cells.
    pub width: u32,
}

impl IncompletePlacement {
    /// Read the placement information from one placeholder cell, given
    /// its style and its combining characters (an empty slice if it has
    /// none).
    pub fn init(style: &Style, cps: &[u32]) -> IncompletePlacement {
        let mut result = IncompletePlacement {
            image_id_low: Self::color_to_id(style.fg_color),
            image_id_high: None,
            placement_id: {
                let id = Self::color_to_id(style.underline_color);
                if id != 0 {
                    Some(id)
                } else {
                    None
                }
            },
            row: None,
            col: None,
            width: 1,
        };

        // Try to decode all our diacritics. Any invalid diacritics are
        // treated as if they don't exist. This isn't explicitly specified
        // at the time of writing this but it appears to be how Kitty behaves.
        if !cps.is_empty() {
            result.row = get_index(cps[0]).or_else(|| {
                log::warn!(target: LOG_TARGET, "virtual placement with invalid row diacritic cp={:X}", cps[0]);
                None
            });

            if cps.len() > 1 {
                result.col = get_index(cps[1]).or_else(|| {
                    log::warn!(target: LOG_TARGET, "virtual placement with invalid col diacritic cp={:X}", cps[1]);
                    None
                });

                if cps.len() > 2 {
                    let high_ = get_index(cps[2]).or_else(|| {
                        log::warn!(target: LOG_TARGET, "virtual placement with invalid high diacritic cp={:X}", cps[2]);
                        None
                    });

                    if let Some(high) = high_ {
                        result.image_id_high = u8::try_from(high).ok().or_else(|| {
                            log::warn!(
                                target: LOG_TARGET,
                                "virtual placement with invalid high diacritic cp={:X} value={}",
                                cps[2],
                                high
                            );
                            None
                        });
                    }

                    // Any additional diacritics are ignored.
                }
            }
        }

        result
    }

    /// Returns this cell as the first cell of a new run. A cell that
    /// leaves out its row or column would normally continue the run to
    /// its left. At the start of a run there is nothing to continue, so
    /// a missing row or column is zero.
    pub fn start(self) -> IncompletePlacement {
        let mut result = self;
        if result.row.is_none() {
            result.row = Some(0);
        }
        if result.col.is_none() {
            result.col = Some(0);
        }
        result
    }

    /// Append this incomplete placement to an existing placement to
    /// create a run. This returns true if the placements are compatible
    /// and were combined. If this returns false, the other placement is
    /// unchanged.
    pub fn append(&mut self, other: &IncompletePlacement) -> bool {
        if !self.can_append(other) {
            return false;
        }
        self.width += 1;
        true
    }

    fn can_append(&self, other: &IncompletePlacement) -> bool {
        // Converted from Kitty's logic, don't @ me.
        self.image_id_low == other.image_id_low
            && self.placement_id == other.placement_id
            && (other.row.is_none() || other.row == self.row)
            && (other.col.is_none() || other.col == Some(self.col.unwrap() + self.width))
            && (other.image_id_high.is_none() || other.image_id_high == self.image_id_high)
    }

    /// The full image ID. The low 24 bits come from the foreground color
    /// and the high 8 bits from the optional third diacritic.
    pub fn image_id(&self) -> u32 {
        let low: u32 = self.image_id_low;
        let high: u32 = self.image_id_high.unwrap_or(0) as u32;
        low | (high << 24)
    }

    /// Convert a style color to a Kitty image protocol ID. This works by
    /// taking the 24 most significant bits of the color, which lets it work
    /// for both palette and rgb-based colors.
    fn color_to_id(c: Color) -> u32 {
        match c {
            Color::None => 0,
            Color::Palette(v) => v as u32,
            Color::Rgb(rgb) => {
                let r: u32 = rgb.r as u32;
                let g: u32 = rgb.g as u32;
                let b: u32 = rgb.b as u32;
                (r << 16) | (g << 8) | b
            }
        }
    }
}

/// Get the row/col index for a diacritic codepoint. These are 0-indexed.
///
/// Rebuilding a row of placeholders calls this up to three times per
/// cell, so it needs to be fast. Most diacritics are below U+2E00 and
/// are found with a single table lookup. The few above that use a
/// binary search.
fn get_index(cp: u32) -> Option<u32> {
    if cp < DENSE_END {
        if cp < DENSE_START {
            return None;
        }
        let idx = DENSE_INDEX[(cp - DENSE_START) as usize];
        return if idx == DENSE_NONE { None } else { Some(idx as u32) };
    }

    let sparse = &DIACRITICS[SPARSE_START..];
    let idx = sparse.binary_search(&cp).ok()?;
    Some((SPARSE_START + idx) as u32)
}

/// The range of codepoints covered by `dense_index`. It holds every
/// diacritic below U+2E00 (all but a few dozen of them) in about 11 KB,
/// and each of their indexes fits in a byte.
const DENSE_START: u32 = DIACRITICS[0];
const DENSE_END: u32 = 0x2E00;
const DENSE_NONE: u8 = u8::MAX;

/// The index in `diacritics` of the first codepoint at or above
/// `dense_end`. Codepoints from here on are found with a binary search.
const SPARSE_START: usize = {
    let mut i = 0;
    while i < DIACRITICS.len() {
        if DIACRITICS[i] >= DENSE_END {
            break;
        }
        i += 1;
    }
    i
};

/// Maps `cp - dense_start` to the index of `cp` in `diacritics`, or to
/// `dense_none` if `cp` is not a diacritic.
static DENSE_INDEX: [u8; (DENSE_END - DENSE_START) as usize] = {
    assert!(SPARSE_START < DENSE_NONE as usize, "dense diacritic index overflows u8");
    let mut table = [DENSE_NONE; (DENSE_END - DENSE_START) as usize];
    let mut i = 0;
    while i < SPARSE_START {
        table[(DIACRITICS[i] - DENSE_START) as usize] = i as u8;
        i += 1;
    }
    table
};

/// These are the diacritics used with the Kitty graphics protocol
/// Unicode placement feature to specify the row/column for placement.
/// The index into the array determines the value.
///
/// This is derived from:
/// https://sw.kovidgoyal.net/kitty/_downloads/f0a0de9ec8d9ff4456206db8e0814937/rowcolumn-diacritics.txt
const DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A,
    0x034B, 0x034C, 0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365,
    0x0366, 0x0367, 0x0368, 0x0369, 0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F,
    0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592, 0x0593, 0x0594, 0x0595, 0x0597,
    0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1, 0x05A8, 0x05A9,
    0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6,
    0x06D7, 0x06D8, 0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2,
    0x06E4, 0x06E7, 0x06E8, 0x06EB, 0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736,
    0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743, 0x0745, 0x0747, 0x0749, 0x074A,
    0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3, 0x0816, 0x0817,
    0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951,
    0x0953, 0x0954, 0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD,
    0x193A, 0x1A17, 0x1A75, 0x1A76, 0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C,
    0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71, 0x1B72, 0x1B73, 0x1CD0, 0x1CD1,
    0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4, 0x1DC5, 0x1DC6,
    0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF,
    0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1,
    0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0,
    0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2, 0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6,
    0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE, 0x2DEF, 0x2DF0,
    0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1,
    0xA8E0, 0xA8E1, 0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9,
    0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED, 0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2,
    0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23,
    0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187, 0x1D188, 0x1D189,
    0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_diacritic_sorted() {
        // diacritics must be sorted since we use a binary search.
        assert!(DIACRITICS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn unicode_diacritic() {
        // Some spot checks based on Kitty behavior
        assert_eq!(30, get_index(0x483).unwrap());
        assert_eq!(294, get_index(0x1d242).unwrap());
    }

    #[test]
    fn unicode_diacritic_lookup_matches_table() {
        // Every diacritic maps to its index, through both the dense table
        // and the binary search.
        for (i, &cp) in DIACRITICS.iter().enumerate() {
            assert_eq!(Some(i as u32), get_index(cp));
        }

        // Nothing else maps, including around both ranges.
        let mut cp: u32 = 0;
        while cp < 0x1E000 {
            if !DIACRITICS.contains(&cp) {
                assert_eq!(None, get_index(cp));
            }
            cp += 1;
        }
    }

    #[test]
    fn incomplete_placement_run() {
        let style = Style {
            fg_color: Color::Palette(1),
            ..Default::default()
        };
        let mut run = IncompletePlacement::init(&style, &[DIACRITICS[0], DIACRITICS[0]]).start();
        assert_eq!(run.row, Some(0));
        assert_eq!(run.col, Some(0));
        assert_eq!(run.image_id(), 1);
        let next = IncompletePlacement::init(&style, &[DIACRITICS[0], DIACRITICS[1]]);
        assert!(run.append(&next));
        assert_eq!(run.width, 2);
        let bad = IncompletePlacement::init(&style, &[DIACRITICS[1]]);
        assert!(!run.append(&bad));
    }

    // TODO: "unicode render placement: dog 4x2", "dog 2x2 with blank
    // cells" and "dog 1x1" need Terminal, ImageStorage and Placement
    // (see the STATUS header).
}
