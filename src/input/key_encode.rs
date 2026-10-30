//! Port of `src/input/key_encode.zig`.
//!
//! STATUS: ported
//!
//! Deviations:
//! - `*std.Io.Writer` is any `std::io::Write` and `std.Io.Writer.Error`
//!   is `std::io::Error`.
//! - `std.unicode.Utf8View.init` (whole-string validation) is
//!   `std::str::from_utf8`; codepoints (`u21`) are `u32`.

use std::io::{self, Write};

use crate::input::config::OptionAsAlt;
use crate::input::function_keys::{self, CursorMode, KeypadMode, ModifyKeys};
use crate::input::key::{self, Key, KeyEvent, Mods};
use crate::input::key_mods::ModSide;
use crate::input::kitty::{Entry as KittyEntry, ENTRIES as KITTY_ENTRIES};
use crate::terminal::kitty::key::Flags as KittyFlags;
use crate::terminal::modes::Mode;
use crate::terminal::terminal::Terminal;

/// Options that affect key encoding behavior. This is a mix of behavior
/// from terminal state as well as application configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Terminal DEC mode 1
    pub cursor_key_application: bool,

    /// Terminal DEC mode 66
    pub keypad_key_application: bool,

    // DEC Backarrow Key Mode (DECBKM)
    // See https://vt100.net/dec/ek-vt3xx-tp-002.pdf page 170
    // If `false` (the default), `backspace` emits 0x7f
    // If `true`, `backspace` emits 0x08
    pub backarrow_key_mode: bool,

    /// Terminal DEC mode 1035
    pub ignore_keypad_with_numlock: bool,

    /// Terminal DEC mode 1036
    pub alt_esc_prefix: bool,

    /// xterm "modifyOtherKeys mode 2". Details here:
    /// https://invisible-island.net/xterm/modified-keys.html
    pub modify_other_keys_state_2: bool,

    /// Kitty keyboard protocol flags.
    pub kitty_flags: KittyFlags,

    /// Determines whether the "option" key on macOS is treated
    /// as "alt" or not. See the Ghostty `macos_option-as-alt` config
    /// docs for a more detailed description of why this is needed.
    pub macos_option_as_alt: OptionAsAlt,
}

impl Default for Options {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Options {
    pub const DEFAULT: Options = Options {
        cursor_key_application: false,
        keypad_key_application: false,
        backarrow_key_mode: false,
        ignore_keypad_with_numlock: false,
        alt_esc_prefix: false,
        modify_other_keys_state_2: false,
        kitty_flags: KittyFlags::DISABLED,
        macos_option_as_alt: OptionAsAlt::False,
    };

    /// Initialize our options from the terminal state.
    ///
    /// Note that `macos_option_as_alt` cannot be determined from
    /// terminal state so it must be set manually after this call.
    pub fn from_terminal(t: &Terminal) -> Options {
        Options {
            alt_esc_prefix: t.modes.get(Mode::AltEscPrefix),
            cursor_key_application: t.modes.get(Mode::CursorKeys),
            keypad_key_application: t.modes.get(Mode::KeypadKeys),
            backarrow_key_mode: t.modes.get(Mode::BackarrowKeyMode),
            ignore_keypad_with_numlock: t.modes.get(Mode::IgnoreKeypadWithNumlock),
            modify_other_keys_state_2: t.flags.modify_other_keys_2,
            kitty_flags: t.screens.active.kitty_keyboard.current(),

            // These can't be known from the terminal state.
            macos_option_as_alt: OptionAsAlt::False,
        }
    }
}

/// Encode the key event to the writer in the proper format given
/// the options. For example, this will properly encode a key press
/// such as "ctrl+A" to Kitty format if Kitty encoding is enabled.
///
/// Not all key events will result in output. It is up to the caller
/// to use a writer that can track whether any output was written if
/// they care about that.
pub fn encode<W: Write + ?Sized>(
    writer: &mut W,
    event: KeyEvent<'_>,
    opts: Options,
) -> io::Result<()> {
    //std.log.warn("KEYENCODER event={} opts={}", .{ event, opts });
    if opts.kitty_flags.int() != 0 {
        kitty(writer, event, opts)
    } else {
        legacy(writer, event, opts)
    }
}

/// Decode `bytes` as UTF-8 (Zig: `std.unicode.Utf8View.init`), returning
/// an iterator over the codepoints, or `None` if the bytes are not valid
/// UTF-8.
fn utf8_view(bytes: &[u8]) -> Option<impl Iterator<Item = u32> + '_> {
    std::str::from_utf8(bytes)
        .ok()
        .map(|s| s.chars().map(|c| c as u32))
}

