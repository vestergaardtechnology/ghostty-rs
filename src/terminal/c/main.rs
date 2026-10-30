//! Port of `src/terminal/c/main.zig`.
//!
//! STATUS: partial — the re-exports of the batch-1 C API files are ported;
//! the `formatter`, `render`, `terminal` (beyond new/free), `selection`,
//! `selection_gesture`, `search`, `snapshot`, `kitty_graphics`, `grid_ref`,
//! `grid_ref_tracked` and `terminal_paste` aliases are TODO(c-api batch 2/3)
//! (marked below).
//!
//! The C API of libghostty-vt (`include/ghostty/vt.h`).

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "allocator.rs"]
pub mod allocator;
#[path = "build_info.rs"]
pub mod build_info;
#[path = "cell.rs"]
pub mod cell;
#[path = "color.rs"]
pub mod color;
#[path = "color_scheme.rs"]
pub mod color_scheme;
#[path = "focus.rs"]
pub mod focus;
#[path = "formatter.rs"]
pub mod formatter;
#[path = "grid_ref.rs"]
pub mod grid_ref;
#[path = "grid_ref_tracked.rs"]
pub mod grid_ref_tracked;
#[path = "io.rs"]
pub mod io;
#[path = "key_encode.rs"]
pub mod key_encode;
#[path = "key_event.rs"]
pub mod key_event;
#[path = "kitty_graphics.rs"]
pub mod kitty_graphics;
#[path = "modes.rs"]
pub mod modes;
#[path = "mouse_encode.rs"]
pub mod mouse_encode;
#[path = "mouse_event.rs"]
pub mod mouse_event;
#[path = "osc.rs"]
pub mod osc;
#[path = "paste.rs"]
pub mod paste;
#[path = "render.rs"]
pub mod render;
#[path = "result.rs"]
pub mod result;
#[path = "row.rs"]
pub mod row;
#[path = "search.rs"]
pub mod search;
#[path = "selection.rs"]
pub mod selection;
#[path = "selection_gesture.rs"]
pub mod selection_gesture;
#[path = "sgr.rs"]
pub mod sgr;
#[path = "size_report.rs"]
pub mod size_report;
#[path = "snapshot.rs"]
pub mod snapshot;
#[path = "style.rs"]
pub mod style;
#[path = "sys.rs"]
pub mod sys;
#[path = "terminal.rs"]
pub mod terminal;
#[path = "types.rs"]
pub mod types;
#[path = "unicode.rs"]
pub mod unicode;
// @generated-mods-end

// The full C API. Every function is also exported under its C name
// (`ghostty_<name>`, see `include/ghostty/vt.h`) with `#[export_name]`
// on its definition; Rust always compiles and exports them (Zig only
// with `options.c_abi`), which costs nothing when unused.

pub use self::build_info::get as build_info;

pub use self::osc::command_data as osc_command_data;
pub use self::osc::command_type as osc_command_type;
pub use self::osc::end as osc_end;
pub use self::osc::free as osc_free;
pub use self::osc::new as osc_new;
pub use self::osc::next as osc_next;
pub use self::osc::reset as osc_reset;
pub use self::osc::set as osc_set;

pub use self::color::contrast as color_contrast;
pub use self::color::luminance as color_luminance;
pub use self::color::palette_default as color_palette_default;
pub use self::color::palette_generate as color_palette_generate;
pub use self::color::parse as color_parse;
pub use self::color::parse_palette_entry as color_parse_palette_entry;
pub use self::color::parse_x11 as color_parse_x11;
pub use self::color::perceived_luminance as color_perceived_luminance;
pub use self::color::rgb_get as color_rgb_get;
pub use self::color::x11_name_count as color_x11_name_count;
pub use self::color::x11_names as color_x11_names;

pub use self::color_scheme::report_encode as color_scheme_report_encode;

pub use self::focus::encode as focus_encode;

pub use self::modes::report_encode as mode_report_encode;

// TODO(c-api batch 2/3): formatter_terminal_new, formatter_format,
// formatter_format_buf, formatter_format_alloc, formatter_free
// (c/formatter).

// TODO(c-api batch 2/3): render_state_* (c/render): new, free, update,
// begin_update, end_update, clean, get, get_multi, set,
// row_iterator_new/next/next_dirty/free, row_get, row_get_multi, row_set,
// row_cells_new/next/select/get/get_multi/free,
// image_iterator_new/next/free, image_get, image_get_multi.

