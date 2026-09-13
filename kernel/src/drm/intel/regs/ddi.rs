//! One block of the display engine's register table.
//!
//! Owner: [`super`].  Every offset here cites the reference section it came
//! from, and the access classification is the sequence's: a register the
//! bring-up writes is `read_write`, one it only reads back is `read_only`.

use super::{Meaning, Register};

/// `TRANS_HTOTAL(A)`, horizontal total and active width; reference section 5.3.
///
/// `[31:16]` is the total line length less one and `[15:0]` is the active width
/// less one; section 6.1 gives the arithmetic.  Transcoder A's timing block
/// starts at base `0x60000`, and every field of all six timing registers is
/// stored minus one.
pub(crate) const TRANS_HTOTAL_A: Register =
    Register::read_write("TRANS_HTOTAL(A)", 0x60_000, Meaning::BringUp, None);

/// `TRANS_HBLANK(A)`, horizontal blanking start and end; reference section 5.3.
///
/// `[31:16]` is the blanking end less one and `[15:0]` the blanking start less
/// one.  For a mode whose blanking runs to the end of the line, section 6.1
/// programs it with the same value as `TRANS_HTOTAL`.
pub(crate) const TRANS_HBLANK_A: Register =
    Register::read_write("TRANS_HBLANK(A)", 0x60_004, Meaning::BringUp, None);

/// `TRANS_HSYNC(A)`, horizontal sync start and end; reference section 5.3.
///
/// `[31:16]` is the sync end less one and `[15:0]` the sync start less one.
/// The sync *polarity* is not here: section 6.1 puts it in
/// `TRANS_DDI_FUNC_CTL`'s `PHSYNC` bit.
pub(crate) const TRANS_HSYNC_A: Register =
    Register::read_write("TRANS_HSYNC(A)", 0x60_008, Meaning::BringUp, None);

/// `TRANS_VTOTAL(A)`, vertical total and active height; reference section 5.3.
///
/// `[31:16]` is the total number of lines less one and `[15:0]` the active
/// height less one.
pub(crate) const TRANS_VTOTAL_A: Register =
    Register::read_write("TRANS_VTOTAL(A)", 0x60_00c, Meaning::BringUp, None);

/// `TRANS_VBLANK(A)`, vertical blanking start and end; reference section 5.3.
///
/// `[31:16]` is the blanking end less one and `[15:0]` the blanking start less
/// one; section 6.1 gives it the same value as `TRANS_VTOTAL`.
pub(crate) const TRANS_VBLANK_A: Register =
    Register::read_write("TRANS_VBLANK(A)", 0x60_010, Meaning::BringUp, None);

/// `TRANS_VSYNC(A)`, vertical sync start and end; reference section 5.3.
///
/// `[31:16]` is the sync end less one and `[15:0]` the sync start less one.
/// As with the horizontal sync, the polarity lives in `TRANS_DDI_FUNC_CTL`.
pub(crate) const TRANS_VSYNC_A: Register =
    Register::read_write("TRANS_VSYNC(A)", 0x60_014, Meaning::BringUp, None);

/// `TRANS_HTOTAL(B)`, horizontal total and active width; reference section 5.3.
///
/// Transcoder B's timing block starts at base `0x61000`; the other five timing
/// registers follow it at the same offsets within the block.
pub(crate) const TRANS_HTOTAL_B: Register =
    Register::read_write("TRANS_HTOTAL(B)", 0x61_000, Meaning::BringUp, None);

/// `TRANS_HBLANK(B)`, horizontal blanking start and end; reference section 5.3.
pub(crate) const TRANS_HBLANK_B: Register =
    Register::read_write("TRANS_HBLANK(B)", 0x61_004, Meaning::BringUp, None);

/// `TRANS_HSYNC(B)`, horizontal sync start and end; reference section 5.3.
pub(crate) const TRANS_HSYNC_B: Register =
    Register::read_write("TRANS_HSYNC(B)", 0x61_008, Meaning::BringUp, None);

/// `TRANS_VTOTAL(B)`, vertical total and active height; reference section 5.3.
pub(crate) const TRANS_VTOTAL_B: Register =
    Register::read_write("TRANS_VTOTAL(B)", 0x61_00c, Meaning::BringUp, None);

/// `TRANS_VBLANK(B)`, vertical blanking start and end; reference section 5.3.
pub(crate) const TRANS_VBLANK_B: Register =
    Register::read_write("TRANS_VBLANK(B)", 0x61_010, Meaning::BringUp, None);