/// Perform Kitty keyboard protocol encoding of the key event.
fn kitty<W: Write + ?Sized>(writer: &mut W, event: KeyEvent<'_>, opts: Options) -> io::Result<()> {
    // This should never happen but we'll check anyway.
    if opts.kitty_flags.int() == 0 {
        return legacy(writer, event, opts);
    }

    // We only processed "press" events unless report events is active
    if event.action == key::Action::Release {
        if !opts.kitty_flags.report_events {
            return Ok(());
        }

        // Enter, backspace, and tab do not report release events unless "report
        // all" is set
        if !opts.kitty_flags.report_all {
            match event.key {
                Key::Enter | Key::Backspace | Key::Tab => return Ok(()),
                _ => {}
            }
        }
    }

    let all_mods = event.mods;
    let effective_mods = event.effective_mods();
    let binding_mods = effective_mods.binding();

    // Find the entry for this key in the kitty table.
    let entry_: Option<KittyEntry> = 'entry: {
        // Functional or predefined keys
        for entry in KITTY_ENTRIES.iter() {
            if entry.key == event.key {
                break 'entry Some(*entry);
            }
        }

        // Otherwise, we use our unicode codepoint from UTF8. We
        // always use the unshifted value.
        if event.unshifted_codepoint > 0 {
            break 'entry Some(KittyEntry {
                key: event.key,
                code: event.unshifted_codepoint,
                final_: b'u',
                modifier: false,
            });
        }

        None
    };

    'preprocessing: {
        // When composing, the only keys sent are plain modifiers.
        if event.composing {
            if let Some(entry) = entry_ {
                if entry.modifier {
                    break 'preprocessing;
                }
            }

            return Ok(());
        }

        // IME confirmation still sends an enter key so if we have enter
        // and UTF8 text we just send it directly since we assume that is
        // what's happening. See legacy()'s similar logic for more details
        // on how to verify this.
        if !event.utf8.is_empty() {
            match event.key {
                Key::Enter | Key::Backspace => {
                    // See legacy for why we handle this way.
                    if !is_control_utf8(event.utf8) {
                        if event.key == Key::Backspace {
                            return Ok(());
                        }
                        return writer.write_all(event.utf8);
                    }
                }
                _ => {}
            }
        }

        // If we're reporting all then we always send CSI sequences.
        if !opts.kitty_flags.report_all {
            // Quote:
            // The only exceptions are the Enter, Tab and Backspace keys which
            // still generate the same bytes as in legacy mode this is to allow the
            // user to type and execute commands in the shell such as reset after a
            // program that sets this mode crashes without clearing it.
            //
            // Quote ("report all" mode):
            // Note that all keys are reported as escape codes, including Enter,
            // Tab, Backspace etc.
            if binding_mods.empty() {
                match event.key {
                    Key::Enter => return writer.write_all(b"\r"),
                    Key::Tab => return writer.write_all(b"\t"),
                    Key::Backspace => return writer.write_all(&[0x7F]),
                    _ => {}
                }
            }

            // Send plain-text non-modified text directly to the terminal.
            // We don't send release events because those are specially encoded.
            if !event.utf8.is_empty()
                && binding_mods.empty()
                && event.action != key::Action::Release
            {
                'plain_text: {
                    // We only do this for printable characters. We should
                    // inspect the real unicode codepoint properties here but
                    // the real world issue is usually control characters.
                    let Some(it) = utf8_view(event.utf8) else {
                        // Invalid UTF-8 so let's fallback to encoding the
                        // key press as if it didn't produce UTF-8 text. I'm
                        // not sure what should happen here according to the spec,
                        // since it doesn't specify this behavior. Presumably
                        // this is a caller bug.
                        break 'plain_text;
                    };
                    for cp in it {
                        if is_control(cp) {
                            break 'plain_text;
                        }
                    }

                    return writer.write_all(event.utf8);
                }
            }
        }
    }

    let Some(entry) = entry_ else {
        // No entry found. If we have UTF-8 text this is a pure text event
        // (e.g. composed/IME text), so send it as-is so programs can
        // still receive it. Release events never insert text, same as the
        // plain-text path above.
        if event.action == key::Action::Release {
            return Ok(());
        }
        if event.utf8.is_empty() {
            return Ok(());
        }
        return writer.write_all(event.utf8);
    };

    // If this is just a modifier we require "report all" to send the sequence.
    if entry.modifier && !opts.kitty_flags.report_all {
        return Ok(());
    }

    let seq: KittySequence<'_> = {
        let mut seq = KittySequence {
            key: entry.code,
            final_: entry.final_,
            mods: KittyMods::from_input(event.action, event.key, all_mods),
            ..Default::default()
        };

        if opts.kitty_flags.report_events {
            seq.event = match event.action {
                key::Action::Press => KittySequenceEvent::Press,
                key::Action::Release => KittySequenceEvent::Release,
                key::Action::Repeat => KittySequenceEvent::Repeat,
            };
        }

        if opts.kitty_flags.report_alternates {
            'alternates: {
                // Break early if this is a control key
                if is_control(seq.key) {
                    break 'alternates;
                }

                let Some(mut it) = utf8_view(event.utf8) else {
                    // Assume invalid UTF-8 means no UTF-8.
                    break 'alternates;
                };

                // If we have a codepoint in our UTF-8 sequence, then we can
                // report the shifted version.
                if let Some(cp1) = it.next() {
                    // Set the first alternate (shifted version)
                    if cp1 != seq.key && seq.mods.shift {
                        seq.alternates[0] = Some(cp1);
                    }

                    // We want to know if there are additional codepoints because
                    // our logic below depends on the utf8 being a single codepoint.
                    let has_cp2 = it.next().is_some();

                    // Set the base layout key. We only report this if this codepoint
                    // differs from our pressed key.
                    if let Some(base) = event.key.codepoint() {
                        if base != seq.key && (cp1 != base && !has_cp2) {
                            seq.alternates[1] = Some(base);
                        }
                    }
                } else {
                    // No UTF-8 so we can't report a shifted key but we can still
                    // report a base layout key.
                    if let Some(base) = event.key.codepoint() {
                        if base != seq.key {
                            seq.alternates[1] = Some(base);
                        }
                    }
                }
            }
        }

        if opts.kitty_flags.report_associated && seq.event != KittySequenceEvent::Release {
            'associated: {
                // Determine if the Alt modifier should be treated as an actual
                // modifier (in which case it prevents associated text) or as
                // the macOS Option key, which does not prevent associated text.
                let alt_prevents_text = if cfg!(target_os = "macos") {
                    match opts.macos_option_as_alt {
                        OptionAsAlt::Left => all_mods.sides.alt == ModSide::Left,
                        OptionAsAlt::Right => all_mods.sides.alt == ModSide::Right,
                        OptionAsAlt::True => true,
                        OptionAsAlt::False => false,
                    }
                } else {
                    true
                };

                if seq.mods.prevents_text(alt_prevents_text) {
                    break 'associated;
                }

                seq.text = event.utf8;
            }
        }

        seq
    };

    seq.encode(writer)
}