pub use self::sgr::attribute_tag as sgr_attribute_tag;
pub use self::sgr::attribute_value as sgr_attribute_value;
pub use self::sgr::free as sgr_free;
pub use self::sgr::new as sgr_new;
pub use self::sgr::next as sgr_next;
pub use self::sgr::reset as sgr_reset;
pub use self::sgr::set_params as sgr_set_params;
pub use self::sgr::unknown_full as sgr_unknown_full;
pub use self::sgr::unknown_partial as sgr_unknown_partial;

pub use self::key_event::free as key_event_free;
pub use self::key_event::get_action as key_event_get_action;
pub use self::key_event::get_composing as key_event_get_composing;
pub use self::key_event::get_consumed_mods as key_event_get_consumed_mods;
pub use self::key_event::get_key as key_event_get_key;
pub use self::key_event::get_mods as key_event_get_mods;
pub use self::key_event::get_unshifted_codepoint as key_event_get_unshifted_codepoint;
pub use self::key_event::get_utf8 as key_event_get_utf8;
pub use self::key_event::new as key_event_new;
pub use self::key_event::set_action as key_event_set_action;
pub use self::key_event::set_composing as key_event_set_composing;
pub use self::key_event::set_consumed_mods as key_event_set_consumed_mods;
pub use self::key_event::set_key as key_event_set_key;
pub use self::key_event::set_mods as key_event_set_mods;
pub use self::key_event::set_unshifted_codepoint as key_event_set_unshifted_codepoint;
pub use self::key_event::set_utf8 as key_event_set_utf8;

pub use self::key_encode::encode as key_encoder_encode;
pub use self::key_encode::free as key_encoder_free;
pub use self::key_encode::new as key_encoder_new;
pub use self::key_encode::setopt as key_encoder_setopt;
pub use self::key_encode::setopt_from_terminal as key_encoder_setopt_from_terminal;

pub use self::mouse_event::clear_button as mouse_event_clear_button;
pub use self::mouse_event::free as mouse_event_free;
pub use self::mouse_event::get_action as mouse_event_get_action;
pub use self::mouse_event::get_button as mouse_event_get_button;
pub use self::mouse_event::get_mods as mouse_event_get_mods;
pub use self::mouse_event::get_position as mouse_event_get_position;
pub use self::mouse_event::new as mouse_event_new;
pub use self::mouse_event::set_action as mouse_event_set_action;
pub use self::mouse_event::set_button as mouse_event_set_button;
pub use self::mouse_event::set_mods as mouse_event_set_mods;
pub use self::mouse_event::set_position as mouse_event_set_position;

pub use self::mouse_encode::encode as mouse_encoder_encode;
pub use self::mouse_encode::free as mouse_encoder_free;
pub use self::mouse_encode::new as mouse_encoder_new;
pub use self::mouse_encode::reset as mouse_encoder_reset;
pub use self::mouse_encode::setopt as mouse_encoder_setopt;
pub use self::mouse_encode::setopt_from_terminal as mouse_encoder_setopt_from_terminal;

pub use self::paste::encode as paste_encode;
pub use self::paste::is_safe as paste_is_safe;
// TODO(c-api batch 2): terminal_paste (paste.terminal_paste).

pub use self::allocator::alloc as alloc_alloc;
pub use self::allocator::free as alloc_free;

pub use self::size_report::encode as size_report_encode;

pub use self::cell::get as cell_get;
pub use self::cell::get_multi as cell_get_multi;

pub use self::row::get as row_get;
pub use self::row::get_multi as row_get_multi;

pub use self::style::default_style as style_default;
pub use self::style::style_is_default;

pub use self::sys::log_stderr as sys_log_stderr;
pub use self::sys::set as sys_set;

pub use self::terminal::free as terminal_free;
pub use self::terminal::new as terminal_new;
// TODO(c-api batch 2/3): the rest of the terminal_* API (c/terminal:
// reset, resize, set, vt_write, vt_write_until, scroll_viewport,
// compression_activity, compress, get, get_multi, continuation_write,
// continuation_buf, continuation_alloc, grid_ref, grid_ref_track,
// point_from_grid_ref), terminal_select_* / terminal_selection_*
// (c/selection), selection_gesture_* (c/selection_gesture), search_*
// (c/search), snapshot_* (c/snapshot), kitty_graphics_* (c/kitty_graphics),
// grid_ref_* (c/grid_ref) and tracked_grid_ref_* (c/grid_ref_tracked).

pub use self::types::get_json as type_json;

pub use self::unicode::codepoint_width as unicode_codepoint_width;
pub use self::unicode::grapheme_width as unicode_grapheme_width;

#[cfg(test)]
pub(crate) mod tests {
    use std::ffi::c_int;
    use std::mem::{align_of, offset_of, size_of};

    use crate::lib::check_ghostty_h_enum;