/// `TRANS_VSYNC(B)`, vertical sync start and end; reference section 5.3.
pub(crate) const TRANS_VSYNC_B: Register =
    Register::read_write("TRANS_VSYNC(B)", 0x61_014, Meaning::BringUp, None);

/// `TRANS_HTOTAL(C)`, horizontal total and active width; reference section 5.3.
///
/// Transcoder C's timing block starts at base `0x62000`.  Section 5.1 notes the
/// transcoders are tied one-to-one to the pipes, so pipe C's mode is programmed
/// here.
pub(crate) const TRANS_HTOTAL_C: Register =
    Register::read_write("TRANS_HTOTAL(C)", 0x62_000, Meaning::BringUp, None);

/// `TRANS_HBLANK(C)`, horizontal blanking start and end; reference section 5.3.
pub(crate) const TRANS_HBLANK_C: Register =
    Register::read_write("TRANS_HBLANK(C)", 0x62_004, Meaning::BringUp, None);

/// `TRANS_HSYNC(C)`, horizontal sync start and end; reference section 5.3.
pub(crate) const TRANS_HSYNC_C: Register =
    Register::read_write("TRANS_HSYNC(C)", 0x62_008, Meaning::BringUp, None);

/// `TRANS_VTOTAL(C)`, vertical total and active height; reference section 5.3.
pub(crate) const TRANS_VTOTAL_C: Register =
    Register::read_write("TRANS_VTOTAL(C)", 0x62_00c, Meaning::BringUp, None);

/// `TRANS_VBLANK(C)`, vertical blanking start and end; reference section 5.3.
pub(crate) const TRANS_VBLANK_C: Register =
    Register::read_write("TRANS_VBLANK(C)", 0x62_010, Meaning::BringUp, None);

/// `TRANS_VSYNC(C)`, vertical sync start and end; reference section 5.3.
pub(crate) const TRANS_VSYNC_C: Register =
    Register::read_write("TRANS_VSYNC(C)", 0x62_014, Meaning::BringUp, None);

/// `TRANS_HTOTAL(D)`, horizontal total and active width; reference section 5.3.
///
/// Transcoder D's timing block starts at base `0x63000`.
pub(crate) const TRANS_HTOTAL_D: Register =
    Register::read_write("TRANS_HTOTAL(D)", 0x63_000, Meaning::BringUp, None);

/// `TRANS_HBLANK(D)`, horizontal blanking start and end; reference section 5.3.
pub(crate) const TRANS_HBLANK_D: Register =
    Register::read_write("TRANS_HBLANK(D)", 0x63_004, Meaning::BringUp, None);

/// `TRANS_HSYNC(D)`, horizontal sync start and end; reference section 5.3.
pub(crate) const TRANS_HSYNC_D: Register =
    Register::read_write("TRANS_HSYNC(D)", 0x63_008, Meaning::BringUp, None);

/// `TRANS_VTOTAL(D)`, vertical total and active height; reference section 5.3.
pub(crate) const TRANS_VTOTAL_D: Register =
    Register::read_write("TRANS_VTOTAL(D)", 0x63_00c, Meaning::BringUp, None);

/// `TRANS_VBLANK(D)`, vertical blanking start and end; reference section 5.3.
pub(crate) const TRANS_VBLANK_D: Register =
    Register::read_write("TRANS_VBLANK(D)", 0x63_010, Meaning::BringUp, None);

/// `TRANS_VSYNC(D)`, vertical sync start and end; reference section 5.3.
pub(crate) const TRANS_VSYNC_D: Register =
    Register::read_write("TRANS_VSYNC(D)", 0x63_014, Meaning::BringUp, None);
/// `TRANS_CLK_SEL(A)`, the transcoder's port-clock select; reference section 6.3.
///
/// One of the three muxes that must agree before a mode appears: it connects
/// the transcoder to a port's clock, and `TGL_TRANS_CLK_SEL_PORT(port) =
/// (port + 1) << 28` is the Gen12 encoding (port 0 would otherwise mean
/// "none").  Section 11 step 5.4 writes `(PORT_A + 1) << 28` for transcoder A
/// and the disable sequence clears it.
pub(crate) const TRANS_CLK_SEL_A: Register =
    Register::read_write("TRANS_CLK_SEL(A)", 0x4_6140, Meaning::BringUp, None);