/// Perform legacy encoding of the key event. "Legacy" in this case
/// is referring to the behavior of traditional terminals, plus
/// xterm's `modifyOtherKeys`, plus Paul Evans's "fixterms" spec.
/// These together combine the legacy protocol because they're all
/// meant to be extensions that do not change any existing behavior
/// and therefore safe to combine.
fn legacy<W: Write + ?Sized>(writer: &mut W, event: KeyEvent<'_>, opts: Options) -> io::Result<()> {
    let all_mods = event.mods;
    let effective_mods = event.effective_mods();
    let binding_mods = effective_mods.binding();

    // Legacy encoding only does press/repeat
    if event.action != key::Action::Press && event.action != key::Action::Repeat {
        return Ok(());
    }

    // If we're in a dead key state then we never emit a sequence.
    if event.composing {
        return Ok(());
    }

    // If we match a PC style function key then that is our result.
    if let Some(sequence) = pc_style_function_key(
        event.key,
        all_mods,
        opts.cursor_key_application,
        opts.keypad_key_application,
        opts.ignore_keypad_with_numlock,
        opts.modify_other_keys_state_2,
        opts.backarrow_key_mode,
    ) {
        'pc_style: {
            // If we have UTF-8 text, then we never emit PC style function
            // keys. Many function keys (escape, enter, backspace) have
            // a specific meaning when dead keys are active and so we don't
            // want to send that to the terminal. Examples:
            //
            //   - Japanese: escape clears the dead key state
            //   - Korean: escape commits the dead key state
            //   - Korean: backspace should delete a single preedit char
            //
            if !event.utf8.is_empty() {
                match event.key {
                    Key::Backspace | Key::Enter | Key::Escape => {
                        // We want to ignore control characters. This is because
                        // some apprts (macOS) will send control characters as
                        // UTF-8 encodings and we handle that manually.
                        if !is_control_utf8(event.utf8) {
                            // Backspace encodes nothing because we modified IME.
                            // Enter/escape don't encode the PC-style encoding
                            // because we want to encode committed text.
                            if event.key == Key::Backspace {
                                return Ok(());
                            }
                            break 'pc_style;
                        }
                    }
                    _ => {}
                }
            }

            return writer.write_all(sequence);
        }
    }

    // In modify other keys state 2, we send the CSI 27 sequence
    // for any char with a modifier. This must happen before converting
    // Ctrl+<char> to a C0 byte because mode 2 encodes those keys, too.
    let utf8 = event.utf8;
    if opts.modify_other_keys_state_2 {
        'modify_other: {
            let Some(mut it) = utf8_view(utf8) else {
                // Assume invalid UTF-8 means we no UTF-8.
                break 'modify_other;
            };
            let Some(codepoint) = it.next() else {
                break 'modify_other;
            };

            // We only do this if we have a single codepoint. There shouldn't
            // ever be a multi-codepoint sequence that triggers this.
            if it.next().is_some() {
                break 'modify_other;
            }

            // The mods we encode for this are just the binding mods (shift, ctrl,
            // super, alt unless it is actually option).
            let mods = {
                let mut mods_binding = event.mods.binding();
                if cfg!(any(target_os = "macos", target_os = "ios")) {
                    let keep_alt = match opts.macos_option_as_alt {
                        OptionAsAlt::False => false,
                        OptionAsAlt::True => true,
                        OptionAsAlt::Left => event.mods.sides.alt == ModSide::Left,
                        OptionAsAlt::Right => event.mods.sides.alt == ModSide::Right,
                    };
                    if !keep_alt {
                        mods_binding.alt = false;
                    }
                }
                mods_binding
            };

            // This copies xterm's `ModifyOtherKeys` function that returns
            // whether modify other keys should be encoded for the given
            // input.
            let should_modify = 'should_modify: {
                // xterm IsControlInput
                if (0x40..=0x7F).contains(&codepoint) {
                    break 'should_modify true;
                }

                // If we have anything other than shift pressed, encode.
                let mut mods_no_shift = mods;
                mods_no_shift.shift = false;
                if !mods_no_shift.empty() {
                    break 'should_modify true;
                }

                // We only have shift pressed. We only allow space.
                if codepoint == ' ' as u32 {
                    break 'should_modify true;
                }

                // This logic isn't complete but I don't fully understand
                // the rest so I'm going to wait until we can have a
                // reasonable test scenario.
                false
            };

            if should_modify {
                for (i, modset) in function_keys::MODIFIERS.iter().enumerate() {
                    let code = i + 2;
                    if !mods.equal(*modset) {
                        continue;
                    }
                    return write!(writer, "\x1B[27;{};{}~", code, codepoint);
                }
            }
        }
    }

    // If we match a control sequence, we output that directly. For
    // ctrlSeq we have to use all mods because we want it to only
    // match ctrl+<char>.
    if let Some(char) = ctrl_seq(event.key, event.utf8, event.unshifted_codepoint, all_mods) {
        // C0 sequences support alt-as-esc prefixing.
        if binding_mods.alt {
            writer.write_all(&[0x1B])?;
            writer.write_all(&[char])?;
            return Ok(());
        }

        writer.write_all(&[char])?;
        return Ok(());
    }

    // If we have no UTF8 text then the only possibility is the
    // alt-prefix handling of unshifted codepoints... so we process that.
    if utf8.is_empty() {
        let _ = legacy_alt_prefix(writer, event, binding_mods, all_mods, opts)?;
        return Ok(());
    }

    // Let's see if we should apply fixterms to this codepoint.
    // At this stage of key processing, we only need to apply fixterms
    // to unicode codepoints if we have ctrl set.
    if event.mods.ctrl {
        'csiu: {
            // Important: we want to use the original mods here, not the
            // effective mods. The fixterms spec states the shifted chars
            // should be sent uppercase but Kitty changes that behavior
            // so we'll send all the mods.
            let (csi_u_mods, char) = {
                let mut mods = CsiUMods::from_input(event.mods);

                // Get our codepoint. If we have more than one codepoint this
                // can't be valid CSIu.
                let Some(mut it) = utf8_view(event.utf8) else {
                    break 'csiu;
                };
                let Some(mut char) = it.next() else {
                    break 'csiu;
                };
                if it.next().is_some() {
                    break 'csiu;
                }

                // If our character is A to Z and we have shift set, then
                // we lowercase it. This is a Kitty-specific behavior that
                // we choose to follow and diverge from the fixterms spec.
                // This makes it easier for programs to detect shifted letters
                // for keybindings and is not just theoretical but used by
                // real programs.
                if (b'A' as u32..=b'Z' as u32).contains(&char) && mods.shift {
                    // We want to rely on apprt to send us the correct
                    // unshifted codepoint...
                    char = (char as u8).to_ascii_lowercase() as u32;
                }

                // If our unshifted codepoint is identical to the shifted
                // then we consider shift. Otherwise, we do not because the
                // shift key was used to obtain the character. This is specified
                // by fixterms.
                if event.unshifted_codepoint != char {
                    mods.shift = false;
                }

                (mods, char)
            };
            return write!(writer, "\x1B[{};{}u", char, csi_u_mods.seq_int());
        }
    }

    // If we have alt-pressed and alt-esc-prefix is enabled, then
    // we need to prefix the utf8 sequence with an esc.
    if legacy_alt_prefix(writer, event, binding_mods, all_mods, opts)? {
        return Ok(());
    }

    // If we are on macOS, command+keys do not encode text. It isn't
    // typical for command+keys on macOS to ever encode text. They
    // don't in native text inputs (i.e. TextEdit) and they also don't
    // in other native terminals (Terminal.app officially but also
    // iTerm2).
    //
    // For Linux, we continue to encode text because it is typical.
    // For example on Gnome Console Super+b will encode a "b" character
    // with legacy encoding.
    if cfg!(target_os = "macos") && all_mods.super_ {
        return Ok(());
    }

    writer.write_all(utf8)
}