    /// Every libghostty-vt header, concatenated (the Rust counterpart of
    /// Zig's `@cImport` of `ghostty/vt.h`).
    pub(crate) fn all_headers() -> String {
        [
            include_str!("../../../include/ghostty/vt/types.h"),
            include_str!("../../../include/ghostty/vt/allocator.h"),
            include_str!("../../../include/ghostty/vt/build_info.h"),
            include_str!("../../../include/ghostty/vt/color.h"),
            include_str!("../../../include/ghostty/vt/color_scheme.h"),
            include_str!("../../../include/ghostty/vt/device.h"),
            include_str!("../../../include/ghostty/vt/focus.h"),
            include_str!("../../../include/ghostty/vt/formatter.h"),
            include_str!("../../../include/ghostty/vt/grid_ref.h"),
            include_str!("../../../include/ghostty/vt/grid_ref_tracked.h"),
            include_str!("../../../include/ghostty/vt/io.h"),
            include_str!("../../../include/ghostty/vt/key.h"),
            include_str!("../../../include/ghostty/vt/key/encoder.h"),
            include_str!("../../../include/ghostty/vt/key/event.h"),
            include_str!("../../../include/ghostty/vt/kitty_graphics.h"),
            include_str!("../../../include/ghostty/vt/modes.h"),
            include_str!("../../../include/ghostty/vt/mouse.h"),
            include_str!("../../../include/ghostty/vt/mouse/encoder.h"),
            include_str!("../../../include/ghostty/vt/mouse/event.h"),
            include_str!("../../../include/ghostty/vt/osc.h"),
            include_str!("../../../include/ghostty/vt/paste.h"),
            include_str!("../../../include/ghostty/vt/point.h"),
            include_str!("../../../include/ghostty/vt/render.h"),
            include_str!("../../../include/ghostty/vt/screen.h"),
            include_str!("../../../include/ghostty/vt/search.h"),
            include_str!("../../../include/ghostty/vt/selection.h"),
            include_str!("../../../include/ghostty/vt/sgr.h"),
            include_str!("../../../include/ghostty/vt/size_report.h"),
            include_str!("../../../include/ghostty/vt/snapshot.h"),
            include_str!("../../../include/ghostty/vt/style.h"),
            include_str!("../../../include/ghostty/vt/sys.h"),
            include_str!("../../../include/ghostty/vt/terminal.h"),
            include_str!("../../../include/ghostty/vt/unicode.h"),
            include_str!("../../../include/ghostty/vt/wasm.h"),
            include_str!("../../../include/ghostty/vt.h"),
        ]
        .join("\n")
    }

    /// Rust port: Zig compares the C API enums against `ghostty/vt.h` with
    /// `checkGhosttyHEnum` where they are declared; this checks the batch-1
    /// enums whose Rust variant names spell the header constants.
    #[test]
    fn enums_match_vt_headers() {
        use super::*;
        let h = all_headers();
        check_ghostty_h_enum::<result::Result>("GHOSTTY_", &h).unwrap();
        check_ghostty_h_enum::<build_info::BuildInfo>("GHOSTTY_BUILD_INFO_", &h).unwrap();
        check_ghostty_h_enum::<build_info::OptimizeMode>("GHOSTTY_OPTIMIZE_", &h).unwrap();
        check_ghostty_h_enum::<cell::CellData>("GHOSTTY_CELL_DATA_", &h).unwrap();
        check_ghostty_h_enum::<cell::ContentTag>("GHOSTTY_CELL_CONTENT_", &h).unwrap();
        check_ghostty_h_enum::<cell::Wide>("GHOSTTY_CELL_WIDE_", &h).unwrap();
        check_ghostty_h_enum::<cell::SemanticContent>("GHOSTTY_CELL_SEMANTIC_", &h).unwrap();
        check_ghostty_h_enum::<row::RowData>("GHOSTTY_ROW_DATA_", &h).unwrap();
        check_ghostty_h_enum::<row::SemanticPrompt>("GHOSTTY_ROW_SEMANTIC_", &h).unwrap();
        check_ghostty_h_enum::<osc::Option>("GHOSTTY_OSC_OPT_", &h).unwrap();
        check_ghostty_h_enum::<osc::CommandData>("GHOSTTY_OSC_DATA_", &h).unwrap();
        check_ghostty_h_enum::<sgr::Underline>("GHOSTTY_SGR_UNDERLINE_", &h).unwrap();
        check_ghostty_h_enum::<style::ColorTag>("GHOSTTY_STYLE_COLOR_", &h).unwrap();
        check_ghostty_h_enum::<sys::LogLevel>("GHOSTTY_SYS_LOG_LEVEL_", &h).unwrap();
        check_ghostty_h_enum::<sys::Option>("GHOSTTY_SYS_OPT_", &h).unwrap();
        check_ghostty_h_enum::<modes::ReportState>("GHOSTTY_MODE_REPORT_", &h).unwrap();
        check_ghostty_h_enum::<focus::Event>("GHOSTTY_FOCUS_", &h).unwrap();
        check_ghostty_h_enum::<color_scheme::ColorScheme>("GHOSTTY_COLOR_SCHEME_", &h).unwrap();
        check_ghostty_h_enum::<mouse_encode::Option>("GHOSTTY_MOUSE_ENCODER_OPT_", &h).unwrap();
        check_ghostty_h_enum::<mouse_encode::TrackingMode>("GHOSTTY_MOUSE_TRACKING_", &h).unwrap();
        check_ghostty_h_enum::<mouse_encode::Format>("GHOSTTY_MOUSE_FORMAT_", &h).unwrap();
        check_ghostty_h_enum::<paste::Source>("GHOSTTY_PASTE_SOURCE_", &h).unwrap();
    }