/// `TRANS_CLK_SEL(B)`, the port-clock select for transcoder B; reference section 6.3.
///
/// Section 6.3 gives the address as `0x46140 + tran*4`, so the four transcoders
/// are consecutive dwords.
pub(crate) const TRANS_CLK_SEL_B: Register =
    Register::read_write("TRANS_CLK_SEL(B)", 0x4_6144, Meaning::BringUp, None);

/// `TRANS_CLK_SEL(C)`, the port-clock select for transcoder C; reference section 6.3.
pub(crate) const TRANS_CLK_SEL_C: Register =
    Register::read_write("TRANS_CLK_SEL(C)", 0x4_6148, Meaning::BringUp, None);

/// `TRANS_CLK_SEL(D)`, the port-clock select for transcoder D; reference section 6.3.
pub(crate) const TRANS_CLK_SEL_D: Register =
    Register::read_write("TRANS_CLK_SEL(D)", 0x4_614c, Meaning::BringUp, None);

/// `TRANS_DDI_FUNC_CTL(A)`, the transcoder-to-DDI mode select; reference section 8.4.
///
/// It picks the DDI (`TGL_TRANS_DDI_SELECT_PORT(p) = (p + 1) << 27`), the
/// output mode (`[26:24]`: HDMI = 0, DVI = 1, DP SST = 2, DP MST = 3), the bits
/// per colour (`[22:20]`), the sync polarities (bits 17 and 16) and the port
/// width (`(lanes - 1) << 1` in `[3:1]`).  Section 11 step 5.5 writes the HDMI
/// value for the first bring-up; the old values for the mode-select, polarity
/// and high-TMDS fields in an earlier draft of section 8.4 were wrong, and the
/// corrected table is the one used here.
pub(crate) const TRANS_DDI_FUNC_CTL_A: Register =
    Register::read_write("TRANS_DDI_FUNC_CTL(A)", 0x6_0400, Meaning::BringUp, None);

/// `TRANS_DDI_FUNC_CTL(B)`, the mode select for transcoder B; reference section 8.4.
///
/// Section 8.4 gives the address as `0x60400 + T*0x1000`, so each transcoder
/// has its own control register one 4 KiB block apart.
pub(crate) const TRANS_DDI_FUNC_CTL_B: Register =
    Register::read_write("TRANS_DDI_FUNC_CTL(B)", 0x6_1400, Meaning::BringUp, None);

/// `TRANS_DDI_FUNC_CTL(C)`, the mode select for transcoder C; reference section 8.4.
pub(crate) const TRANS_DDI_FUNC_CTL_C: Register =
    Register::read_write("TRANS_DDI_FUNC_CTL(C)", 0x6_2400, Meaning::BringUp, None);

/// `TRANS_DDI_FUNC_CTL(D)`, the mode select for transcoder D; reference section 8.4.
pub(crate) const TRANS_DDI_FUNC_CTL_D: Register =
    Register::read_write("TRANS_DDI_FUNC_CTL(D)", 0x6_3400, Meaning::BringUp, None);

/// `DDI_BUF_CTL(A)`, port A's DDI buffer control; reference section 8.4.
///
/// `ENABLE[31]`, `BUF_TRANS_SELECT[27:24]`, `PHY_LINK_RATE[23:20]`,
/// `PORT_WIDTH[3:1]` and `A_4_LANES[4]` are written by section 11 step 5.7;
/// `IS_IDLE[7]` is then polled, and section 11 calls it the single best "is my
/// DDI alive" bit on the chip -- it stays 1 when the DDI has no clock.  Bit 0,
/// `DDI_INIT_DISPLAY_DETECTED`, is a legacy DVI/HDMI-only presence detect
/// (section 9.4) that hotplug supersedes.
pub(crate) const DDI_BUF_CTL_A: Register =
    Register::read_write("DDI_BUF_CTL(A)", 0x6_4000, Meaning::BringUp, None);