fn legacy_alt_prefix<W: Write + ?Sized>(
    writer: &mut W,
    event: KeyEvent<'_>,
    binding_mods: Mods,
    mods: Mods,
    opts: Options,
) -> io::Result<bool> {
    // This only takes effect with alt pressed
    if !binding_mods.alt || !opts.alt_esc_prefix {
        return Ok(false);
    }

    // On macOS, we only handle option like alt in certain
    // circumstances. Otherwise, macOS does a unicode translation
    // and we allow that to happen.
    if cfg!(target_os = "macos") {
        match opts.macos_option_as_alt {
            OptionAsAlt::False => return Ok(false),
            OptionAsAlt::Left => {
                if mods.sides.alt == ModSide::Right {
                    return Ok(false);
                }
            }
            OptionAsAlt::Right => {
                if mods.sides.alt == ModSide::Left {
                    return Ok(false);
                }
            }
            OptionAsAlt::True => {}
        }
    }

    // A single byte is already the exact text we want to prefix. In
    // particular, this preserves shifted ASCII punctuation.
    let utf8 = event.utf8;
    if utf8.len() == 1 {
        writer.write_all(&[0x1B])?;
        writer.write_all(utf8)?;
        return Ok(true);
    }

    let mut unshifted_buf = [0u8; 4];
    let value: &[u8] = 'value: {
        // On macOS, Option may translate the text into a different Unicode
        // value. When Option is configured as Alt, use the physical key's
        // unshifted codepoint just as the single-byte implementation did.
        if cfg!(target_os = "macos") && event.unshifted_codepoint > 0 {
            let Some(c) = char::from_u32(event.unshifted_codepoint) else {
                return Ok(false);
            };
            break 'value c.encode_utf8(&mut unshifted_buf).as_bytes();
        }

        // Outside of the macOS translation case, prefix the complete UTF-8
        // text rather than truncating it to a single byte.
        if !utf8.is_empty() {
            break 'value utf8;
        }

        // Frontends may omit UTF-8 while still supplying the physical key's
        // unshifted codepoint. Encode the complete scalar in that case.
        if event.unshifted_codepoint > 0 {
            let Some(c) = char::from_u32(event.unshifted_codepoint) else {
                return Ok(false);
            };
            break 'value c.encode_utf8(&mut unshifted_buf).as_bytes();
        }

        return Ok(false);
    };

    writer.write_all(&[0x1B])?;
    writer.write_all(value)?;
    Ok(true)
}