    /// Rust port: struct layouts against the C declarations in the
    /// headers.
    #[test]
    fn struct_layouts_match_vt_headers() {
        use super::*;
        let ptr = size_of::<usize>();

        // GhosttyString / GhosttyBuffer
        assert_eq!(2 * ptr, size_of::<crate::lib::String>());
        assert_eq!(3 * ptr, size_of::<crate::lib::Buffer>());
        // GhosttyColorRgb
        assert_eq!(3, size_of::<crate::terminal::color::RgbC>());
        assert_eq!(1, align_of::<crate::terminal::color::RgbC>());
        // GhosttyColorPaletteMask
        assert_eq!(32, size_of::<color::PaletteMask>());
        // GhosttyStyleColor: { int tag; union { uint8_t; rgb; uint64_t } }
        assert_eq!(16, size_of::<style::Color>());
        assert_eq!(8, offset_of!(style::Color, value));
        // GhosttyStyle
        assert_eq!(0, offset_of!(style::Style, size));
        assert_eq!(ptr, offset_of!(style::Style, fg_color));
        assert_eq!(ptr + 48, offset_of!(style::Style, bold));
        assert_eq!(ptr + 56, offset_of!(style::Style, underline));
        // GhosttySgrAttribute: { int tag; union { ...; uint64_t _padding[8]; } }
        assert_eq!(64, size_of::<sgr::AttributeCValue>());
        assert_eq!(72, size_of::<sgr::AttributeC>());
        assert_eq!(4 * ptr, size_of::<crate::terminal::sgr::UnknownC>());
        // GhosttyMousePosition / GhosttyMouseEncoderSize
        assert_eq!(8, size_of::<mouse_event::Position>());
        assert_eq!((ptr + 8 * 4).next_multiple_of(ptr), size_of::<mouse_encode::Size>());
        // GhosttySizeReportSize
        assert_eq!(12, size_of::<size_report::Size>());
        // GhosttySysImage
        assert_eq!(8 + 2 * ptr, size_of::<sys::Image>());
        // GhosttyReader / GhosttyWriter / GhosttyMimeReader
        assert_eq!(2 * ptr, size_of::<io::Reader>());
        assert_eq!(2 * ptr, size_of::<io::Writer>());
        assert_eq!(2 * ptr, size_of::<io::MimeReader>());
        // GhosttyColorX11Entry
        assert_eq!(2 * ptr, size_of::<color::X11Entry>());
        // GhosttyCellsView
        assert_eq!(2 * ptr, size_of::<cell::CellsView>());
        // GhosttySurfacePosition / GhosttyCodepoints
        assert_eq!(16, size_of::<types::SurfacePosition>());
        assert_eq!(2 * ptr, size_of::<types::Codepoints>());
        // GhosttyAllocator
        assert_eq!(2 * ptr, size_of::<crate::lib::allocator::Allocator>());
        assert_eq!(4 * ptr, size_of::<crate::lib::allocator::VTable>());
        // GhosttyPaste
        assert_eq!(0, offset_of!(paste::Request, size));
        assert_eq!(ptr, offset_of!(paste::Request, location));
        assert_eq!(ptr + 4, offset_of!(paste::Request, source));
        assert_eq!(ptr + 8, offset_of!(paste::Request, mimes));
        // Enums are C ints.
        assert_eq!(size_of::<c_int>(), size_of::<result::Result>());
        assert_eq!(size_of::<c_int>(), size_of::<sgr::AttributeTag>());
        assert_eq!(size_of::<c_int>(), size_of::<paste::Source>());
    }
}