/// `DDI_BUF_CTL(B)`, port B's DDI buffer control; reference section 8.4.
///
/// Section 8.1 explains why the table has only A and B instances: the register
/// macro distinguishes A from B with one bit, while the Type-C ports live in
/// different blocks entirely, and section 8.1's advice for a first bring-up is
/// a combo PHY port -- A or B.
pub(crate) const DDI_BUF_CTL_B: Register =
    Register::read_write("DDI_BUF_CTL(B)", 0x6_4100, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,0)`, port A translation entry 0, low dword; reference section 8.4.
///
/// The de-emphasis / balance leg of one voltage-swing entry.  `DDI_BUF_CTL`'s
/// `BUF_TRANS_SELECT[27:24]` chooses which entry the port uses; section 8.5
/// holds the values, which are data and are not in this file.
pub(crate) const DDI_BUF_TRANS_LO_A0: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,0)", 0x6_4e00, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,0)`, port A entry 0, high dword; reference section 8.4.
///
/// The vref / vswing half of the same entry, at the low dword's address `+4`.
pub(crate) const DDI_BUF_TRANS_HI_A0: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,0)", 0x6_4e04, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,1)`, port A entry 1, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_A1: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,1)", 0x6_4e08, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,1)`, port A entry 1, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_A1: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,1)", 0x6_4e0c, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,2)`, port A entry 2, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_A2: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,2)", 0x6_4e10, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,2)`, port A entry 2, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_A2: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,2)", 0x6_4e14, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,3)`, port A entry 3, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_A3: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,3)", 0x6_4e18, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,3)`, port A entry 3, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_A3: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,3)", 0x6_4e1c, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,4)`, port A entry 4, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_A4: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,4)", 0x6_4e20, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,4)`, port A entry 4, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_A4: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,4)", 0x6_4e24, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,5)`, port A entry 5, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_A5: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,5)", 0x6_4e28, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,5)`, port A entry 5, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_A5: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,5)", 0x6_4e2c, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,6)`, port A entry 6, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_A6: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,6)", 0x6_4e30, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,6)`, port A entry 6, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_A6: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,6)", 0x6_4e34, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,7)`, port A entry 7, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_A7: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,7)", 0x6_4e38, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,7)`, port A entry 7, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_A7: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,7)", 0x6_4e3c, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,8)`, port A entry 8, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_A8: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,8)", 0x6_4e40, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,8)`, port A entry 8, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_A8: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,8)", 0x6_4e44, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(A,9)`, port A entry 9, de-emphasis leg; reference section 8.4.
///
/// Entry 9 is the last one section 8.5's ADL-P/N combo DP table enumerates;
/// see the transcription notes for why the table stops here.
pub(crate) const DDI_BUF_TRANS_LO_A9: Register =
    Register::read_write("DDI_BUF_TRANS_LO(A,9)", 0x6_4e48, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(A,9)`, port A entry 9, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_A9: Register =
    Register::read_write("DDI_BUF_TRANS_HI(A,9)", 0x6_4e4c, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,0)`, port B translation entry 0, low dword; reference section 8.4.
///
/// Port B's entries start at `0x64E60`, which section 8.4 gives as
/// `0x64E60 + i*8`; the high dwords are the same `+4` as port A's.
pub(crate) const DDI_BUF_TRANS_LO_B0: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,0)", 0x6_4e60, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,0)`, port B entry 0, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B0: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,0)", 0x6_4e64, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,1)`, port B entry 1, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_B1: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,1)", 0x6_4e68, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,1)`, port B entry 1, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B1: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,1)", 0x6_4e6c, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,2)`, port B entry 2, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_B2: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,2)", 0x6_4e70, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,2)`, port B entry 2, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B2: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,2)", 0x6_4e74, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,3)`, port B entry 3, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_B3: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,3)", 0x6_4e78, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,3)`, port B entry 3, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B3: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,3)", 0x6_4e7c, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,4)`, port B entry 4, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_B4: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,4)", 0x6_4e80, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,4)`, port B entry 4, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B4: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,4)", 0x6_4e84, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,5)`, port B entry 5, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_B5: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,5)", 0x6_4e88, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,5)`, port B entry 5, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B5: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,5)", 0x6_4e8c, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,6)`, port B entry 6, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_B6: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,6)", 0x6_4e90, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,6)`, port B entry 6, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B6: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,6)", 0x6_4e94, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,7)`, port B entry 7, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_B7: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,7)", 0x6_4e98, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,7)`, port B entry 7, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B7: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,7)", 0x6_4e9c, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,8)`, port B entry 8, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_B8: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,8)", 0x6_4ea0, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,8)`, port B entry 8, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B8: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,8)", 0x6_4ea4, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_LO(B,9)`, port B entry 9, de-emphasis leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_LO_B9: Register =
    Register::read_write("DDI_BUF_TRANS_LO(B,9)", 0x6_4ea8, Meaning::BringUp, None);

/// `DDI_BUF_TRANS_HI(B,9)`, port B entry 9, vswing leg; reference section 8.4.
pub(crate) const DDI_BUF_TRANS_HI_B9: Register =
    Register::read_write("DDI_BUF_TRANS_HI(B,9)", 0x6_4eac, Meaning::BringUp, None);

/*
 * Transcription notes
 *
 * Offsets the document does not state
 * - `DDI_BUF_CTL` for the Type-C/DKL ports (TC1..TC4, numbered D..G by section
 *   8.1's enumeration): section 8.4 gives literal offsets for ports A and B
 *   only, so only those two instances are declared.  Section 8.1's advice is to
 *   bring up a combo PHY port, so nothing here needs the others.
 * - The number of `DDI_BUF_TRANS` entries a port implements: section 8.4 gives
 *   the stride (`0x64E00 + i*8` for port A, `0x64E60 + i*8` for port B, high
 *   dword at `+4`) and no count.  Entries 0..9 are declared because that is the
 *   range section 8.5's "ADL-P/N combo PHY, DP up to HBR, 10 entries" table
 *   enumerates.  `BUF_TRANS_SELECT` is four bits (`[27:24]`), and the `0x60`
 *   spacing between the two ports' bases is twelve entries, so the hardware may
 *   hold more -- but the document never says how many, so no more are declared.
 * - The HDMI translation table's values and length: section 13.1 item 12 records
 *   `icl_combo_phy_trans_hdmi` as identified but not extracted, so a combo-PHY
 *   HDMI bring-up has no sourced values to write into any entry.
 *
 * Where the document disagrees with itself, or is ambiguous
 * - `PORT_TX_DW*` group base: section 8.2 corrects an earlier draft's `+0x400`
 *   to `+0x680`, noting that `+0x400` lands in unassigned space and a swing
 *   value written there reaches no register.  This file uses `+0x680`.
 * - `TRANS_DDI_FUNC_CTL` bit fields: section 8.4 records four corrected rows
 *   (mode select values, sync polarity bits 17/16, high-TMDS bits 4/0) from an
 *   earlier draft.  No offset here depends on the old values.
 * - Output bit depth: section 8.4 records that an earlier draft put it in
 *   `TRANSCONF`; on Gen12 it is `PIPE_MISC[7:5]`, and `TRANSCONF`'s BPC/DITHER
 *   fields are a pre-Haswell leftover.  Nothing here programs them.
 * - The same register is `TRANSCONF` to section 5.2 (and to i915 v6.12) and
 *   `PIPECONF` to the PRM; one register, one offset, so no conflict in the
 *   table -- but do not go looking for a separate `PIPECONF`.
 * - DDI voltage-swing values: section 8.5 records that the DG1 PRM's table and
 *   i915's ADL-P table disagree for the same nominal levels, and trusts ADL-P.
 *   That is data, not an offset, and is not transcribed here.
 * - Section 8.5 says `BUF_TRANS_SELECT` indexes the DDI's translation table
 *   while its own programming sequence writes the values into the combo PHY's
 *   `PORT_TX_DW2/DW4/DW5/DW7`.  Both register sets exist in the merged table
 *   (the indexed one here, the PHY's in `regs-phy.rs`); which one a port
 *   actually uses is a PHY-family question this group does not decide.
 * - Section 8.5 names `PORT_TX_DW2/DW5/DW7` without naming a TX instance.  The
 *   group instance is the one used, because section 8.2's worked examples give
 *   the group offsets for exactly those dwords and the per-lane carve-out is
 *   stated for `DW4` only.
 *
 * Registers deliberately left out
 * - `DP_AUX_CH_CTL` (`0x64010`/`0x64110`) and `DP_AUX_CH_DATA(i)` (`0x64014 +
 *   i*4`, five registers): section 8.4 documents them, but section 11's bring-up
 *   reads EDID over GMBUS and the section 8.6 sequence never touches AUX.  They
 *   belong to the DP path (sections 8.7 and 9.6).
 * - `ICL_DPCLKA_CFGCR0` (`0x164280`) and the per-PHY `DDI_CLK_OFF` bits: written
 *   by section 8.6 step 4 and section 11 step 5.2, but the document gives them
 *   as PLL routing (section 6.3) and the clock workstream owns them.
 * - Every combo PHY register, including the ones section 8.5's write sequence
 *   and section 8.6 step 7 program, because the PHY workstream owns them and
 *   the project's rule is one declaration per register: `PORT_PCS_DW1` and
 *   `PORT_CL_DW5` live in `regs.rs`'s `COMBO_PHY_A`/`COMBO_PHY_B` (with
 *   `PORT_COMP_DW0/1/3/8/9/10`, `PORT_TX_DW8` and `ICL_PHY_MISC`), while
 *   `PORT_CL_DW10` (`0x162028`/`0x06c028`), the `TX` group's `DW2`/`DW5`/`DW7`
 *   (`0x162688`/`0x162694`/`0x16269c`, and `0x06c688`/`0x06c694`/`0x06c69c` for
 *   PHY B) and the four per-lane `TX DW4` registers (`0x162890 + 0x100*ln`, and
 *   `0x06c890 + 0x100*ln` for PHY B) are declared by `regs-phy.rs`; those
 *   offsets were derived here from section 8.2 and agree with that file.
 * - `PORT_TX_DW4`'s group instance (`0x162690`/`0x06C690`): section 8.5 says
 *   group access must not be used for `DW4`, so no group `DW4` is declared
 *   anywhere, here or in `regs-phy.rs`, only the four per-lane registers.
 * - `TRANS_VSYNCSHIFT` (`+0x28`), `BCLRPAT` (`+0x20`) and `TRANS_MULT` (`+0x2c`):
 *   section 5.3 says to leave them at reset for a progressive RGB mode, and
 *   section 11 programs none of them.
 * - `PIPE_MISC` (`0x70030`) and section 5.2's other pipe registers: those are
 *   the pipe workstream's table, not the transcoder/DDI one.  Section 11 step
 *   5.6 writes `PIPE_MISC` for output bit depth and dithering, so the merged
 *   table needs it from that workstream.
 * - The eDP panel power registers (`PP_*`, section 8.7) and the Type-C/DKL PHY
 *   registers (section 8.8): deferred by the document itself and not needed for
 *   a combo-PHY HDMI port.
 * - Section 8.5's translation tables themselves: they are data.  Only the
 *   registers that carry them are declared.
 *
 * Derived rather than literal (stated arithmetic, not a guess)
 * - The six timing registers for all four transcoders are literal: section 5.3
 *   gives all four bases (`0x60000`/`0x61000`/`0x62000`/`0x63000`) and the
 *   offset of each register within the block.
 * - `PIPESRC`, `TRANSCONF`, `PIPESTAT` and `PIPEDSL` for pipes B/C/D come from
 *   section 5.2's "Pipe B adds 0x1000, C adds 0x2000, D adds 0x3000" (and,
 *   for `PIPESTAT`/`PIPEDSL`, section 10.4's `+ pipe*0x1000`).
 * - `TRANS_CLK_SEL` B/C/D come from section 6.3's `0x46140 + tran*4`.
 * - `TRANS_DDI_FUNC_CTL` B/C/D come from section 8.4's `0x60400 + T*0x1000`.
 * - The combo PHY offsets listed above came from section 8.2's sub-block table
 *   and its worked examples: `CL` is `base + 4*dw`, the `TX` group instance is
 *   `base + 0x680 + 4*dw` and the `TX` lane instance is
 *   `base + 0x880 + ln*0x100`, with PHY A at `0x162000` and PHY B at `0x06C000`.
 *
 * Overlap with the sibling register-group files (resolve when they are merged)
 * - `DDI_BUF_CTL_A`, `DDI_BUF_CTL_B` and `TRANS_DDI_FUNC_CTL_A` are declared
 *   both here and in `regs-phy.rs`, at the same offsets (`0x6_4000`, `0x6_4100`,
 *   `0x6_0400`): one of the two declarations has to go, and section 8.4's DDI
 *   table is this group's.
 * - `PIPESRC_A..D`, `PIPESTAT_A..D` and `PIPEDSL_A..D` are declared both here
 *   and in `regs-pipe.rs`, again at the same offsets.  `PIPECONF_A..D` there is
 *   this file's `TRANSCONF_A..D`: the same register under the PRM's name, also
 *   at the same offsets, so the merge needs one name, not two.
 * - `ICL_DPCLKA_CFGCR0` is declared in `regs-dpll.rs`, which is why this file
 *   leaves it out.
 * - `DDI_BUF_TRANS_LO`/`_HI` are declared only here: `regs-phy.rs` lists them
 *   among the registers it deliberately omitted.
 */