/// Error returned by [`copy_to_buf`] (Zig: `error.OutOfMemory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutOfMemory;

/// A helper to memcpy a src value to a buffer and return the result.
#[allow(dead_code)]
fn copy_to_buf<'a>(buf: &'a mut [u8], src: &[u8]) -> Result<&'a [u8], OutOfMemory> {
    if src.len() > buf.len() {
        return Err(OutOfMemory);
    }
    let result = &mut buf[..src.len()];
    result.copy_from_slice(src);
    Ok(result)
}

/// Determines whether the key should be encoded in the xterm
/// "PC-style Function Key" syntax (roughly). This is a hardcoded
/// table of keys and modifiers that result in a specific sequence.
fn pc_style_function_key(
    keyval: Key,
    mods: Mods,
    cursor_key_application: bool,
    keypad_key_application_req: bool,
    ignore_keypad_with_numlock: bool,
    modify_other_keys: bool, // True if state 2
    backarrow_key_mode: bool,
) -> Option<&'static [u8]> {
    // We only want binding-sensitive mods because lock keys
    // and directional modifiers (left/right) don't matter for
    // pc-style function keys.
    let mods_int = mods.binding().int();

    // Keypad application keymode isn't super straightforward.
    // On xterm, in VT220 mode, numlock alone is enough to trigger
    // application mode. But in more modern modes, numlock is
    // ignored by default via mode 1035 (default true). If mode
    // 1035 is on, we always are in numerical keypad mode. If
    // mode 1035 is off, we are in application mode if the
    // proper numlock state is pressed. The numlock state is implicitly
    // determined based on the keycode sent (i.e. 1 with numlock
    // on will be kp_end).
    let keypad_key_application = {
        // If we're ignoring keypad then this is always false.
        // In other words, we're always in numerical keypad mode.
        if ignore_keypad_with_numlock {
            false
        } else {
            // If we're not ignoring then we enable the desired state.
            keypad_key_application_req
        }
    };

    for entry in function_keys::KEYS.get(keyval) {
        match entry.cursor {
            CursorMode::Any => {}
            CursorMode::Normal => {
                if cursor_key_application {
                    continue;
                }
            }
            CursorMode::Application => {
                if !cursor_key_application {
                    continue;
                }
            }
        }

        match entry.keypad {
            KeypadMode::Any => {}
            KeypadMode::Normal => {
                if keypad_key_application {
                    continue;
                }
            }
            KeypadMode::Application => {
                if !keypad_key_application {
                    continue;
                }
            }
        }

        match entry.modify_other_keys {
            ModifyKeys::Any => {}
            ModifyKeys::Set => {
                if modify_other_keys {
                    continue;
                }
            }
            ModifyKeys::SetOther => {
                if !modify_other_keys {
                    continue;
                }
            }
        }

        let entry_mods_int = entry.mods.int();
        if entry_mods_int == 0 {
            if mods_int != 0 && !entry.mods_empty_is_any {
                continue;
            }
            // mods are either empty, or empty means any so we allow it.
        } else if entry_mods_int != mods_int {
            // any set mods require an exact match
            continue;
        }

        if backarrow_key_mode {
            if let Some(sequence) = entry.sequence_decbkm {
                return Some(sequence);
            }
        }

        return Some(entry.sequence);
    }

    None
}

