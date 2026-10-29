//! Port of `src/renderer/row.zig`.
//!
//! STATUS: ported (uses minimal stand-ins for `terminal.page.Row`,
//! `terminal.page.Cell` and `terminal.Style`, which are not yet ported)

use crate::terminal::color;

use self::terminal_standin as terminal;

// TODO: Test neverExtendBg function

/// Returns true if the row of this pin should never have its background
/// color extended for filling padding space in the renderer. This is
/// a set of heuristics that help making our padding look better.
pub fn never_extend_bg(
    row: terminal::Row,
    cells: &[terminal::Cell],
    styles: &[terminal::Style],
    palette: &color::Palette,
    default_background: color::Rgb,
) -> bool {
    // Any semantic prompts should not have their background extended
    // because prompts often contain special formatting (such as
    // powerline) that looks bad when extended.
    match row.semantic_prompt {
        terminal::SemanticPrompt::Prompt | terminal::SemanticPrompt::PromptContinuation => {
            return true
        }
        terminal::SemanticPrompt::None => {}
    }

    for (x, cell) in cells.iter().enumerate() {
        // If any cell has a default background color then we don't
        // extend because the default background color probably looks
        // good enough as an extension.
        match cell.content_tag {
            // If it is a background color cell, we check the color.
            terminal::ContentTag::BgColorPalette | terminal::ContentTag::BgColorRgb => {
                let s = if cell.has_styling() {
                    styles[x]
                } else {
                    terminal::Style::default()
                };
                let Some(bg) = s.bg(cell, palette) else {
                    return true;
                };
                if bg.eql(default_background) {
                    return true;
                }
            }

            // If its a codepoint cell we can check the style.
            terminal::ContentTag::Codepoint | terminal::ContentTag::CodepointGrapheme => {
                // For codepoint containing, we also never extend bg
                // if any cell has a powerline glyph because these are
                // perfect-fit.
                match cell.codepoint() {
                    // Powerline
                    0xE0B0..=0xE0C8 | 0xE0CA | 0xE0CC..=0xE0D2 | 0xE0D4 => return true,
                    _ => {}
                }

                // Never extend a cell that has a default background.
                // A default background is applied if there is no background
                // on the style or the explicitly set background
                // matches our default background.
                let s = if cell.has_styling() {
                    styles[x]
                } else {
                    terminal::Style::default()
                };
                let Some(bg) = s.bg(cell, palette) else {
                    return true;
                };
                if bg.eql(default_background) {
                    return true;
                }
            }
        }
    }

    false
}

/// Minimal stand-ins for the parts of `terminal.page` / `terminal.style`
/// that `neverExtendBg` reads.
///
/// TODO(port-dedupe): replace with crate::terminal::page::{Row, Cell} and
/// crate::terminal::style::Style once those are ported.
pub mod terminal_standin {
    use crate::terminal::color;

    /// Zig: `page.Row.SemanticPrompt`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum SemanticPrompt {
        #[default]
        None,
        Prompt,
        PromptContinuation,
    }

    /// Zig: `page.Row` (only `semantic_prompt`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Row {
        pub semantic_prompt: SemanticPrompt,
    }

    /// Zig: `page.Cell.ContentTag`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum ContentTag {
        #[default]
        Codepoint,
        CodepointGrapheme,
        BgColorPalette,
        BgColorRgb,
    }

    /// Zig: `page.Cell` (content and style id only).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Cell {
        pub content_tag: ContentTag,
        /// The codepoint (`codepoint`/`codepoint_grapheme`).
        pub content_codepoint: u32,
        /// The palette index (`bg_color_palette`).
        pub content_color_palette: u8,
        /// The RGB color (`bg_color_rgb`).
        pub content_color_rgb: color::Rgb,
        pub style_id: u16,
    }

    impl Cell {
        pub fn has_styling(&self) -> bool {
            self.style_id != 0
        }

        pub fn codepoint(&self) -> u32 {
            match self.content_tag {
                ContentTag::Codepoint | ContentTag::CodepointGrapheme => self.content_codepoint,
                ContentTag::BgColorPalette | ContentTag::BgColorRgb => 0,
            }
        }
    }

    /// Zig: `Style.Color`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum StyleColor {
        #[default]
        None,
        Palette(u8),
        Rgb(color::Rgb),
    }

    /// Zig: `terminal.Style` (only `bg_color`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Style {
        pub bg_color: StyleColor,
    }

    impl Style {
        pub fn bg(&self, cell: &Cell, palette: &color::Palette) -> Option<color::Rgb> {
            match cell.content_tag {
                ContentTag::BgColorPalette => Some(palette[cell.content_color_palette as usize]),
                ContentTag::BgColorRgb => Some(cell.content_color_rgb),
                _ => match self.bg_color {
                    StyleColor::None => None,
                    StyleColor::Palette(idx) => Some(palette[idx as usize]),
                    StyleColor::Rgb(rgb) => Some(rgb),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::terminal_standin::*;
    use super::*;

    // Not a Zig test (the Zig file has a TODO for one).
    #[test]
    fn never_extend_bg_heuristics() {
        let palette: color::Palette = [color::Rgb { r: 1, g: 2, b: 3 }; 256];
        let default_bg = color::Rgb { r: 0, g: 0, b: 0 };
        let red = color::Rgb { r: 255, g: 0, b: 0 };
        let styled = Style {
            bg_color: StyleColor::Rgb(red),
        };

        // Prompt rows never extend.
        let prompt = Row {
            semantic_prompt: SemanticPrompt::Prompt,
        };
        assert!(never_extend_bg(prompt, &[], &[], &palette, default_bg));

        // A row of styled cells with a non-default background extends.
        let cell = Cell {
            content_codepoint: 'a' as u32,
            style_id: 1,
            ..Default::default()
        };
        let row = Row::default();
        assert!(!never_extend_bg(
            row,
            &[cell; 2],
            &[styled; 2],
            &palette,
            default_bg
        ));

        // An unstyled cell has the default background.
        let plain = Cell {
            content_codepoint: 'a' as u32,
            ..Default::default()
        };
        assert!(never_extend_bg(
            row,
            &[cell, plain],
            &[styled; 2],
            &palette,
            default_bg
        ));

        // Powerline glyphs never extend.
        let powerline = Cell {
            content_codepoint: 0xE0B0,
            style_id: 1,
            ..Default::default()
        };
        assert!(never_extend_bg(
            row,
            &[powerline],
            &[styled],
            &palette,
            default_bg
        ));

        // Background color cells are checked against the default.
        let bg_cell = Cell {
            content_tag: ContentTag::BgColorRgb,
            content_color_rgb: default_bg,
            ..Default::default()
        };
        assert!(never_extend_bg(
            row,
            &[bg_cell],
            &[styled],
            &palette,
            default_bg
        ));
        let bg_cell = Cell {
            content_tag: ContentTag::BgColorPalette,
            content_color_palette: 7,
            ..Default::default()
        };
        assert!(!never_extend_bg(
            row,
            &[bg_cell],
            &[styled],
            &palette,
            default_bg
        ));
    }
}
