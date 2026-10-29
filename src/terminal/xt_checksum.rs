//! Port of `src/terminal/xt_checksum.zig`.
//!
//! STATUS: partial — `Flags`, `Request` (without `Request.selection`),
//! `MAX_ENCODE_SIZE` and `encode` are ported. `Request.selection`,
//! `compute` (and its private helpers `value`, `attributes`, `extended`)
//! and the tests that need a `Terminal` are pending the PageList, Screen,
//! Selection and Terminal ports.
//!
//! DECRQCRA (Request Checksum of Rectangular Area) and XTCHECKSUM
//! (select the checksum variant DECRQCRA computes).
//!
//! The checksum follows xterm's `xtermCheckRect`. No real VT420 was
//! available to compare against while this was written, so xterm is the
//! reference:
//!
//! - `xtermCheckRect`: https://github.com/ThomasDickey/xterm-snapshots/blob/xterm-411/screen.c#L3162-L3290
//! - `xtermCharSetDec`: https://github.com/ThomasDickey/xterm-snapshots/blob/xterm-411/charsets.c#L608

use std::io;

/// The XTCHECKSUM (CSI Ps # y) bits. The zero value is the DEC checksum,
/// which is also what a full reset restores.
///
/// These are xterm's `CSBITS`, which it also takes from the
/// `checksumExtension` resource:
///
/// - `CSBITS`: https://github.com/ThomasDickey/xterm-snapshots/blob/xterm-411/ptyx.h#L488-L496
/// - XTCHECKSUM: https://github.com/ThomasDickey/xterm-snapshots/blob/xterm-411/charproc.c#L6044-L6050
/// - Reset to `checksumExtension`: https://github.com/ThomasDickey/xterm-snapshots/blob/xterm-411/charproc.c#L14424-L14426
/// - `checksumExtension` docs: https://github.com/ThomasDickey/xterm-snapshots/blob/xterm-411/xterm.man#L2870-L2896
/// - XTCHECKSUM docs: https://github.com/ThomasDickey/xterm-snapshots/blob/xterm-411/ctlseqs.ms#L2562-L2569
///
/// xterm's documentation disagrees with its code in two places, and we
/// follow the code. The docs describe bit 3 as "omit checksum for cells
/// not explicitly initialized", but `csDRAWN` does the opposite: without
/// it those cells are skipped, and with it they count as spaces. The man
/// page also lists a bit 5, "do not mask cell value to 7 bits", which
/// `xtermCheckRect` never looks at.
///
/// `packed struct(u5)`: bit 0 is `positive`, then `no_attributes`,
/// `no_trim`, `undrawn`, `full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Flags {
    /// Don't negate the result.
    pub positive: bool,

    /// Don't add the VT100 video attributes to each cell.
    pub no_attributes: bool,

    /// Don't omit blanks. The DEC checksum only counts a plain space if it
    /// is the first cell of the rectangle.
    pub no_trim: bool,

    /// Count cells that were never written to as spaces instead of
    /// skipping them.
    pub undrawn: bool,

    /// Use the full codepoint rather than the DEC 8-bit value. Wide
    /// spacers are skipped and, as in xterm, combining marks are not
    /// counted in this mode.
    pub full: bool,
}

impl Flags {
    /// Unpack from the u5 backing integer (`@bitCast`); higher bits are
    /// ignored.
    pub const fn from_bits(v: u8) -> Flags {
        Flags {
            positive: v & 0b1 != 0,
            no_attributes: v & 0b10 != 0,
            no_trim: v & 0b100 != 0,
            undrawn: v & 0b1000 != 0,
            full: v & 0b10000 != 0,
        }
    }

    /// Pack into the u5 backing integer (`@bitCast`).
    pub const fn to_bits(self) -> u8 {
        (self.positive as u8)
            | (self.no_attributes as u8) << 1
            | (self.no_trim as u8) << 2
            | (self.undrawn as u8) << 3
            | (self.full as u8) << 4
    }
}

/// A DECRQCRA request as it arrives in `CSI Pi ; Pg ; Pt ; Pl ; Pb ; Pr * y`.
/// The coordinates are 1-based and zero means the parameter was omitted.
/// The page number (Pg) is ignored since we only have one page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Request {
    pub id: u16,
    pub top: u16,
    pub left: u16,
    pub bottom: u16,
    pub right: u16,
}

// TODO(port): `Request.selection(pages, origin)` and `compute(screen, sel,
// flags)` need `PageList`, `Screen`, `Selection` and
// `Terminal.ScrollingRegion`, which are not ported yet.

/// The largest reply `encode` writes.
pub const MAX_ENCODE_SIZE: usize = b"\x1bP65535!~FFFF\x1b\\".len();

/// Encode the DECRQCRA reply, `DCS Pi ! ~ XXXX ST`.
pub fn encode<W: io::Write + ?Sized>(writer: &mut W, id: u16, sum: u16) -> io::Result<()> {
    write!(writer, "\x1bP{}!~{:04X}\x1b\\", id, sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xt_checksum_encode() {
        let mut buf = [0u8; MAX_ENCODE_SIZE];
        let mut writer: &mut [u8] = &mut buf;
        encode(&mut writer, 65535, 0xFFFF).unwrap();
        let n = MAX_ENCODE_SIZE - writer.len();
        assert_eq!(b"\x1bP65535!~FFFF\x1b\\", &buf[..n]);

        let mut writer: &mut [u8] = &mut buf;
        encode(&mut writer, 1, 0x1A).unwrap();
        let n = MAX_ENCODE_SIZE - writer.len();
        assert_eq!(b"\x1bP1!~001A\x1b\\", &buf[..n]);
    }

    #[test]
    fn flags_bits_round_trip() {
        assert_eq!(Flags::default(), Flags::from_bits(0));
        assert_eq!(
            Flags {
                positive: true,
                no_trim: true,
                ..Default::default()
            },
            Flags::from_bits(5)
        );
        assert_eq!(
            Flags {
                full: true,
                ..Default::default()
            },
            Flags::from_bits(48 & 0x1F)
        );
        for v in 0..32u8 {
            assert_eq!(v, Flags::from_bits(v).to_bits());
        }
    }
}