/// Returns the C0 byte for the key event if it should be used.
/// This converts a key event into the expected terminal behavior
/// such as Ctrl+C turning into 0x03, amongst many other translations.
///
/// This will return null if the key event should not be converted
/// into a C0 byte. There are many cases for this and you should read
/// the source code to understand them.
fn ctrl_seq(logical_key: Key, utf8: &[u8], unshifted_codepoint: u32, mods: Mods) -> Option<u8> {
    const CTRL_ONLY: u16 = Mods {
        ctrl: true,
        ..Mods::EMPTY
    }
    .int();

    // If ctrl is not pressed then we never do anything.
    if !mods.ctrl {
        return None;
    }

    let (char, unset_mods) = {
        // We need to only get binding modifiers so we strip lock
        // keys, sides, etc.
        let mut unset_mods = mods.binding();

        // Remove alt from our modifiers because it does not impact whether
        // we are generating a ctrl sequence and we handle the ESC-prefix
        // logic separately.
        unset_mods.alt = false;

        let mut char: u8 = 'char: {
            // If we have exactly one UTF8 byte, we assume that is the
            // character we want to convert to a C0 byte.
            if utf8.len() == 1 {
                break 'char utf8[0];
            }

            // If we have a logical key that maps to a single byte
            // printable character, we use that. History to explain this:
            // this was added to support cyrillic keyboard layouts such
            // as Russian and Mongolian. These layouts have a `c` key that
            // maps to U+0441 (cyrillic small letter "c") but every
            // terminal I've tested encodes this as ctrl+c.
            if let Some(cp) = logical_key.codepoint() {
                if let Ok(byte) = u8::try_from(cp) {
                    // For this specific case, we only map to the key if
                    // we have exactly ctrl pressed. This is because shift
                    // would modify the key and we don't know how to do that
                    // properly here (don't have the layout). And we want
                    // to encode shift as CSIu.
                    if unset_mods.int() != CTRL_ONLY {
                        return None;
                    }
                    break 'char byte;
                }
            }

            // Otherwise we don't have a character to convert that
            // we can reliably map to a C0 byte.
            return None;
        };

        // Remove shift if we have something outside of the US letter
        // range. This is so that characters such as `ctrl+shift+-`
        // generate the correct ctrl-seq (used by emacs).
        if unset_mods.shift && !(b'A'..=b'Z').contains(&char) {
            // Special case for fixterms awkward case as specified.
            if char != b'@' {
                unset_mods.shift = false;
            }
        }

        // If the character is uppercase, we convert it to lowercase. We
        // rely on the unshifted codepoint to do this. This handles
        // the scenario where we have caps lock pressed. Note that
        // shifted characters are handled above, if we are just pressing
        // shift then the ctrl-only check will fail later and we won't
        // ctrl-seq encode.
        if (b'A'..=b'Z').contains(&char) && unshifted_codepoint > 0 {
            if let Ok(byte) = u8::try_from(unshifted_codepoint) {
                char = byte;
            }
        }

        // An additional note on caps lock and shift interaction.
        // If we have caps lock set and an ASCII letter is pressed,
        // we lowercase it (above). If we have only control pressed,
        // we process it as a ctrl seq. For example ctrl+M with caps
        // lock but no shift will encode as 0x0D.
        //
        // But, if you press ctrl+shift+m, this will not encode as a
        // ctrl-seq and falls through to CSIu encoding. This lets programs
        // detect the difference between ctrl+M and ctrl+shift+M. This
        // diverges from the fixterms "spec" and most terminals. This
        // only matches Kitty in behavior. But I believe this is a
        // justified divergence because it's a useful distinction.

        (char, unset_mods)
    };

    // After unsetting, we only continue if we have ONLY control set.
    if unset_mods.int() != CTRL_ONLY {
        return None;
    }

    // From Kitty's key encoding logic. I tried to discern the exact
    // behavior across different terminals but it's not clear, so I'm
    // just going to repeat what Kitty does.
    match char {
        b' ' => Some(0),
        b'/' => Some(31),
        b'0' => Some(48),
        b'1' => Some(49),
        b'2' => Some(0),
        b'3' => Some(27),
        b'4' => Some(28),
        b'5' => Some(29),
        b'6' => Some(30),
        b'7' => Some(31),
        b'8' => Some(127),
        b'9' => Some(57),
        b'?' => Some(127),
        b'@' => Some(0),
        b'\\' => Some(28),
        b']' => Some(29),
        b'^' => Some(30),
        b'_' => Some(31),
        b'a' => Some(1),
        b'b' => Some(2),
        b'c' => Some(3),
        b'd' => Some(4),
        b'e' => Some(5),
        b'f' => Some(6),
        b'g' => Some(7),
        b'h' => Some(8),
        b'j' => Some(10),
        b'k' => Some(11),
        b'l' => Some(12),
        b'n' => Some(14),
        b'o' => Some(15),
        b'p' => Some(16),
        b'q' => Some(17),
        b'r' => Some(18),
        b's' => Some(19),
        b't' => Some(20),
        b'u' => Some(21),
        b'v' => Some(22),
        b'w' => Some(23),
        b'x' => Some(24),
        b'y' => Some(25),
        b'z' => Some(26),
        b'~' => Some(30),

        // These are purposely NOT handled here because of the fixterms
        // specification: https://www.leonerd.org.uk/hacks/fixterms/
        // These are processed as CSI u.
        // 'i' => 0x09,
        // 'm' => 0x0D,
        // '[' => 0x1B,
        _ => None,
    }
}

/// Returns true if this is an ASCII control character, matches libc implementation.
fn is_control(cp: u32) -> bool {
    cp < 0x20 || cp == 0x7F
}

/// Returns true if this string is comprised of a single
/// control character. This returns false for multi-byte strings.
fn is_control_utf8(str: &[u8]) -> bool {
    str.len() == 1 && is_control(str[0] as u32)
}

/// This is the bitmask for fixterm CSI u modifiers.
///
/// (Zig: `packed struct(u3)`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CsiUMods {
    shift: bool,
    alt: bool,
    ctrl: bool,
}

impl CsiUMods {
    /// Convert an input mods value into the CSI u mods value.
    fn from_input(mods: Mods) -> CsiUMods {
        CsiUMods {
            shift: mods.shift,
            alt: mods.alt,
            ctrl: mods.ctrl,
        }
    }

    /// Returns the raw int value of this packed struct.
    fn int(self) -> u8 {
        (self.shift as u8) | ((self.alt as u8) << 1) | ((self.ctrl as u8) << 2)
    }

    /// Returns the integer value sent as part of the CSI u sequence.
    /// This adds 1 to the bitmask value as described in the spec.
    fn seq_int(self) -> u8 {
        self.int() + 1
    }
}

/// This is the bitfields for Kitty modifiers.
///
/// (Zig: `packed struct(u8)`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct KittyMods {
    shift: bool,
    alt: bool,
    ctrl: bool,
    super_: bool,
    hyper: bool,
    meta: bool,
    caps_lock: bool,
    num_lock: bool,
}

impl KittyMods {
    /// Convert an input mods value into the CSI u mods value.
    fn from_input(action: key::Action, k: Key, mods: Mods) -> KittyMods {
        let _ = action;
        let _ = k;
        KittyMods {
            shift: mods.shift,
            alt: mods.alt,
            ctrl: mods.ctrl,
            super_: mods.super_,
            caps_lock: mods.caps_lock,
            num_lock: mods.num_lock,
            ..Default::default()
        }
    }

    /// Returns true if the modifiers prevent printable text.
    ///
    /// The alt_prevents_text parameter determines whether or not the Alt
    /// modifier prevents printable text. On Linux, this is always true. On
    /// macOS, this is only true if macos-option-as-alt is set.
    fn prevents_text(self, alt_prevents_text: bool) -> bool {
        (self.alt && alt_prevents_text) || self.ctrl || self.super_ || self.hyper || self.meta
    }

    /// Returns the raw int value of this packed struct.
    fn int(self) -> u8 {
        (self.shift as u8)
            | ((self.alt as u8) << 1)
            | ((self.ctrl as u8) << 2)
            | ((self.super_ as u8) << 3)
            | ((self.hyper as u8) << 4)
            | ((self.meta as u8) << 5)
            | ((self.caps_lock as u8) << 6)
            | ((self.num_lock as u8) << 7)
    }

    /// Returns the integer value sent as part of the Kitty sequence.
    /// This adds 1 to the bitmask value as described in the spec.
    fn seq_int(self) -> u16 {
        self.int() as u16 + 1
    }
}

/// Values for the event code (see "event-type" in [`KittySequence`]).
/// Note that Kitty omits the ":1" for the press event but other
/// terminals include it. We'll include it.
///
/// (Zig: `KittySequence.Event`, `enum(u2)`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
enum KittySequenceEvent {
    #[default]
    None = 0,
    Press = 1,
    Repeat = 2,
    Release = 3,
}

/// Represents a kitty key sequence and has helpers for encoding it.
/// The sequence from the Kitty specification:
///
/// CSI unicode-key-code:alternate-key-codes ; modifiers:event-type ; text-as-codepoints u
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct KittySequence<'a> {
    /// (Zig: `u21`.)
    key: u32,
    final_: u8,
    mods: KittyMods,
    event: KittySequenceEvent,
    alternates: [Option<u32>; 2],
    text: &'a [u8],
}

impl KittySequence<'_> {
    fn encode<W: Write + ?Sized>(&self, writer: &mut W) -> io::Result<()> {
        if self.final_ == b'u' || self.final_ == b'~' {
            return self.encode_full(writer);
        }
        self.encode_special(writer)
    }

    fn encode_full<W: Write + ?Sized>(&self, writer: &mut W) -> io::Result<()> {
        // Key section
        write!(writer, "\x1B[{}", self.key)?;
        // Write our alternates
        if let Some(shifted) = self.alternates[0] {
            write!(writer, ":{}", shifted)?;
        }
        if let Some(base) = self.alternates[1] {
            if self.alternates[0].is_none() {
                write!(writer, "::{}", base)?;
            } else {
                write!(writer, ":{}", base)?;
            }
        }

        // Mods and events section
        let mods = self.mods.seq_int();
        let mut emit_prior = false;
        if self.event != KittySequenceEvent::None && self.event != KittySequenceEvent::Press {
            write!(writer, ";{}:{}", mods, self.event as u8)?;
            emit_prior = true;
        } else if mods > 1 {
            write!(writer, ";{}", mods)?;
            emit_prior = true;
        }

        // Text section
        if !self.text.is_empty() {
            // Assume invalid UTF-8 means we have no text.
            if let Some(it) = utf8_view(self.text) {
                let mut count: usize = 0;
                for cp in it {
                    // If the codepoint is non-printable ASCII character, skip.
                    if is_control(cp) {
                        continue;
                    }

                    // We need to add our ";". We need to add two if we didn't emit
                    // the modifier section. We only do this initially.
                    if count == 0 {
                        if !emit_prior {
                            writer.write_all(b";")?;
                        }
                        writer.write_all(b";")?;
                    } else {
                        writer.write_all(b":")?;
                    }

                    write!(writer, "{}", cp)?;
                    count += 1;
                }
            }
        }

        writer.write_all(&[self.final_])
    }

    fn encode_special<W: Write + ?Sized>(&self, writer: &mut W) -> io::Result<()> {
        let mods = self.mods.seq_int();
        if self.event != KittySequenceEvent::None {
            write!(writer, "\x1B[1;{}:{}", mods, self.event as u8)?;
            return writer.write_all(&[self.final_]);
        }

        if mods > 1 {
            write!(writer, "\x1B[1;{}", mods)?;
            return writer.write_all(&[self.final_]);
        }

        writer.write_all(b"\x1B[")?;
        writer.write_all(&[self.final_])
    }
}
