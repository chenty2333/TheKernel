//! Reference section 11 phases 3.4 and 4: the pipe's timings, its share of the
//! display buffer, its watermarks, and its primary plane.
//!
//! [`compute_with_watermarks`] turns a [`Mode`], [`PlaneSurface`] and the
//! PCode-derived latency config into a [`PipeProgram`],
//! which is every register value the pipe half of a modeset needs; [`program`]
//! writes the shadow half of that program in one order, [`arm`] writes the two
//! registers that latch it after the pipe is running, and [`prove`] performs the
//! pipe-side half of phase 6 -- the three reads that say whether any of it
//! worked.
//!
//! ```text
//! —    PIPE_MISC(A)  BPC 8, dither off, pixel rounding truncated
//! —    PIPE_ARB_CTL(A)  USE_PROG_SLOTS                     Wa_22012358565:adl-p
//! 3.4  the timing registers                                    from timing.rs
//! 4.1  PLANE_BUF_CFG(A,1) = 0x0fff0000                         the whole DDB
//! 4.2  PLANE_WM(A,1,0..7)  latency-derived levels              skl_watermark.c
//!      PLANE_WM_TRANS and both SAGV watermarks                  skl_watermark.c
//! 4.3  PLANE_STRIDE, PLANE_POS, PLANE_SIZE, PLANE_OFFSET,
//!      PLANE_COLOR_CTL                            shadow, latched by the arm
//! 5.6  the output workstream writes TRANSCONF and the pipe starts
//! 4.3  PLANE_CTL then PLANE_SURF                 the arm: separate step, last
//! 6.1  PIPEDSL(A) four times, a millisecond apart, must change
//! 6.2  PLANE_SURFLIVE(A,1) must equal what PLANE_SURF was written with,
//!      polled for about two frame times because the arm latches at a vblank
//! 6.4  PIPESTAT(A) bit 31, the FIFO underrun, must be clear
//! ```
//!
//! # The two halves of phase 4, and why the order is the deliverable
//!
//! Section 5.6 splits the plane commit into a `noarm` half and an `arm` half,
//! and says why: every one of those registers is double buffered, and the
//! **only** thing that makes them take effect is the write to `PLANE_SURF`.
//! The PRM says the same thing independently -- "Write the plane surface base
//! address register to trigger update of the watermarks and other plane double
//! buffered registers. This should be done only after all plane configuration
//! is configured to match the new watermark values."  So `PLANE_SURF` is the
//! commit, and the one ordering rule this module exists to keep is that nothing
//! is armed until the watermarks are in place.
//!
//! **The arm is also a separate step in time, not only a separate group of
//! registers.**  The shadow writes are latched at the plane's update event,
//! which is the pipe's vblank, and a disabled transcoder has no vblank to latch
//! at: `[I915]` says "Until the pipe starts PIPEDSL reads will return a stale
//! value" (`display/intel_display.c:478-486`).  A `PLANE_SURF` written before
//! `TRANSCONF` therefore arms nothing until the pipe runs, and depending on
//! that is depending on undocumented latch behaviour.  `[I915]` does not: it
//! enables the crtc (`:7200`) and arms the plane afterwards (`:7249`), and the
//! arm writes `PLANE_CTL` and `PLANE_SURF` adjacently
//! (`display/skl_universal_plane.c:1530-1531`).  So [`program`] writes every
//! shadow register and stops, and [`arm`] -- called after the output
//! workstream's `TRANSCONF` -- writes the pair.  Coreboot's libgfxinit arms
//! before the enable and ships that order, so the reference's order is probably
//! not fatal; whether a pending pre-enable arm survives the enable is
//! undocumented, and that is exactly why this module no longer relies on it.
//!
//! That matters more than it sounds, because section 7.1 says the failure it
//! prevents is invisible: "The default settings of the watermark configuration
//! registers **will not allow the display engine to operate**", and section 11
//! step 4.2 adds that a plane whose `PLANE_WM_EN` is 0 "reads nothing".  A pipe
//! with correct timings, a correct DDI and a correct surface address shows a
//! black screen if the watermarks are missing, which is why [`program`] builds
//! the entire write list -- including the values `PIPE_MISC` and `PIPE_ARB_CTL`
//! will end up with, which need a read each first -- *before* it writes any of
//! it, and why the tests assert on the write log rather than on the return
//! value.//!
//! # The `value − 1` convention, and the one place it is not `timing.rs`'s
//!
//! `timing.rs` owns the `value − 1` encoding of the timing registers and has a
//! round-trip test over every published mode; this module calls
//! [`timing::timing_registers`] and writes the values it returns, and there is
//! no timing arithmetic in this file at all.  The exception inside `timing.rs`
//! is `TRANS_SET_CONTEXT_LATENCY`, whose value is a plain line count and is
//! zero on this display version; this module still only writes what it is
//! given.  `PLANE_SIZE` is not a timing
//! register, but its two halves are the same two counts as `PIPESRC`'s in the
//! opposite order -- `[31:16]` is `height − 1` and `[15:0]` is `width − 1`
//! (section 5.4, against section 5.2's `WIDTH[31:16]`, `HEIGHT[15:0]`), which
//! `[I915]` writes as `PLANE_HEIGHT(src_h - 1) | PLANE_WIDTH(src_w - 1)`
//! (`display/skl_universal_plane.c:1294-1295`).  So it is `pipesrc` with its
//! halves swapped -- `rotate_left(16)` -- and no second subtraction is
//! introduced.
//!
//! The one `− 1` this file owns is `PLANE_BUF_CFG`'s, from section 7.2:
//! `((end - 1) << 16) | start`, which is a *block index*, not a count of
//! blocks, and `[I915]`'s `skl_plane_ddb_reg_val` writes it the same way
//! (`display/skl_universal_plane.c:699-706`).
//!
//! # Reference defects this module found and did not follow
//!
//! Five, and the design document records each with its citations: the two
//! below, which are defects of *description* that this file works around, and
//! three the module originally followed and no longer does (`PLANE_WM` levels
//! 6 and 7, which are not levels; `PLANE_WM_TRANS`, which the document gives no
//! offset for although `[I915]` has one; and `TRANS_VBLANK`'s low half, which
//! this display version no longer reads).  See
//! `docs/design/intel-pipe.md` §4.
//!
//! **`PLANE_STRIDE` is not in bytes.**  Section 5.4 describes `[11:0]` as
//! "stride in bytes", and a 1920-pixel XRGB8888 surface has a 7680-byte
//! stride, which does not fit in twelve bits -- so the description cannot be
//! right, and the mode section 11 phase 3.1 prefers could not be programmed
//! from it.  `[I915]` states the actual encoding in a comment: "The stride is
//! either expressed as a multiple of 64 bytes chunks for linear buffers or in
//! number of tiles for tiled buffers" (`display/skl_universal_plane.c:671-684`),
//! and `skl_plane_stride` returns `scanout_stride / 64` for a linear surface
//! (`:686-697`).  Section 11 phase 3.2's requirement that the stride be a
//! multiple of 64 bytes is the same fact seen from the allocator's side.  This
//! module therefore divides, and refuses a stride that is not a multiple of 64
//! rather than truncating it.  7680 / 64 = 120, which fits.
//!
//! # What is deliberately not here
//!
//! * **The PLL, the DDI and the pipe's own enable.**  `TRANS_CLK_SEL`,
//!   `TRANSCONF`, `TRANS_DDI_FUNC_CTL` and `DDI_BUF_CTL` are section 11 phase 5
//!   and belong to the output workstream.  This module produces the values that
//!   step needs and never touches those registers; the two halves meet at
//!   [`PipeProgram`], not at a register.  `PIPE_MISC` and `PIPE_ARB_CTL` are
//!   the two exceptions the brief calls out or that the workaround forces:
//!   section 11 step 5.6 puts `PIPE_MISC` with the pipe enable, but it is the
//!   pipe's output depth, it is not double buffered, it arms nothing, and it is
//!   written here so that everything the pipe owns is in one place;
//!   `PIPE_ARB_CTL`'s one bit is the pipe half of `Wa_22012358565:adl-p` and
//!   has to be set before the plane that reads `ARB_SLOTS` is armed, which
//!   `[I915]` also does (`display/intel_display.c:441-445`).
//! * **The framebuffer and the GGTT.**  Where the pixels are is a
//!   [`PlaneSurface`] parameter -- a plain GGTT address and a byte stride -- and
//!   this module obtains neither.  It checks what section 11 phase 3.2
//!   requires of them (4 KiB alignment for the address, a multiple of 64 for
//!   the stride) and refuses the rest.
//! * **Hardware integration and atomic watermark transitions.**  The pure
//!   [`plan_multi_plane_dbuf`] seam validates and partitions a simple
//!   non-overlapping set of packed-RGB planes using the translated i915
//!   watermark calculation. It is not connected to Native register writers,
//!   does not model cursor/scaler/async-flip planes or multiple pipes, and is
//!   not a claim of hardware support.
//! * **`PLANE_KEYVAL`, `PLANE_KEYMSK`, `PLANE_KEYMAX`, `PLANE_AUX_DIST` and
//!   `PLANE_AUX_OFFSET`.**  Section 5.6 names all five and gives none of them
//!   an offset, so none is in the register table and none is invented here.
//!   Writing "the key registers to zero" therefore means the key registers that
//!   exist, which is none of them; a surface with an explicit colour key or a
//!   planar auxiliary plane cannot be programmed from this module.
//!   `PLANE_WM_TRANS`, the sixth register section 5.6 names without an offset,
//!   is no longer in this list: `[I915]` has the offset and the module writes
//!   it, disabled, for the reason [`WatermarkProgram`] gives.
//! * **Disable.**  Section 5.6's disable path is two writes (`PLANE_CTL <- 0`,
//!   `PLANE_SURF <- 0`).  It is not implemented because section 11's bring-up
//!   order does not use it; a retry after a failed modeset will need it.
//! * **Scaling, colour management, gamma, CSC, tiling other than linear, NV12,
//!   and planes other than the primary.**  All deferred by the reference.  The
//!   plane's *gamma is disabled*, which is a bit of `PLANE_COLOR_CTL` and not
//!   the gamma block; nothing here programs a gamma table.
//!
//! # What has not been checked
//!
//! **No value from this module has been written to real silicon.**  Everything
//! below is exercised on the host against [`regs::mock::MockRegisters`], which
//! is a `BTreeMap` with a write log: it proves that the sequence writes what it
//! says, in the order it says, and that a refused write leaves the plane
//! unarmed.  It cannot prove that the values are right, and it cannot prove
//! that a plane comes up.  The three phase-6 reads in particular have never
//! observed a real `PIPEDSL` change.

use alloc::{format, string::String, vec::Vec};

use super::{
    gmbus::{MonotonicTimer, PollTimer},
    hex,
    regs::{Meaning, Register, Registers, ddi, pipe as pipe_regs},
    timing::{self, TimingRegister, TimingRegisters},
};
use crate::drm::modes::Mode;

// ---------------------------------------------------------------------------
// Field positions and encodings.
//
// Every constant cites the reference section it came from; the section in turn
// cites the PRM or the Linux `drm/i915` tree.  Where a cached `[I915]` file was
// consulted directly, the file and line are named, because two of these fields
// are places the reference document is wrong or silent.
// ---------------------------------------------------------------------------

/// `PLANE_CTL_ENABLE`, bit 31.  Reference section 5.5.
pub(crate) const PLANE_CTL_ENABLE: u32 = 1 << 31;

/// `PLANE_CTL_FORMAT_XRGB_8888` = `4 << 24`.  Reference section 5.5.
///
/// The Gen12 format field is `[27:23]` and this constant is built with the
/// Skylake mask, which the source comment says still maps correctly as long as
/// bit 23 stays 0 -- and it does, because 4 has no bit 23.
pub(crate) const PLANE_CTL_FORMAT_XRGB_8888: u32 = 4 << 24;

/// `PLANE_CTL_TILED_LINEAR` = 0: a linear surface, `TILED_MASK[12:10]` clear.
/// Reference section 5.5.
pub(crate) const PLANE_CTL_TILED_LINEAR: u32 = 0;

/// `PLANE_CTL_ARB_SLOTS(1)`: the plane's arbiter slot count, `[30:28]`.
///
/// Section 5.5 does not list the field -- it is not part of a format -- but
/// `[I915]` writes it for display version 13 and only that version, as one
/// half of `Wa_22012358565:adl-p` (`display/skl_universal_plane.c:1090-1092`).
/// `adlp_plane_ctl_arb_slots` returns exactly this value for a plane that is
/// not YUV semi-planar and whose first component is four bytes wide
/// (`:1027-1033`, the `case 4` at `:1029-1030`), which a 32-bit XRGB8888
/// surface is.  The field is `PLANE_CTL_ARB_SLOTS_MASK = REG_GENMASK(30, 28)`
/// (`skl_universal_plane_regs.h:39-40`), so `ARB_SLOTS(1)` is `1 << 28`.  Its
/// other half is `PIPE_ARB_CTL`'s `USE_PROG_SLOTS`, which
/// [`PIPE_ARB_USE_PROG_SLOTS`] sets; the value is only meaningful when that
/// bit is set, which is why both are written in the same program.
pub(crate) const PLANE_CTL_ARB_SLOTS_1: u32 = 1 << 28;

/// `PLANE_CTL` for a linear XRGB8888 scanout with no rotation and no alpha.
///
/// Section 5.5: "For the simplest bring-up use `DRM_FORMAT_XRGB8888` +
/// `DRM_FORMAT_MOD_LINEAR`, which is `PLANE_CTL_ENABLE | (4<<24)`".
///
/// `PLANE_CTL_ARB_SLOTS_1` is in here as well, and deliberately: it is the
/// plane half of `Wa_22012358565:adl-p`, this kernel's one platform is display
/// version 13, and `adlp_plane_ctl_arb_slots`'s answer for this format is the
/// same `1` on every machine the value is written to.  A plane `PLANE_CTL`
/// without it while `PIPE_ARB_CTL.USE_PROG_SLOTS` is set would leave the
/// arbiter reading whatever slot count the register's other bits happen to
/// hold.
pub(crate) const PLANE_CTL_LINEAR_XRGB8888: u32 =
    PLANE_CTL_ENABLE | PLANE_CTL_FORMAT_XRGB_8888 | PLANE_CTL_TILED_LINEAR | PLANE_CTL_ARB_SLOTS_1;

/// `PLANE_COLOR_CTL` for alpha disabled and no CSC: `ALPHA[5:4]` is zero,
/// which is [`PLANE_COLOR_CTL_ALPHA_DISABLE`].  Reference sections 5.4 and 11
/// step 4.3.
pub(crate) const PLANE_COLOR_CTL_ALPHA_DISABLE: u32 = 0;

/// `PLANE_COLOR_PLANE_GAMMA_DISABLE`, bit 13.
///
/// Section 5.4 lists `PLANE_COLOR_CTL`'s gamma field without saying which
/// value disables it, and `[I915]` is unambiguous: `glk_plane_color_ctl` ORs
/// this bit into the register for **every** plane it builds, before it looks
/// at the plane's format (`display/skl_universal_plane.c:1114-1124`, the OR at
/// `:1123`), and the field is `REG_BIT(13)`
/// (`skl_universal_plane_regs.h:262`).  A zero written here would *enable* the
/// plane's gamma correction with no gamma table programmed, which is not the
/// "no gamma" the reference's step 4.3 asks for.
pub(crate) const PLANE_COLOR_PLANE_GAMMA_DISABLE: u32 = 1 << 13;

/// `PLANE_COLOR_CTL` for a linear RGB scanout: alpha disabled, plane gamma
/// disabled, no CSC.
///
/// The reference asks for "alpha disabled, no CSC" (section 11 step 4.3) and
/// names no gamma; `[I915]`'s value for the same plane sets the gamma-disable
/// bit unconditionally, so it is set here.
pub(crate) const PLANE_COLOR_CTL_LINEAR_RGB: u32 =
    PLANE_COLOR_CTL_ALPHA_DISABLE | PLANE_COLOR_PLANE_GAMMA_DISABLE;

/// `PLANE_SURF`'s address field, `[31:12]`.  Reference section 5.4.
pub(crate) const PLANE_SURF_ADDRESS_MASK: u32 = 0xffff_f000;

/// How aligned a scanout address must be, in bytes.
///
/// Section 11 phase 3.2: "make the surface stride a multiple of 64 bytes (256
/// is safest) and the base address 4 KiB-aligned".  A 4 KiB-aligned GGTT
/// address is exactly what `[31:12]` can carry, so this is the same fact as the
/// field width.
pub(crate) const PLANE_SURF_ALIGNMENT: u64 = 4096;

/// The unit `PLANE_STRIDE` counts in for a linear surface.
///
/// Not in the reference -- see the module documentation -- but stated verbatim
/// in `[I915]`'s comment at `display/skl_universal_plane.c:671-684` and applied
/// by `skl_plane_stride` at `:686-697`.
pub(crate) const PLANE_STRIDE_UNIT_BYTES: u32 = 64;

/// `PLANE_STRIDE`'s field width, `[11:0]`, as a count of
/// [`PLANE_STRIDE_UNIT_BYTES`] units.
pub(crate) const PLANE_STRIDE_MAX: u32 = 0xfff;

/// `PLANE_WM_EN`, bit 31.  Reference section 7.3.
pub(crate) const PLANE_WM_EN: u32 = 1 << 31;

/// `PLANE_WM_IGNORE_LINES`, bit 30.  Reference section 7.3.
pub(crate) const PLANE_WM_IGNORE_LINES: u32 = 1 << 30;

/// `PLANE_WM_LINES[26:14]`'s shift.  Reference section 7.3.
pub(crate) const PLANE_WM_LINES_SHIFT: u32 = 14;

/// The old test-only generous profile's line count; active display-12/13
/// programming uses `skl_wm_max_lines()` from the translated source module.
#[cfg(test)]
pub(crate) const PLANE_WM_LINES_MAX: u32 = 31;

/// `PLANE_WM_BLOCKS`'s largest value, `[11:0]`.  Reference section 7.3.
pub(crate) const PLANE_WM_BLOCKS_MAX: u32 = 0xfff;

/// `PLANE_WM_LINES`'s field width, `[26:14]`: thirteen bits, so the largest
/// value the register can hold is `0x1fff`.
///
/// Section 7.3 says the field is 13 bits wide; the source algorithm applies
/// the per-display-version hardware limit before values reach this encoder.
pub(crate) const PLANE_WM_LINES_MASK_MAX: u32 = 0x1fff;

/// `PIPE_ARB_USE_PROG_SLOTS`, bit 13 of `PIPE_ARB_CTL`.
///
/// The pipe half of `Wa_22012358565:adl-p`, written for display version 13
/// only: `intel_de_rmw(..., PIPE_ARB_CTL(pipe), 0, PIPE_ARB_USE_PROG_SLOTS)`
/// (`display/intel_display.c:441-445`).  It tells the pipe to take the plane's
/// arbiter slot count from `PLANE_CTL`'s `ARB_SLOTS[30:28]`
/// ([`PLANE_CTL_ARB_SLOTS_1`]) instead of from the register's own default.
/// Reference section 5.2 states the field, and `[I915]`'s
/// `PIPE_ARB_USE_PROG_SLOTS REG_BIT(13)` (`i915_reg.h:1706`) agrees with it.
pub(crate) const PIPE_ARB_USE_PROG_SLOTS: u32 = 1 << 13;

/// How many latency levels there are, and therefore how many `PLANE_WM`
/// registers the six-level block holds.
///
/// The reference never states a count; section 7.4 step 1 has PCode return
/// levels 0-3 and 4-7, which is where the original eight came from.  `[I915]`
/// programs **six** on this platform: `skl_setup_wm_latency` sets
/// `num_levels = 6` when `HAS_HW_SAGV_WM`
/// (`display/skl_watermark.c:3378-3383`), and `HAS_HW_SAGV_WM` is
/// `DISPLAY_VER >= 13 && !IS_DGFX` (`display/intel_display_device.h:141`) --
/// ADL-N is display version 13 and integrated, so it is six.
///
/// This is not a detail of how many levels to compute.  `PLANE_WM(pipe, plane,
/// level)` is `_PLANE_WM_1_A_0 (0x70240) + level*4`
/// (`skl_universal_plane_regs.h:315-321`), so the formula's level 6 and level 7
/// are `0x70258` and `0x7025c`, which are `PLANE_WM_SAGV` and
/// `PLANE_WM_SAGV_TRANS` (`:327`, `:335`) -- different registers with the same
/// field layout and a different meaning.  Iterating to eight and clearing
/// `PLANE_WM_EN` on the last two would therefore not "disable levels 7 and 8",
/// as the first version of this module put it: it would disable the SAGV
/// watermarks, and a plane whose watermark has `PLANE_WM_EN` clear "reads
/// nothing" (section 11 step 4.2, section 7.1).
pub(crate) const PLANE_WM_LEVELS: usize = 6;

/// The DBUF's size in 512-byte blocks.
///
/// Section 7.2 gives the DDB as 4096 blocks, and `[I915]`'s `XE_LPD_FEATURES`
/// has `.dbuf.size = 4096` (`display/intel_display_device.c:1023`).
pub(crate) const DDB_BLOCKS: u32 = 4096;

/// `PLANE_BUF_START[11:0]`.  Reference section 7.2.
pub(crate) const PLANE_BUF_START_MASK: u32 = 0xfff;

/// `PLANE_BUF_END[27:16]`.  Reference section 7.2.
pub(crate) const PLANE_BUF_END_SHIFT: u32 = 16;

/// `PIPE_MISC_BPC_MASK[7:5]`.  Reference section 8.4.
pub(crate) const PIPE_MISC_BPC_MASK: u32 = 0b111 << 5;

/// `PIPE_MISC_BPC_8` = 0 in `[7:5]`.  Reference sections 8.4 and 11 step 5.6.
pub(crate) const PIPE_MISC_BPC_8: u32 = 0 << 5;

/// `PIPE_MISC_DITHER_ENABLE`, bit 4.  Reference section 8.4.
pub(crate) const PIPE_MISC_DITHER_ENABLE: u32 = 1 << 4;

/// `PIPE_MISC_DITHER_TYPE[3:2]`.  Reference section 8.4.
pub(crate) const PIPE_MISC_DITHER_TYPE_MASK: u32 = 0b11 << 2;

/// `PIPE_MISC_PIXEL_ROUNDING_TRUNC`, bit 8.
///
/// `[I915]`'s `bdw_set_pipe_misc` sets it for display version 12 and later --
/// `if (DISPLAY_VER(dev_priv) >= 12) val |= PIPE_MISC_PIXEL_ROUNDING_TRUNC;`
/// (`display/intel_display.c:3289-3290`) -- and the field is `REG_BIT(8)`
/// (`i915_reg.h:1719`, commented `tgl+`).  The reference document's section
/// 5.2 lists the bit and never says what to do with it, so this is the other
/// place this module follows `[I915]` over the reference: truncation is what
/// the vendor driver does to the pipe's 8-bit output on every machine this
/// code can run on.
pub(crate) const PIPE_MISC_PIXEL_ROUNDING_TRUNC: u32 = 1 << 8;

/// The bits of `PIPE_MISC` this bring-up owns: the output depth, dithering and
/// pixel rounding truncation.
///
/// `[I915]`'s `bdw_set_pipe_misc` builds the whole register and sets
/// `PIXEL_ROUNDING_TRUNC` for display version 12 and later
/// (`display/intel_display.c:3289-3290`); every other bit belongs to a colour
/// format, an HDR mode, a YUV output or PSR, none of which this bring-up does.
/// Owning bit 8 as well means the read-modify-write sets it rather than
/// preserving whatever the firmware left, which is what the vendor driver does.
pub(crate) const PIPE_MISC_OWNED_MASK: u32 = PIPE_MISC_BPC_MASK
    | PIPE_MISC_DITHER_ENABLE
    | PIPE_MISC_DITHER_TYPE_MASK
    | PIPE_MISC_PIXEL_ROUNDING_TRUNC;

/// `PIPE_FIFO_UNDERRUN_STATUS`, bit 31 of the per-pipe `PIPESTAT`.
///
/// Reference sections 5.2, 10.4 and 11 step 6.4.  It is a bit of a register
/// this module reads, never a register of its own.
pub(crate) const PIPE_FIFO_UNDERRUN_STATUS: u32 = 1 << 31;

/// `PIPEDSL`'s `LINE[19:0]`.  Reference section 5.2.
pub(crate) const PIPEDSL_LINE_MASK: u32 = 0xf_ffff;

/// How many times `PIPEDSL` is read before the scanline check gives up on it.
///
/// Section 11 step 6.1 asks for two reads "a few milliseconds apart".  Four
/// samples at [`SCANLINE_INTERVAL_MICROS`] cost three intervals and make the
/// check immune to the one way two reads can agree by accident: a second read
/// that lands on the same line as the first -- at 1080p60 a line is 14.9
/// microseconds and the counter has 1125 values, so a fixed two-read check
/// would call a perfectly good pipe stopped once in a few hundred boots.  The
/// samples' timestamps are kept as well, so the same four reads also produce
/// the line rate section 12.3 asks for.
pub(crate) const SCANLINE_SAMPLES: usize = 4;

/// The gap between two `PIPEDSL` samples.
///
/// At 1080p60 a whole line is 14.9 microseconds, so a millisecond is 67 lines
/// and every interval moves the counter by tens of lines.  The four samples
/// span three intervals -- about 3 ms, which is **less than one 1080p60 frame**
/// (16.7 ms) -- so this is not a frame count and could not be one; it is a line
/// rate, which is what section 12.3 asks for.  The reference's "a few
/// milliseconds" is the outer bound this sits inside.
pub(crate) const SCANLINE_INTERVAL_MICROS: u64 = 1_000;

/// How many times one interval may call [`PollTimer::pause`] before moving on.
///
/// A bound, not a duration: [`PollTimer`]'s contract is that `pause` advances
/// the clock, and a test double that broke that contract would otherwise hang
/// the boot.  The real timer's pause is two microseconds, so a millisecond
/// costs 500 of these.
const SCANLINE_PAUSE_BUDGET: u32 = 4096;

// ---------------------------------------------------------------------------
// The four pipes
// ---------------------------------------------------------------------------

/// Which pipe a program is for.
///
/// Section 5.1: four pipes, tied one to one to transcoders A-D, and section 5.2
/// gives the per-pipe stride: pipe B adds `0x1000`, C adds `0x2000`, D adds
/// `0x3000`.  Every register this module writes has a declared instance for all
/// four, so nothing here computes an offset from a base.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Pipe {
    A,
    B,
    C,
    D,
}

impl Pipe {
    /// Every pipe, in index order.
    pub(crate) const ALL: [Self; 4] = [Self::A, Self::B, Self::C, Self::D];

    /// The pipe's name, as the reference's tables and every log line use it.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
        }
    }

    /// The pipe's index, `0` for A.
    pub(crate) const fn index(self) -> u32 {
        match self {
            Self::A => 0,
            Self::B => 1,
            Self::C => 2,
            Self::D => 3,
        }
    }

    /// The transcoder timing register for one of `timing.rs`'s values.
    ///
    /// Section 5.3 gives the transcoder block's base and the six offsets;
    /// section 5.2 gives `PIPESRC` at `0x6001c`.  The two are the same block,
    /// which is why one function maps both.  `SET_CONTEXT_LATENCY` is the
    /// eighth and is not in the reference at all: `[I915]` has it at the
    /// block's `+0x7c` for display version 13 and later
    /// (`i915_reg.h:4027-4031`, `display/intel_display.c:2725-2727`), which is
    /// why `ddi.rs` declares it beside the six.
    pub(crate) const fn timing(self, register: TimingRegister) -> Register {
        match self {
            Self::A => match register {
                TimingRegister::SetContextLatency => ddi::TRANS_SET_CONTEXT_LATENCY_A,
                TimingRegister::Htotal => ddi::TRANS_HTOTAL_A,
                TimingRegister::Hblank => ddi::TRANS_HBLANK_A,
                TimingRegister::Hsync => ddi::TRANS_HSYNC_A,
                TimingRegister::Vtotal => ddi::TRANS_VTOTAL_A,
                TimingRegister::Vblank => ddi::TRANS_VBLANK_A,
                TimingRegister::Vsync => ddi::TRANS_VSYNC_A,
                TimingRegister::Pipesrc => pipe_regs::PIPESRC_A,
            },
            Self::B => match register {
                TimingRegister::SetContextLatency => ddi::TRANS_SET_CONTEXT_LATENCY_B,
                TimingRegister::Htotal => ddi::TRANS_HTOTAL_B,
                TimingRegister::Hblank => ddi::TRANS_HBLANK_B,
                TimingRegister::Hsync => ddi::TRANS_HSYNC_B,
                TimingRegister::Vtotal => ddi::TRANS_VTOTAL_B,
                TimingRegister::Vblank => ddi::TRANS_VBLANK_B,
                TimingRegister::Vsync => ddi::TRANS_VSYNC_B,
                TimingRegister::Pipesrc => pipe_regs::PIPESRC_B,
            },
            Self::C => match register {
                TimingRegister::SetContextLatency => ddi::TRANS_SET_CONTEXT_LATENCY_C,
                TimingRegister::Htotal => ddi::TRANS_HTOTAL_C,
                TimingRegister::Hblank => ddi::TRANS_HBLANK_C,
                TimingRegister::Hsync => ddi::TRANS_HSYNC_C,
                TimingRegister::Vtotal => ddi::TRANS_VTOTAL_C,
                TimingRegister::Vblank => ddi::TRANS_VBLANK_C,
                TimingRegister::Vsync => ddi::TRANS_VSYNC_C,
                TimingRegister::Pipesrc => pipe_regs::PIPESRC_C,
            },
            Self::D => match register {
                TimingRegister::SetContextLatency => ddi::TRANS_SET_CONTEXT_LATENCY_D,
                TimingRegister::Htotal => ddi::TRANS_HTOTAL_D,
                TimingRegister::Hblank => ddi::TRANS_HBLANK_D,
                TimingRegister::Hsync => ddi::TRANS_HSYNC_D,
                TimingRegister::Vtotal => ddi::TRANS_VTOTAL_D,
                TimingRegister::Vblank => ddi::TRANS_VBLANK_D,
                TimingRegister::Vsync => ddi::TRANS_VSYNC_D,
                TimingRegister::Pipesrc => pipe_regs::PIPESRC_D,
            },
        }
    }

    /// `PIPE_MISC(pipe)`: output depth and dithering.  Section 5.2.
    pub(crate) const fn misc(self) -> Register {
        match self {
            Self::A => pipe_regs::PIPE_MISC_A,
            Self::B => pipe_regs::PIPE_MISC_B,
            Self::C => pipe_regs::PIPE_MISC_C,
            Self::D => pipe_regs::PIPE_MISC_D,
        }
    }

    /// `PLANE_BUF_CFG(pipe,1)`: the primary plane's DDB allocation.
    /// Sections 5.4 and 7.2.
    pub(crate) const fn plane_buf_cfg(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_BUF_CFG_A,
            Self::B => pipe_regs::PLANE_BUF_CFG_B,
            Self::C => pipe_regs::PLANE_BUF_CFG_C,
            Self::D => pipe_regs::PLANE_BUF_CFG_D,
        }
    }

    /// `PLANE_WM(pipe,1,level)`: one watermark level of the primary plane.
    /// Sections 5.4 and 7.3.
    ///
    /// The six levels, not the eight the reference's `0x70240 + level*4`
    /// formula would produce: see [`PLANE_WM_LEVELS`] for why the formula's
    /// last two results are different registers.
    ///
    /// # Panics
    ///
    /// If `level` is not below [`PLANE_WM_LEVELS`].  The only caller iterates
    /// [`WatermarkProgram::levels`], which has exactly that many entries, so a
    /// panic here would be a bug in this file rather than a hardware condition.
    pub(crate) fn plane_wm(self, level: usize) -> Register {
        let levels: &[Register; PLANE_WM_LEVELS] = match self {
            Self::A => &[
                pipe_regs::PLANE_WM_0_A,
                pipe_regs::PLANE_WM_1_A,
                pipe_regs::PLANE_WM_2_A,
                pipe_regs::PLANE_WM_3_A,
                pipe_regs::PLANE_WM_4_A,
                pipe_regs::PLANE_WM_5_A,
            ],
            Self::B => &[
                pipe_regs::PLANE_WM_0_B,
                pipe_regs::PLANE_WM_1_B,
                pipe_regs::PLANE_WM_2_B,
                pipe_regs::PLANE_WM_3_B,
                pipe_regs::PLANE_WM_4_B,
                pipe_regs::PLANE_WM_5_B,
            ],
            Self::C => &[
                pipe_regs::PLANE_WM_0_C,
                pipe_regs::PLANE_WM_1_C,
                pipe_regs::PLANE_WM_2_C,
                pipe_regs::PLANE_WM_3_C,
                pipe_regs::PLANE_WM_4_C,
                pipe_regs::PLANE_WM_5_C,
            ],
            Self::D => &[
                pipe_regs::PLANE_WM_0_D,
                pipe_regs::PLANE_WM_1_D,
                pipe_regs::PLANE_WM_2_D,
                pipe_regs::PLANE_WM_3_D,
                pipe_regs::PLANE_WM_4_D,
                pipe_regs::PLANE_WM_5_D,
            ],
        };
        levels[level]
    }

    /// `PLANE_WM_TRANS(pipe,1)`: the plane's transition watermark.
    /// `[I915]` `skl_universal_plane_regs.h:343-349`.
    pub(crate) const fn plane_wm_trans(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_WM_TRANS_A,
            Self::B => pipe_regs::PLANE_WM_TRANS_B,
            Self::C => pipe_regs::PLANE_WM_TRANS_C,
            Self::D => pipe_regs::PLANE_WM_TRANS_D,
        }
    }

    /// `PLANE_WM_SAGV(pipe,1)`: the watermark used while SAGV is active.
    /// `[I915]` `skl_universal_plane_regs.h:327-333`.
    pub(crate) const fn plane_wm_sagv(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_WM_SAGV_A,
            Self::B => pipe_regs::PLANE_WM_SAGV_B,
            Self::C => pipe_regs::PLANE_WM_SAGV_C,
            Self::D => pipe_regs::PLANE_WM_SAGV_D,
        }
    }

    /// `PLANE_WM_SAGV_TRANS(pipe,1)`: the transition half of
    /// [`Self::plane_wm_sagv`].  `[I915]` `skl_universal_plane_regs.h:335-341`.
    pub(crate) const fn plane_wm_sagv_trans(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_WM_SAGV_TRANS_A,
            Self::B => pipe_regs::PLANE_WM_SAGV_TRANS_B,
            Self::C => pipe_regs::PLANE_WM_SAGV_TRANS_C,
            Self::D => pipe_regs::PLANE_WM_SAGV_TRANS_D,
        }
    }

    /// `PIPE_ARB_CTL(pipe)`: the pipe's arbiter control.  Section 5.2, and the
    /// pipe half of `Wa_22012358565:adl-p` (`display/intel_display.c:441-445`).
    pub(crate) const fn arb_ctl(self) -> Register {
        match self {
            Self::A => pipe_regs::PIPE_ARB_CTL_A,
            Self::B => pipe_regs::PIPE_ARB_CTL_B,
            Self::C => pipe_regs::PIPE_ARB_CTL_C,
            Self::D => pipe_regs::PIPE_ARB_CTL_D,
        }
    }

    /// `PLANE_STRIDE(pipe,1)`.  Section 5.4.
    pub(crate) const fn plane_stride(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_STRIDE_A,
            Self::B => pipe_regs::PLANE_STRIDE_B,
            Self::C => pipe_regs::PLANE_STRIDE_C,
            Self::D => pipe_regs::PLANE_STRIDE_D,
        }
    }

    /// `PLANE_POS(pipe,1)`.  Section 5.4.
    pub(crate) const fn plane_pos(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_POS_A,
            Self::B => pipe_regs::PLANE_POS_B,
            Self::C => pipe_regs::PLANE_POS_C,
            Self::D => pipe_regs::PLANE_POS_D,
        }
    }

    /// `PLANE_SIZE(pipe,1)`.  Section 5.4.
    pub(crate) const fn plane_size(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_SIZE_A,
            Self::B => pipe_regs::PLANE_SIZE_B,
            Self::C => pipe_regs::PLANE_SIZE_C,
            Self::D => pipe_regs::PLANE_SIZE_D,
        }
    }

    /// `PLANE_OFFSET(pipe,1)`.  Section 5.4.
    pub(crate) const fn plane_offset(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_OFFSET_A,
            Self::B => pipe_regs::PLANE_OFFSET_B,
            Self::C => pipe_regs::PLANE_OFFSET_C,
            Self::D => pipe_regs::PLANE_OFFSET_D,
        }
    }

    /// `PLANE_COLOR_CTL(pipe,1)`.  Section 5.4.
    pub(crate) const fn plane_color_ctl(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_COLOR_CTL_A,
            Self::B => pipe_regs::PLANE_COLOR_CTL_B,
            Self::C => pipe_regs::PLANE_COLOR_CTL_C,
            Self::D => pipe_regs::PLANE_COLOR_CTL_D,
        }
    }

    /// `PLANE_CTL(pipe,1)`.  Section 5.4.
    pub(crate) const fn plane_ctl(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_CTL_A,
            Self::B => pipe_regs::PLANE_CTL_B,
            Self::C => pipe_regs::PLANE_CTL_C,
            Self::D => pipe_regs::PLANE_CTL_D,
        }
    }

    /// `PLANE_SURF(pipe,1)`: the commit.  Section 5.4.
    pub(crate) const fn plane_surf(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_SURF_A,
            Self::B => pipe_regs::PLANE_SURF_B,
            Self::C => pipe_regs::PLANE_SURF_C,
            Self::D => pipe_regs::PLANE_SURF_D,
        }
    }

    /// `PLANE_SURFLIVE(pipe,1)`: the address being scanned.  Section 5.4.
    pub(crate) const fn plane_surflive(self) -> Register {
        match self {
            Self::A => pipe_regs::PLANE_SURFLIVE_A,
            Self::B => pipe_regs::PLANE_SURFLIVE_B,
            Self::C => pipe_regs::PLANE_SURFLIVE_C,
            Self::D => pipe_regs::PLANE_SURFLIVE_D,
        }
    }

    /// `PIPEDSL(pipe)`: the live scanline counter.  Section 5.2.
    pub(crate) const fn pipedsl(self) -> Register {
        match self {
            Self::A => pipe_regs::PIPEDSL_A,
            Self::B => pipe_regs::PIPEDSL_B,
            Self::C => pipe_regs::PIPEDSL_C,
            Self::D => pipe_regs::PIPEDSL_D,
        }
    }

    /// `PIPESTAT(pipe)`: bit 31 is the FIFO underrun.  Sections 5.2 and 10.4.
    pub(crate) const fn pipestat(self) -> Register {
        match self {
            Self::A => pipe_regs::PIPESTAT_A,
            Self::B => pipe_regs::PIPESTAT_B,
            Self::C => pipe_regs::PIPESTAT_C,
            Self::D => pipe_regs::PIPESTAT_D,
        }
    }
}

impl core::fmt::Display for Pipe {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// Compute
// ---------------------------------------------------------------------------

/// Where the plane's pixels are, which this module cannot work out for itself.
///
/// Both fields come from the framebuffer and GGTT workstream; neither is
/// derived here.  Section 11 phase 3.2 states what they must satisfy: "Use a
/// linear, contiguous allocation; make the surface stride a multiple of 64
/// bytes (256 is safest) and the base address 4 KiB-aligned."
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlaneSurface {
    /// The GGTT address of the surface's first pixel.
    ///
    /// A graphics address, not a physical one: the display engine reads through
    /// the GGTT, and section 11 phase 3.2's second step is writing the PTE for
    /// the allocation.  Only `[31:12]` of it can be expressed in `PLANE_SURF`,
    /// so a wider address is refused rather than truncated.
    pub(crate) ggtt_address: u64,
    /// Bytes from the start of one scanline to the next.
    ///
    /// The surface's *pitch*, as the framebuffer allocated it.  Section 11
    /// phase 3.2 requires a multiple of 64; see [`PLANE_STRIDE_UNIT_BYTES`] for
    /// why.
    pub(crate) stride_bytes: u32,
}

/// The primary plane's DDB allocation, as a block range.
///
/// Section 7.2: `PLANE_BUF_CFG` holds `PLANE_BUF_START[11:0]` and
/// `PLANE_BUF_END[27:16]`, encoded `((end - 1) << 16) | start` -- an inclusive
/// *end index*, which is where this type's one subtraction lives.  ADL-N uses
/// the full 12 bits of both fields, so block index 4095 fits and block 4096 is
/// the first address past the buffer.
///
/// The fields are private; [`Self::WHOLE_BUFFER`] is used by first-light-up,
/// while the checked range constructor is reserved for the pure DBUF planner:
/// section 7.2's
/// "[INF] This is the *safe maximum*: the one plane owns the entire DBUF.  It
/// is not what a production driver does (it wastes power), but it cannot
/// under-allocate.  For a first light-up, take it."
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DdbAllocation {
    start: u32,
    end: u32,
}

impl DdbAllocation {
    /// One plane owning all [`DDB_BLOCKS`] blocks: section 11 step 4.1's
    /// `PLANE_BUF_CFG(A,1) = ((4096-1) << 16) | 0 = 0x0FFF0000`.
    pub(crate) const WHOLE_BUFFER: Self = Self {
        start: 0,
        end: DDB_BLOCKS,
    };

    /// Make a half-open allocation after checking it fits the display buffer.
    fn checked(start: u32, end: u32) -> Result<Self, PipeError> {
        if start >= end || end > DDB_BLOCKS {
            return Err(PipeError::DdbAllocationOutOfBounds { start, end });
        }
        Ok(Self { start, end })
    }

    /// The first block the plane owns, inclusive.
    pub(crate) const fn start(self) -> u32 {
        self.start
    }

    /// The first block past the plane's allocation, exclusive.
    pub(crate) const fn end(self) -> u32 {
        self.end
    }

    /// How many blocks the plane owns.
    pub(crate) const fn blocks(self) -> u32 {
        self.end - self.start
    }

    /// The value of `PLANE_BUF_CFG`, ready to write.
    ///
    /// `end - 1` is section 7.2's encoding and cannot underflow: the fields are
    /// private and [`Self::WHOLE_BUFFER`] is the only value that exists, so
    /// `end` is always [`DDB_BLOCKS`].
    pub(crate) const fn register_value(self) -> u32 {
        ((self.end - 1) << PLANE_BUF_END_SHIFT) | (self.start & PLANE_BUF_START_MASK)
    }
}

/// One watermark level: the fields of a single `PLANE_WM` register.
///
/// Section 7.3: `PLANE_WM_EN[31]`, `PLANE_WM_IGNORE_LINES[30]`,
/// `PLANE_WM_LINES[26:14]`, `PLANE_WM_BLOCKS[11:0]`.  `lines` is a count of
/// scanlines and `blocks` a count of 512-byte DBUF blocks, both of which are
/// written as themselves -- not as a count minus one, unlike the timing
/// registers and `PLANE_BUF_CFG`'s end index.
///
/// `ignore_lines` and all numeric fields are copied from the source-generated
/// `skl_watermark_full::WmLevel` before register encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatermarkLevel {
    enabled: bool,
    ignore_lines: bool,
    blocks: u32,
    lines: u32,
}

impl WatermarkLevel {
    /// A level with `PLANE_WM_EN` clear, which the hardware ignores.
    pub(crate) const fn disabled() -> Self {
        Self {
            enabled: false,
            ignore_lines: false,
            blocks: 0,
            lines: 0,
        }
    }

    /// Test fixture only. Active values use the full source calculation.
    #[cfg(test)]
    pub(crate) const fn generous(blocks: u32) -> Self {
        Self {
            enabled: true,
            ignore_lines: false,
            blocks: blocks & PLANE_WM_BLOCKS_MAX,
            lines: PLANE_WM_LINES_MAX,
        }
    }

    /// Whether `PLANE_WM_EN` is set.
    pub(crate) const fn is_enabled(self) -> bool {
        self.enabled
    }

    /// The level's block count, as the DDB counts blocks.
    pub(crate) const fn blocks(self) -> u32 {
        self.blocks
    }

    /// The level's scanline count.
    pub(crate) const fn lines(self) -> u32 {
        self.lines
    }

    /// The value of `PLANE_WM(pipe,1,level)`, ready to write.
    ///
    /// The block count is masked to the field rather than trusted, so a
    /// too-large count cannot bleed into the line field; the constructors
    /// above mask it as well, and this is the second half of the same rule.
    pub(crate) const fn register_value(self) -> u32 {
        let mut value = self.blocks & PLANE_WM_BLOCKS_MAX;
        value |= self.lines << PLANE_WM_LINES_SHIFT;
        if self.ignore_lines {
            value |= PLANE_WM_IGNORE_LINES;
        }
        if self.enabled {
            value |= PLANE_WM_EN;
        }
        value
    }
}

/// Check the test-only generous profile against the field encoding.
#[cfg(test)]
const _: () = assert!(PLANE_WM_LINES_MAX <= PLANE_WM_LINES_MASK_MAX);

/// Every watermark value one plane has: levels, transition, and SAGV entries.
/// Active values use the translated PCode latency and `skl_build_plane_wm_single()`
/// calculation below; generous values are test-only register-ordering fixtures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatermarkProgram {
    levels: [WatermarkLevel; PLANE_WM_LEVELS],
    transition: WatermarkLevel,
    sagv: WatermarkLevel,
    sagv_transition: WatermarkLevel,
}

/// Source-derived inputs needed for the primary-plane WM calculation. The
/// values are read during power initialization, before this program can write
/// the plane's double-buffered state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatermarkConfig {
    pub(crate) display_ver: u8,
    pub(crate) latencies: [u32; intel_display::skl_watermark_full::WM_LEVELS],
    pub(crate) num_levels: usize,
    pub(crate) sagv_block_time_us: u32,
}

impl WatermarkProgram {
    /// Build single-visible-plane watermarks for the selected packed RGB
    /// format with the translated `skl_build_plane_wm_single()` path, then apply i915's DDB
    /// minimum-allocation checks before packing the register program.
    fn from_i915(
        ddb: DdbAllocation,
        mode: &Mode,
        config: WatermarkConfig,
        pixel_format: u32,
    ) -> Result<Self, PipeError> {
        Self::from_i915_plane(
            ddb,
            mode,
            config,
            pixel_format,
            0,
            u32::from(mode.hdisplay),
            mode.clock_khz,
        )
        .map(|(program, _)| program)
    }

    /// Build one packed-RGB plane's source watermark state. The caller owns
    /// plane-index validation and any DDB partitioning across planes.
    fn from_i915_plane(
        ddb: DdbAllocation,
        mode: &Mode,
        config: WatermarkConfig,
        pixel_format: u32,
        plane_index: usize,
        width: u32,
        pixel_rate: u32,
    ) -> Result<(Self, u32), PipeError> {
        use intel_display::skl_watermark_full as wm;

        let cpp = pixel_format_cpp(pixel_format)?;
        if plane_index >= wm::PLANES || width == 0 || pixel_rate == 0 {
            return Err(PipeError::Watermark(-22));
        }

        let display = wm::DisplayCaps {
            display_ver: config.display_ver,
            display_ver_fixed: config.display_ver,
            alderlake_p: config.display_ver == 13,
            sagv: true,
            sagv_wm: true,
            has_hw_sagv_wm: true,
            ..wm::DisplayCaps::default()
        };
        let input = wm::PlaneWmInput {
            width,
            cpp: cpp as u8,
            pixel_rate,
            pipe_htotal: u32::from(mode.htotal),
            num_format_planes: 1,
            visible: true,
            ..wm::PlaneWmInput::default()
        };
        let mut source = wm::PlaneWm::default();
        wm::skl_build_plane_wm_single(
            &display,
            &input,
            plane_index,
            0,
            config.num_levels.min(wm::WM_LEVELS),
            &config.latencies,
            false,
            false,
            config.sagv_block_time_us,
            &mut source,
        )
        .map_err(PipeError::Watermark)?;

        // Retain every configured level plus transition/SAGV minimum. This
        // conservative minimum-only policy intentionally does not reproduce
        // i915's data-rate-weighted spare-block distribution or level fallback.
        let mut minimum = 0u32;
        for level in source
            .levels
            .iter()
            .take(config.num_levels.min(wm::WM_LEVELS))
        {
            minimum = minimum.max(u32::from(level.min_ddb_alloc));
        }
        for level in [source.trans_wm, source.sagv_wm0, source.sagv_trans_wm] {
            minimum = minimum.max(u32::from(level.min_ddb_alloc));
        }
        if minimum == u32::from(u16::MAX) {
            return Err(PipeError::Watermark(-22));
        }

        let source_ddb = wm::DdbEntry {
            start: u16::try_from(ddb.start).map_err(|_| PipeError::Watermark(-22))?,
            end: u16::try_from(ddb.end).map_err(|_| PipeError::Watermark(-22))?,
        };
        for level in &mut source.levels {
            wm::skl_check_wm_level(level, source_ddb);
        }
        wm::skl_check_wm_level(&mut source.trans_wm, source_ddb);
        wm::skl_check_wm_level(&mut source.sagv_wm0, source_ddb);
        wm::skl_check_wm_level(&mut source.sagv_trans_wm, source_ddb);

        let convert = |level: wm::WmLevel| -> Result<WatermarkLevel, PipeError> {
            if level.blocks > PLANE_WM_BLOCKS_MAX || level.lines > PLANE_WM_LINES_MASK_MAX {
                return Err(PipeError::Watermark(-22));
            }
            Ok(WatermarkLevel {
                enabled: level.enable,
                ignore_lines: level.ignore_lines,
                blocks: level.blocks,
                lines: level.lines,
            })
        };
        let mut levels = [WatermarkLevel::disabled(); PLANE_WM_LEVELS];
        for (target, source) in levels.iter_mut().zip(source.levels) {
            *target = convert(source)?;
        }
        Ok((
            Self {
                levels,
                transition: convert(source.trans_wm)?,
                sagv: convert(source.sagv_wm0)?,
                sagv_transition: convert(source.sagv_trans_wm)?,
            },
            minimum,
        ))
    }
}

impl WatermarkProgram {
    /// Test fixture for section 7.3's initial generous first-light-up profile.
    /// Production modesets require `from_i915()` and PCode-derived latencies.
    #[cfg(test)]
    pub(crate) fn generous(ddb: DdbAllocation) -> Self {
        let blocks = ddb.blocks().min(PLANE_WM_BLOCKS_MAX);
        let level_zero = WatermarkLevel::generous(blocks);
        let mut levels = [WatermarkLevel::disabled(); PLANE_WM_LEVELS];
        levels[0] = level_zero;
        Self {
            levels,
            transition: WatermarkLevel::disabled(),
            sagv: level_zero,
            sagv_transition: level_zero,
        }
    }

    /// Every level, in register order.
    pub(crate) const fn levels(&self) -> &[WatermarkLevel; PLANE_WM_LEVELS] {
        &self.levels
    }

    /// The level-zero register value, also retained in phase-6 underrun reports.
    pub(crate) const fn level_zero_value(&self) -> u32 {
        self.levels[0].register_value()
    }

    /// `PLANE_WM_TRANS` source-calculated register value.
    pub(crate) const fn transition_value(&self) -> u32 {
        self.transition.register_value()
    }

    /// `PLANE_WM_SAGV` source-calculated register value.
    pub(crate) const fn sagv_value(&self) -> u32 {
        self.sagv.register_value()
    }

    /// `PLANE_WM_SAGV_TRANS` source-calculated register value.
    pub(crate) const fn sagv_transition_value(&self) -> u32 {
        self.sagv_transition.register_value()
    }
}

/// The primary plane's register values.
///
/// Every field is the value a register takes, except `stride_bytes`, which is
/// kept in the caller's units as well so that a log line can show both.  The
/// plane is the whole mode: no scaling, no panning and no rotation, so
/// `PLANE_POS` and `PLANE_OFFSET` are zero and `PLANE_SIZE` is the active size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlaneProgram {
    /// `PLANE_STRIDE`'s value: the stride in [`PLANE_STRIDE_UNIT_BYTES`] units.
    pub(crate) stride: u32,
    /// The stride as the caller gave it, in bytes.
    pub(crate) stride_bytes: u32,
    /// `PLANE_POS`: `Y[31:16]`, `X[15:0]`.  Zero for a full-screen plane.
    pub(crate) pos: u32,
    /// `PLANE_SIZE`: `height-1[31:16]`, `width-1[15:0]`.
    pub(crate) size: u32,
    /// `PLANE_OFFSET`: source `Y[31:16]`, `X[15:0]` inside the surface.  Zero.
    pub(crate) offset: u32,
    /// `PLANE_COLOR_CTL`: alpha disabled, no CSC, no gamma.
    pub(crate) color_ctl: u32,
    /// `PLANE_CTL`: enable, the admitted packed RGB format, linear.
    pub(crate) ctl: u32,
    /// `PLANE_SURF`: the GGTT address masked to `[31:12]`.  The commit.
    pub(crate) surf: u32,
}

impl PlaneProgram {
    /// The `PLANE_SURF` field for a GGTT address, or why it cannot be one.
    pub(crate) fn surface_field(address: u64) -> Result<u32, PipeError> {
        if address == 0 {
            return Err(PipeError::SurfaceAddressZero);
        }
        if !address.is_multiple_of(PLANE_SURF_ALIGNMENT) {
            return Err(PipeError::SurfaceMisaligned { address });
        }
        if address >> 32 != 0 {
            return Err(PipeError::SurfaceAboveAddressWindow { address });
        }
        Ok(address as u32 & PLANE_SURF_ADDRESS_MASK)
    }

    /// The `PLANE_STRIDE` field for a byte stride, or why it cannot be one.
    fn stride_field(stride_bytes: u32) -> Result<u32, PipeError> {
        if stride_bytes == 0 {
            return Err(PipeError::StrideZero);
        }
        if !stride_bytes.is_multiple_of(PLANE_STRIDE_UNIT_BYTES) {
            return Err(PipeError::StrideNotAMultipleOf64 { stride_bytes });
        }
        let units = stride_bytes / PLANE_STRIDE_UNIT_BYTES;
        if units > PLANE_STRIDE_MAX {
            return Err(PipeError::StrideTooWide {
                stride_bytes,
                units,
            });
        }
        Ok(units)
    }
}

/// A visible, unscaled packed-RGB plane candidate for pure DBUF planning.
///
/// `source_width/height` describe the complete linear framebuffer image;
/// `dst_*` describe its unscaled destination rectangle on the mode.
/// `allocation_bytes` is the backing framebuffer size, so the seam can reject
/// a stride/extent that scans beyond it. `plane_index` is the translated i915
/// watermark model's plane index (0..`skl_watermark_full::PLANES`), not a
/// Native register address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlaneScanout {
    pub(crate) plane_index: usize,
    pub(crate) surface: PlaneSurface,
    pub(crate) allocation_bytes: u64,
    pub(crate) pixel_format: u32,
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
    pub(crate) dst_x: u32,
    pub(crate) dst_y: u32,
    pub(crate) dst_width: u32,
    pub(crate) dst_height: u32,
}

/// One validated plane's allocated DBUF range and translated watermark values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlannedPlaneDdb {
    pub(crate) plane_index: usize,
    pub(crate) ddb: DdbAllocation,
    pub(crate) watermark: WatermarkProgram,
}

/// A pure, conservative minimum-allocation DBUF plan.
///
/// This is only an input to future atomic-state integration. It contains no
/// MMIO operations, does not program plane registers, and is not proof that
/// the selected topology is supported by the Native KMS writer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MultiPlaneDdbPlan {
    pub(crate) planes: Vec<PlannedPlaneDdb>,
    pub(crate) unused_blocks: u32,
}

/// Program the DBUF and all watermark levels produced by the multi-plane
/// planner.  The source model's `plane_index` is zero-based (`PLANE_1` is 0);
/// this writer deliberately admits only the four packed-RGB plane engines,
/// not the cursor, whose format/WM model is different.  Register offsets are
/// formed only after validating the bounded pipe/plane indices and are still
/// passed through `Registers`, whose DE window rejects non-writable or
/// unmapped MMIO.
///
/// This is the shadow half of `skl_allocate_pipe_ddb()` /
/// `skl_write_plane_wm()`; callers must arm the plane only after every write
/// succeeds.  DDB ranges are half-open internally and encoded by
/// `DdbAllocation::register_value()` as the hardware's inclusive end.
pub(crate) fn program_multi_plane_dbuf(
    regs: &impl Registers,
    pipe: Pipe,
    plan: &MultiPlaneDdbPlan,
) -> Result<(), PipeError> {
    if plan.planes.is_empty() {
        return Err(PipeError::NoVisiblePlanes);
    }
    let mut seen = [false; 4];
    // Validate the entire plan before its first MMIO write.  In particular,
    // never write a partial DDB repartition because a later plane is invalid.
    for plane in &plan.planes {
        if plane.plane_index >= seen.len() {
            return Err(PipeError::PlaneIndexOutOfRange {
                plane_index: plane.plane_index,
            });
        }
        if core::mem::replace(&mut seen[plane.plane_index], true) {
            return Err(PipeError::DuplicatePlaneIndex {
                plane_index: plane.plane_index,
            });
        }
        if plane.ddb.start() >= plane.ddb.end() || plane.ddb.end() > DDB_BLOCKS {
            return Err(PipeError::DdbAllocationOutOfBounds {
                start: plane.ddb.start(),
                end: plane.ddb.end(),
            });
        }
        if plan.planes.iter().any(|other| {
            other.plane_index != plane.plane_index
                && plane.ddb.start() < other.ddb.end()
                && other.ddb.start() < plane.ddb.end()
        }) {
            return Err(PipeError::DdbAllocationOverlap {
                first: plane.plane_index,
                second: plan
                    .planes
                    .iter()
                    .find(|other| {
                        other.plane_index != plane.plane_index
                            && plane.ddb.start() < other.ddb.end()
                            && other.ddb.start() < plane.ddb.end()
                    })
                    .map(|other| other.plane_index)
                    .unwrap_or(plane.plane_index),
            });
        }
    }

    let mut writes = Vec::with_capacity(plan.planes.len() * (1 + PLANE_WM_LEVELS + 3));
    for plane in &plan.planes {
        let regs_for_plane = plane_registers(pipe, plane.plane_index)?;
        writes.push(PlannedWrite {
            register: regs_for_plane.ddb,
            value: plane.ddb.register_value(),
        });
        for (level, value) in plane.watermark.levels().iter().enumerate() {
            writes.push(PlannedWrite {
                register: regs_for_plane.level(level),
                value: value.register_value(),
            });
        }
        writes.push(PlannedWrite {
            register: regs_for_plane.trans,
            value: plane.watermark.transition_value(),
        });
        writes.push(PlannedWrite {
            register: regs_for_plane.sagv,
            value: plane.watermark.sagv_value(),
        });
        writes.push(PlannedWrite {
            register: regs_for_plane.sagv_trans,
            value: plane.watermark.sagv_transition_value(),
        });
    }
    for planned in writes {
        write(regs, planned)?;
    }
    Ok(())
}

/// Program an unscaled packed-RGB multi-plane update from the already checked
/// DDB plan.  All buffer and watermark registers are written before any plane
/// arm, then each plane writes `PLANE_CTL` immediately followed by
/// `PLANE_SURF`, matching `skl_universal_plane`'s update ordering.  Unsupported
/// formats, clipping, scaling and overlap are rejected by the shared planner.
pub(crate) fn update_multi_plane_scanout(
    regs: &impl Registers,
    pipe: Pipe,
    mode: &Mode,
    scanouts: &[PlaneScanout],
    config: WatermarkConfig,
) -> Result<MultiPlaneDdbPlan, PipeError> {
    let plan = plan_multi_plane_dbuf(mode, scanouts, config)?;
    program_multi_plane_dbuf(regs, pipe, &plan)?;

    // Construct every plane's shadow/arm values before writing any of them.
    let mut plane_writes = Vec::with_capacity(scanouts.len() * 7);
    let mut arms = Vec::with_capacity(scanouts.len() * 2);
    for scanout in scanouts {
        let hw = plane_scanout_registers(pipe, scanout.plane_index)?;
        let (_, format, arb_slots) = plane_format_fields(scanout.pixel_format)?;
        let stride = PlaneProgram::stride_field(scanout.surface.stride_bytes)?;
        let surface = PlaneProgram::surface_field(scanout.surface.ggtt_address)?;
        let pos = (scanout.dst_y << 16) | scanout.dst_x;
        let size = ((scanout.dst_height - 1) << 16) | (scanout.dst_width - 1);
        plane_writes.extend([
            PlannedWrite {
                register: hw.stride,
                value: stride,
            },
            PlannedWrite {
                register: hw.pos,
                value: pos,
            },
            PlannedWrite {
                register: hw.size,
                value: size,
            },
            PlannedWrite {
                register: hw.offset,
                value: 0,
            },
            PlannedWrite {
                register: hw.color_ctl,
                value: PLANE_COLOR_CTL_LINEAR_RGB,
            },
        ]);
        arms.extend([
            PlannedWrite {
                register: hw.ctl,
                value: PLANE_CTL_ENABLE | format | PLANE_CTL_TILED_LINEAR | arb_slots,
            },
            PlannedWrite {
                register: hw.surface,
                value: surface,
            },
        ]);
    }
    for planned in plane_writes.into_iter().chain(arms) {
        write(regs, planned)?;
    }
    Ok(plan)
}

/// Disable one supported non-cursor plane, preserving the other control bits
/// while clearing only `PLANE_CTL_ENABLE`.  The disable control write is its
/// arm; no surface register is changed.
pub(crate) fn disable_multi_plane(
    regs: &impl Registers,
    pipe: Pipe,
    plane_index: usize,
) -> Result<(), PipeError> {
    let hw = plane_scanout_registers(pipe, plane_index)?;
    let before = read(regs, hw.ctl)?;
    write(
        regs,
        PlannedWrite {
            register: hw.ctl,
            value: before & !PLANE_CTL_ENABLE,
        },
    )
}

struct PlaneScanoutRegisters {
    stride: Register,
    pos: Register,
    size: Register,
    offset: Register,
    ctl: Register,
    surface: Register,
    color_ctl: Register,
}

fn plane_scanout_registers(
    pipe: Pipe,
    plane_index: usize,
) -> Result<PlaneScanoutRegisters, PipeError> {
    if plane_index >= 4 {
        return Err(PipeError::PlaneIndexOutOfRange { plane_index });
    }
    let base = 0x7_0180 + pipe.index() * 0x1000 + (plane_index as u32) * 0x100;
    Ok(PlaneScanoutRegisters {
        ctl: Register::read_write("PLANE_CTL_MULTI", base, Meaning::BringUp, None),
        stride: Register::read_write("PLANE_STRIDE_MULTI", base + 8, Meaning::BringUp, None),
        pos: Register::read_write("PLANE_POS_MULTI", base + 0xc, Meaning::BringUp, None),
        size: Register::read_write("PLANE_SIZE_MULTI", base + 0x10, Meaning::BringUp, None),
        offset: Register::read_write("PLANE_OFFSET_MULTI", base + 0x14, Meaning::BringUp, None),
        surface: Register::read_write("PLANE_SURF_MULTI", base + 0x1c, Meaning::BringUp, None),
        color_ctl: Register::read_write(
            "PLANE_COLOR_CTL_MULTI",
            base + 0x4c,
            Meaning::BringUp,
            None,
        ),
    })
}

struct MultiPlaneRegisters {
    ddb: Register,
    trans: Register,
    sagv: Register,
    sagv_trans: Register,
    wm_base: u32,
}

impl MultiPlaneRegisters {
    fn level(&self, level: usize) -> Register {
        debug_assert!(level < PLANE_WM_LEVELS);
        Register::read_write(
            match level {
                0 => "PLANE_WM_MULTI_0",
                1 => "PLANE_WM_MULTI_1",
                2 => "PLANE_WM_MULTI_2",
                3 => "PLANE_WM_MULTI_3",
                4 => "PLANE_WM_MULTI_4",
                _ => "PLANE_WM_MULTI_5",
            },
            self.wm_base + (level as u32) * 4,
            Meaning::BringUp,
            None,
        )
    }
}

/// Exact Gen12 plane-register mapping from `skl_universal_plane_regs.h`:
/// pipe stride `0x1000`, plane stride `0x100`, WM block `+0x240`, DBUF `+0x27c`.
/// Restricting the index before arithmetic prevents it becoming an arbitrary
/// DE MMIO offset.  The same source mapping is used for pipes A-D.
fn plane_registers(pipe: Pipe, plane_index: usize) -> Result<MultiPlaneRegisters, PipeError> {
    if plane_index >= 4 {
        return Err(PipeError::PlaneIndexOutOfRange { plane_index });
    }
    let base = 0x7_0180 + pipe.index() * 0x1000 + (plane_index as u32) * 0x100;
    let ddb = Register::read_write(
        match plane_index {
            0 => "PLANE_BUF_CFG_MULTI_1",
            1 => "PLANE_BUF_CFG_MULTI_2",
            2 => "PLANE_BUF_CFG_MULTI_3",
            _ => "PLANE_BUF_CFG_MULTI_4",
        },
        base + 0xfc,
        Meaning::BringUp,
        None,
    );
    let wm_base = base + 0xc0;
    Ok(MultiPlaneRegisters {
        ddb,
        wm_base,
        trans: Register::read_write(
            "PLANE_WM_MULTI_TRANS",
            wm_base + 0x28,
            Meaning::BringUp,
            None,
        ),
        sagv: Register::read_write(
            "PLANE_WM_MULTI_SAGV",
            wm_base + 0x20,
            Meaning::BringUp,
            None,
        ),
        sagv_trans: Register::read_write(
            "PLANE_WM_MULTI_SAGV_TRANS",
            wm_base + 0x24,
            Meaning::BringUp,
            None,
        ),
    })
}

/// Validate and size non-overlapping, unscaled packed-RGB surfaces using the
/// translated i915 single-plane watermark helper and its DDB minimums.
///
/// Only the existing XRGB8888/RGB565 linear encodings are accepted;
/// cursor/planar formats, scaling, clipping, overlap, and rectangles outside
/// the active mode are rejected. Plane pixel-rate inputs conservatively use
/// the full mode clock for each visible plane. The planner allocates each
/// plane its maximum source watermark minimum, leaving any spare DBUF unused
/// instead of guessing an optimization. It performs no register access.
pub(crate) fn plan_multi_plane_dbuf(
    mode: &Mode,
    scanouts: &[PlaneScanout],
    config: WatermarkConfig,
) -> Result<MultiPlaneDdbPlan, PipeError> {
    use intel_display::skl_watermark_full as wm;

    if scanouts.is_empty() {
        return Err(PipeError::NoVisiblePlanes);
    }
    let _ = timing::timing_registers(mode)?;

    let mut seen = [false; wm::PLANES];
    let mut requirements = Vec::with_capacity(scanouts.len());
    for (index, scanout) in scanouts.iter().enumerate() {
        if scanout.plane_index >= wm::PLANES {
            return Err(PipeError::PlaneIndexOutOfRange {
                plane_index: scanout.plane_index,
            });
        }
        if core::mem::replace(&mut seen[scanout.plane_index], true) {
            return Err(PipeError::DuplicatePlaneIndex {
                plane_index: scanout.plane_index,
            });
        }
        if scanout.dst_width == 0
            || scanout.dst_height == 0
            || scanout.source_width == 0
            || scanout.source_height == 0
        {
            return Err(PipeError::EmptyPlaneRect {
                plane_index: scanout.plane_index,
            });
        }
        let right = scanout.dst_x.checked_add(scanout.dst_width).ok_or(
            PipeError::PlaneRectOutOfBounds {
                plane_index: scanout.plane_index,
            },
        )?;
        let bottom = scanout.dst_y.checked_add(scanout.dst_height).ok_or(
            PipeError::PlaneRectOutOfBounds {
                plane_index: scanout.plane_index,
            },
        )?;
        if right > u32::from(mode.hdisplay) || bottom > u32::from(mode.vdisplay) {
            return Err(PipeError::PlaneRectOutOfBounds {
                plane_index: scanout.plane_index,
            });
        }
        if scanout.source_width != scanout.dst_width || scanout.source_height != scanout.dst_height
        {
            return Err(PipeError::PlaneScalingUnsupported {
                plane_index: scanout.plane_index,
            });
        }
        let cpp = pixel_format_cpp(scanout.pixel_format)?;
        let row_bytes = u64::from(scanout.source_width)
            .checked_mul(u64::from(cpp))
            .ok_or(PipeError::FramebufferExtentInvalid {
                plane_index: scanout.plane_index,
            })?;
        if u64::from(scanout.surface.stride_bytes) < row_bytes {
            return Err(PipeError::FramebufferStrideTooShort {
                plane_index: scanout.plane_index,
            });
        }
        let required_bytes = u64::from(scanout.source_height - 1)
            .checked_mul(u64::from(scanout.surface.stride_bytes))
            .and_then(|bytes| bytes.checked_add(row_bytes))
            .ok_or(PipeError::FramebufferExtentInvalid {
                plane_index: scanout.plane_index,
            })?;
        if required_bytes > scanout.allocation_bytes {
            return Err(PipeError::FramebufferAllocationTooSmall {
                plane_index: scanout.plane_index,
                required_bytes,
                allocation_bytes: scanout.allocation_bytes,
            });
        }
        // Reuse the established Native surface checks; this is validation only
        // and performs no hardware access.
        let _ = PlaneProgram::stride_field(scanout.surface.stride_bytes)?;
        let _ = PlaneProgram::surface_field(scanout.surface.ggtt_address)?;

        for previous in &scanouts[..index] {
            let previous_right = previous.dst_x + previous.dst_width;
            let previous_bottom = previous.dst_y + previous.dst_height;
            if scanout.dst_x < previous_right
                && previous.dst_x < right
                && scanout.dst_y < previous_bottom
                && previous.dst_y < bottom
            {
                return Err(PipeError::PlaneRectOverlap {
                    first: previous.plane_index,
                    second: scanout.plane_index,
                });
            }
        }

        let (_, minimum) = WatermarkProgram::from_i915_plane(
            DdbAllocation::WHOLE_BUFFER,
            mode,
            config,
            scanout.pixel_format,
            scanout.plane_index,
            scanout.source_width,
            mode.clock_khz,
        )?;
        if minimum == 0 {
            return Err(PipeError::PlaneDdbMinimumZero {
                plane_index: scanout.plane_index,
            });
        }
        requirements.push((minimum, scanout.plane_index));
    }

    let required_blocks = requirements
        .iter()
        .try_fold(0u32, |sum, (blocks, _)| sum.checked_add(*blocks))
        .ok_or(PipeError::DdbRequirementsOverflow)?;
    if required_blocks > DDB_BLOCKS {
        return Err(PipeError::DdbInsufficient {
            required_blocks,
            available_blocks: DDB_BLOCKS,
        });
    }

    let mut plans = Vec::with_capacity(scanouts.len());
    let mut start = 0u32;
    for (scanout, (blocks, plane_index)) in scanouts.iter().zip(requirements) {
        debug_assert_eq!(scanout.plane_index, plane_index);
        let end = start
            .checked_add(blocks)
            .ok_or(PipeError::DdbRequirementsOverflow)?;
        let ddb = DdbAllocation::checked(start, end)?;
        let (watermark, _) = WatermarkProgram::from_i915_plane(
            ddb,
            mode,
            config,
            scanout.pixel_format,
            scanout.plane_index,
            scanout.source_width,
            mode.clock_khz,
        )?;
        plans.push(PlannedPlaneDdb {
            plane_index: scanout.plane_index,
            ddb,
            watermark,
        });
        start = end;
    }
    Ok(MultiPlaneDdbPlan {
        planes: plans,
        unused_blocks: DDB_BLOCKS - start,
    })
}

/// Everything phase 3.4 and phase 4 write, computed before anything is written.
///
/// [`compute`] is the only constructor, so a value that reaches [`program`] has
/// already passed every check this module makes.  [`Self::writes`] turns it into
/// the exact ordered write list, which is what a caller logs and what
/// [`program`] executes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PipeProgram {
    /// Which pipe the program is for.
    pub(crate) pipe: Pipe,
    /// The seven timing register values, straight from `timing.rs`.
    pub(crate) timings: TimingRegisters,
    /// The mode the timings came from, kept for logging and for `PLANE_SIZE`.
    pub(crate) mode: Mode,
    /// The primary plane's DDB allocation: one plane over the whole DBUF.
    pub(crate) ddb: DdbAllocation,
    /// Every watermark level of the primary plane.
    pub(crate) watermark: WatermarkProgram,
    /// The primary plane's register values.
    pub(crate) plane: PlaneProgram,
    /// The bits of `PIPE_MISC` this module owns: 8 bpc, dithering off.
    pub(crate) pipe_misc: u32,
}

/// One register write, with its value, computed before any of them happen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlannedWrite {
    pub(crate) register: Register,
    pub(crate) value: u32,
}

/// How many writes [`PipeProgram::writes`] performs: the shadow half of the
/// program, all of which is latched by the plane's arm.
///
/// The terms are that function's groups, in order: the pipe's own two
/// ([`Pipe::misc`] and [`Pipe::arb_ctl`]), the timing table's
/// ([`timing::TIMING_REGISTERS`]), `PLANE_BUF_CFG`, the six watermark levels,
/// `PLANE_WM_TRANS` and the SAGV pair, and the plane's three `noarm` registers
/// plus `PLANE_OFFSET` and `PLANE_COLOR_CTL`.  It exists so that the `Vec`'s
/// capacity and the tests' count are the same number as the list rather than a
/// second, hand-kept one: the first version of this file reserved 23 slots for
/// 24 writes.
pub(crate) const PIPE_PROGRAM_WRITES: usize =
    2 + timing::TIMING_REGISTERS + 1 + PLANE_WM_LEVELS + 3 + 3 + 2;

/// How many writes [`arm`] performs: the pair that arms the plane.
///
/// `PLANE_CTL` and then `PLANE_SURF`, adjacent and in that order, which is the
/// one ordering section 5.6 states absolutely.  They are a separate list
/// because they are a separate *step*: see [`arm`].
pub(crate) const PLANE_ARM_WRITES: usize = 2;

impl PipeProgram {
    /// The `PIPE_MISC` value this program wants, given what the register holds
    /// now.
    ///
    /// A read-modify-write of [`PIPE_MISC_OWNED_MASK`] only: section 8.4 puts
    /// the output depth in `[7:5]` and dithering in `[4:2]`, and every other bit
    /// of the register belongs to a colour format, an HDR mode or PSR that this
    /// bring-up does not program.  Writing the whole register would clear them
    /// on the strength of no source at all.
    pub(crate) const fn pipe_misc_value(&self, before: u32) -> u32 {
        (before & !PIPE_MISC_OWNED_MASK) | self.pipe_misc
    }

    /// The `PIPE_ARB_CTL` value this program wants, given what the register
    /// holds now.
    ///
    /// One bit, set and nothing cleared: `[I915]` writes it as
    /// `intel_de_rmw(dev_priv, PIPE_ARB_CTL(pipe), 0, PIPE_ARB_USE_PROG_SLOTS)`
    /// (`display/intel_display.c:443-445`), whose clear mask is **zero**.  The
    /// register may hold settings for the pipe's own arbitration -- the
    /// reference's section 5.2 table lists only `USE_PROG_SLOTS`, which is a
    /// reason this module has no source for the rest rather than a reason to
    /// write them as zero.
    pub(crate) const fn arb_ctl_value(&self, before: u32) -> u32 {
        before | PIPE_ARB_USE_PROG_SLOTS
    }

    /// Every shadow write this program performs, in the order it performs
    /// them, given `PIPE_MISC`'s and `PIPE_ARB_CTL`'s current contents.
    ///
    /// The order is section 11's phases -- 3.4 timings, 4.1 DDB, 4.2 watermarks,
    /// 4.3 plane -- with `PIPE_MISC` first.  Every one of these registers is
    /// double buffered, and none of them takes effect until the plane's arm
    /// latches them at a vblank: [`Self::writes_arm`] is that step, and it is a
    /// separate entry point deliberately (see [`arm`]).
    ///
    /// `PIPE_MISC` is first rather than last for a reason that is worth stating,
    /// because section 11 puts it in step 5.6, after the plane: it is not double
    /// buffered, it arms nothing, and the property section 5.6 cares about is
    /// that nothing in the plane's double-buffered state is committed before the
    /// watermarks are -- which the arm pair being written after all of this is
    /// what guarantees.  Keeping it here means one call programs everything the
    /// pipe owns.
    /// `[I915]` gives the same ordering: `bdw_set_pipe_misc` runs before
    /// `hsw_configure_cpu_transcoder` in `intel_crtc_enable_pipe`
    /// (`display/intel_display.c:1719` against `:1723`), so the depth is
    /// programmed before the pipe is enabled there too.
    ///
    /// `PIPE_ARB_CTL` follows it, and belongs with it for the same reasons: it
    /// is the pipe half of `Wa_22012358565:adl-p`, the plane half is a field of
    /// `PLANE_CTL` (`display/skl_universal_plane.c:1090-1092`), and `[I915]`
    /// writes the pipe half in `intel_enable_transcoder`
    /// (`display/intel_display.c:441-445`) -- that is, before the plane that
    /// reads `ARB_SLOTS` is armed, which is the property this order keeps.  The
    /// write sets one bit and preserves the rest, like `PIPE_MISC`'s.
    pub(crate) fn writes(&self, pipe_misc_before: u32, arb_ctl_before: u32) -> Vec<PlannedWrite> {
        let mut writes = Vec::with_capacity(PIPE_PROGRAM_WRITES);

        // The pipe's output depth.  Section 11 step 5.6, written here so that
        // one call owns the pipe; see the method documentation.
        writes.push(PlannedWrite {
            register: self.pipe.misc(),
            value: self.pipe_misc_value(pipe_misc_before),
        });

        // The pipe's arbiter slots.  Read-modify-written because only bit 13
        // is this workaround's; see the method documentation.
        writes.push(PlannedWrite {
            register: self.pipe.arb_ctl(),
            value: self.arb_ctl_value(arb_ctl_before),
        });

        // Phase 3.4.  The order and the values are `timing.rs`'s.
        for (register, value) in self.timings.in_write_order() {
            writes.push(PlannedWrite {
                register: self.pipe.timing(register),
                value,
            });
        }

        // Phase 4.1.
        writes.push(PlannedWrite {
            register: self.pipe.plane_buf_cfg(),
            value: self.ddb.register_value(),
        });

        // Phase 4.2.  Every level, enabled or not.
        for (level, watermark) in self.watermark.levels().iter().enumerate() {
            writes.push(PlannedWrite {
                register: self.pipe.plane_wm(level),
                value: watermark.register_value(),
            });
        }

        // Phase 4.2's three non-level watermark registers, in `[I915]`'s order
        // around them: the transition watermark immediately after the levels
        // (`display/skl_universal_plane.c:735-749`), then the SAGV pair under
        // `HAS_HW_SAGV_WM` (`:743-748`).
        writes.push(PlannedWrite {
            register: self.pipe.plane_wm_trans(),
            value: self.watermark.transition_value(),
        });
        writes.push(PlannedWrite {
            register: self.pipe.plane_wm_sagv(),
            value: self.watermark.sagv_value(),
        });
        writes.push(PlannedWrite {
            register: self.pipe.plane_wm_sagv_trans(),
            value: self.watermark.sagv_transition_value(),
        });

        // Phase 4.3, the `noarm` half.
        writes.push(PlannedWrite {
            register: self.pipe.plane_stride(),
            value: self.plane.stride,
        });
        writes.push(PlannedWrite {
            register: self.pipe.plane_pos(),
            value: self.plane.pos,
        });
        writes.push(PlannedWrite {
            register: self.pipe.plane_size(),
            value: self.plane.size,
        });

        // Phase 4.3's `arm`-group registers that are *not* the arm pair.
        // Everything here is still shadow state that the arm latches; section
        // 5.6 lists the key and auxiliary-plane registers between them and
        // `PLANE_CTL`, and those never existed in the table -- no section gives
        // them an offset -- so the run is `PLANE_OFFSET`, `PLANE_COLOR_CTL`.
        writes.push(PlannedWrite {
            register: self.pipe.plane_offset(),
            value: self.plane.offset,
        });
        writes.push(PlannedWrite {
            register: self.pipe.plane_color_ctl(),
            value: self.plane.color_ctl,
        });

        writes
    }

    /// The pair that arms the plane: `PLANE_CTL`, then `PLANE_SURF`.
    ///
    /// Two writes, adjacent and in that order, which is the one ordering
    /// section 5.6 states absolutely -- "the control register self-arms if the
    /// plane was previously disabled.  Try to make the plane enable atomic by
    /// writing the control register just before the surface register"
    /// (`display/skl_universal_plane.c:1525-1532`, the two writes at
    /// `:1530-1531`) -- and they are a separate list from [`Self::writes`]
    /// because they are a separate *step* in time.  See [`arm`].
    pub(crate) fn writes_arm(&self) -> [PlannedWrite; PLANE_ARM_WRITES] {
        [
            PlannedWrite {
                register: self.pipe.plane_ctl(),
                value: self.plane.ctl,
            },
            // Section 5.6: "PLANE_SURF <- GGTT address <-- THIS ARMS EVERYTHING".
            PlannedWrite {
                register: self.pipe.plane_surf(),
                value: self.plane.surf,
            },
        ]
    }

    /// The whole program as log lines, one per write, shadow half then arm
    /// pair.
    ///
    /// Built as a string rather than logged as it goes because the sequence
    /// cannot be run on the target yet, so the text has to be something a host
    /// test can assert on.  [`Self::log`] puts the same text in the kernel log.
    /// The arm pair is rendered too, marked with the step it belongs to, so
    /// that the log shows the whole sequence even though two calls perform it.
    ///
    /// `pipe_misc_before` and `arb_ctl_before` are what those two registers
    /// hold now, which is what their read-modify-writes are applied to; a
    /// caller that wants the exact values reads the registers first, and one
    /// that only wants the rest of the program can pass zeroes.
    pub(crate) fn render(&self, pipe_misc_before: u32, arb_ctl_before: u32) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "intel-pipe: pipe {} mode {mode}, reference section 11 phases 3.4 and 4\n",
            self.pipe,
            mode = self.mode,
        ));
        out.push_str(&format!(
            "intel-pipe: pipe {} surface {} stride {} bytes\n",
            self.pipe,
            hex(u64::from(self.plane.surf), 8),
            self.plane.stride_bytes,
        ));
        out.push_str(&format!(
            "intel-pipe: pipe {} DDB start {} end {} ({} blocks), PLANE_BUF_CFG = {:#010x}\n",
            self.pipe,
            self.ddb.start(),
            self.ddb.end(),
            self.ddb.blocks(),
            self.ddb.register_value(),
        ));
        for (level, watermark) in self.watermark.levels().iter().enumerate() {
            out.push_str(&format!(
                "intel-pipe: pipe {} WM{}: {:#010x} (enable {}, blocks {}, lines {})\n",
                self.pipe,
                level,
                watermark.register_value(),
                u8::from(watermark.is_enabled()),
                watermark.blocks(),
                watermark.lines(),
            ));
        }
        out.push_str(&format!(
            "intel-pipe: pipe {} WM SAGV {:#010x}, SAGV_TRANS {:#010x}, TRANS {:#010x}\n",
            self.pipe,
            self.watermark.sagv_value(),
            self.watermark.sagv_transition_value(),
            self.watermark.transition_value(),
        ));
        out.push_str(&format!(
            "intel-pipe: pipe {} PIPE_MISC {:#010x} -> {:#010x} (8 bpc, dither off, pixel \
             rounding truncated; other bits preserved)\n",
            self.pipe,
            pipe_misc_before,
            self.pipe_misc_value(pipe_misc_before),
        ));
        out.push_str(&format!(
            "intel-pipe: pipe {} PIPE_ARB_CTL {:#010x} -> {:#010x} (USE_PROG_SLOTS set, other \
             bits preserved)\n",
            self.pipe,
            arb_ctl_before,
            self.arb_ctl_value(arb_ctl_before),
        ));
        for write in self.writes(pipe_misc_before, arb_ctl_before) {
            out.push_str(&format!(
                "intel-pipe:   {} <- {:#010x}\n",
                write.register.name(),
                write.value
            ));
        }
        out.push_str(&format!(
            "intel-pipe: pipe {} arm (after the pipe is enabled):\n",
            self.pipe,
        ));
        for write in self.writes_arm() {
            out.push_str(&format!(
                "intel-pipe:   {} <- {:#010x}  (arm)\n",
                write.register.name(),
                write.value
            ));
        }
        out
    }

    /// Put [`Self::render`] into the kernel log.
    pub(crate) fn log(&self, pipe_misc_before: u32, arb_ctl_before: u32) {
        for line in self.render(pipe_misc_before, arb_ctl_before).lines() {
            axlog::debug!("{line}");
        }
    }
}

/// Turn a mode and a surface into every register value the pipe needs.
///
/// The timing arithmetic is [`timing::timing_registers`]'s and is not repeated
/// here; this function adds the DDB, the watermarks, the plane and the output
/// depth, and refuses a surface section 11 phase 3.2 would not accept.
///
/// The plane is the whole mode: section 11's preamble assumes "one plane,
/// linear XRGB8888, no scaling", so `PLANE_SIZE` is the mode's active size,
/// `PLANE_POS` and `PLANE_OFFSET` are zero, and the surface is expected to hold
/// at least that many pixels.  Whether the allocation is that large is the
/// allocator's guarantee, not this module's: a stride and an address cannot say
/// how many bytes follow the address.
#[cfg(test)]
pub(crate) fn compute(
    pipe: Pipe,
    mode: &Mode,
    surface: PlaneSurface,
) -> Result<PipeProgram, PipeError> {
    let ddb = DdbAllocation::WHOLE_BUFFER;
    let watermark = WatermarkProgram::generous(ddb);
    compute_with_program(
        pipe,
        mode,
        surface,
        ddb,
        watermark,
        intel_display::universal_plane::XRGB8888,
    )
}

/// Compute the active pipe program using i915's latency-derived watermark
/// policy, not the test-only first-light-up generous profile.
pub(crate) fn compute_with_watermarks(
    pipe: Pipe,
    mode: &Mode,
    surface: PlaneSurface,
    config: WatermarkConfig,
) -> Result<PipeProgram, PipeError> {
    let ddb = DdbAllocation::WHOLE_BUFFER;
    let format = intel_display::universal_plane::XRGB8888;
    let watermark = WatermarkProgram::from_i915(ddb, mode, config, format)?;
    compute_with_program(pipe, mode, surface, ddb, watermark, format)
}

/// Compute a source-watermarked primary plane for a format with a complete
/// source-derived register encoding and bytes-per-pixel profile.
pub(crate) fn compute_with_watermarks_format(
    pipe: Pipe,
    mode: &Mode,
    surface: PlaneSurface,
    config: WatermarkConfig,
    pixel_format: u32,
) -> Result<PipeProgram, PipeError> {
    let ddb = DdbAllocation::WHOLE_BUFFER;
    let watermark = WatermarkProgram::from_i915(ddb, mode, config, pixel_format)?;
    compute_with_program(pipe, mode, surface, ddb, watermark, pixel_format)
}

/// Program only the timing and per-pipe configuration required before a
/// multi-plane DDB/plane update.  Unlike [`program`], this deliberately does
/// not touch the primary plane, DDB partition, or watermark registers: those
/// are owned by [`update_multi_plane_scanout`] when a pipe has more than one
/// visible plane.
///
/// This is the `hsw_crtc_enable()` pipe-configuration portion for a KMS state
/// whose plane allocation was computed by the multi-plane planner.  The
/// caller still owns transcoder enable and plane arming order.
pub(crate) fn program_multi_plane_pipe_config(
    regs: &impl Registers,
    pipe: Pipe,
    mode: &Mode,
) -> Result<(), PipeError> {
    let timings = timing::timing_registers(mode).map_err(PipeError::Timing)?;
    let misc_before = read(regs, pipe.misc())?;
    let arb_before = read(regs, pipe.arb_ctl())?;
    let writes = [
        PlannedWrite {
            register: pipe.misc(),
            value: (misc_before & !PIPE_MISC_OWNED_MASK)
                | PIPE_MISC_BPC_8
                | PIPE_MISC_PIXEL_ROUNDING_TRUNC,
        },
        PlannedWrite {
            register: pipe.arb_ctl(),
            value: arb_before | PIPE_ARB_USE_PROG_SLOTS,
        },
    ];
    // Validate the complete timing sequence before the first MMIO write.
    let timing_writes: Vec<_> = timings
        .in_write_order()
        .into_iter()
        .map(|(register, value)| PlannedWrite {
            register: pipe.timing(register),
            value,
        })
        .collect();
    for planned in writes.into_iter().chain(timing_writes) {
        write(regs, planned)?;
    }
    Ok(())
}

fn plane_format_fields(pixel_format: u32) -> Result<(u32, u32, u32), PipeError> {
    use intel_display::skl_universal_plane_full as source;

    let cpp = match pixel_format {
        intel_display::universal_plane::XRGB8888 => 4,
        intel_display::universal_plane::RGB565 => 2,
        _ => return Err(PipeError::UnsupportedFormat { pixel_format }),
    };
    let (source_format, yuv) = match pixel_format {
        intel_display::universal_plane::XRGB8888 => (source::DRM_FORMAT_XRGB8888, false),
        intel_display::universal_plane::RGB565 => (source::DRM_FORMAT_RGB565, false),
        _ => return Err(PipeError::UnsupportedFormat { pixel_format }),
    };
    let format_info = source::FormatInfo {
        cpp: [cpp as u8, 0, 0, 0],
        planes: 1,
        yuv_semiplanar: false,
        is_yuv: yuv,
    };
    let ctl = match source_format {
        source::DRM_FORMAT_XRGB8888 => source::FMT_XRGB8888,
        source::DRM_FORMAT_RGB565 => source::FMT_RGB565,
        _ => return Err(PipeError::UnsupportedFormat { pixel_format }),
    };
    let arb_slots = source::adlp_plane_ctl_arb_slots(format_info);
    Ok((cpp, ctl, arb_slots))
}

fn pixel_format_cpp(pixel_format: u32) -> Result<u32, PipeError> {
    plane_format_fields(pixel_format).map(|(cpp, ..)| cpp)
}

fn compute_with_program(
    pipe: Pipe,
    mode: &Mode,
    surface: PlaneSurface,
    ddb: DdbAllocation,
    watermark: WatermarkProgram,
    pixel_format: u32,
) -> Result<PipeProgram, PipeError> {
    let (_cpp, format_ctl, arb_slots) = plane_format_fields(pixel_format)?;
    let timings = timing::timing_registers(mode)?;
    let plane = PlaneProgram {
        stride: PlaneProgram::stride_field(surface.stride_bytes)?,
        stride_bytes: surface.stride_bytes,
        // A full-screen plane at the origin: section 11's preamble assumes no
        // scaling and no panning.
        pos: 0,
        // `PLANE_SIZE`'s halves are `PIPESRC`'s the other way round (§5.4
        // against §5.2), so the two `count - 1` fields come from `timing.rs`
        // and no subtraction happens here.
        size: timings.pipesrc().rotate_left(16),
        offset: 0,
        color_ctl: PLANE_COLOR_CTL_LINEAR_RGB,
        ctl: PLANE_CTL_ENABLE | format_ctl | PLANE_CTL_TILED_LINEAR | arb_slots,
        surf: PlaneProgram::surface_field(surface.ggtt_address)?,
    };
    Ok(PipeProgram {
        pipe,
        timings,
        mode: *mode,
        ddb,
        watermark,
        plane,
        // 8 bpc, dithering off, pixel rounding truncated.  An XRGB8888 surface
        // is 8 bits per component and the sink is being driven at 8 bpc, so
        // there is no depth conversion to dither; section 8.4's `[INF]` warns
        // against enabling what the simple case does not need.  The rounding
        // bit is `[I915]`'s for display version 12 and later and the reference
        // never mentions it -- see `PIPE_MISC_PIXEL_ROUNDING_TRUNC`.
        pipe_misc: PIPE_MISC_BPC_8 | PIPE_MISC_PIXEL_ROUNDING_TRUNC,
    })
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Why a pipe program could not be computed or written.
///
/// Named the way [`super::power::PowerError`] is, and for the same reason: the
/// target machine's only output device is the screen this program is trying to
/// turn on, so an error that says which register and which value is the
/// difference between a diagnosis and a guess.  [`Self::describe`] is the
/// rendered form the boot log carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PipeError {
    /// The active primary-plane writer has no proven register encoding for
    /// this pixel format.
    UnsupportedFormat { pixel_format: u32 },
    /// A register the sequence cannot do without was outside the mapped window.
    Unreadable { register: &'static str },
    /// A write did not happen: the register is not declared writable, or it is
    /// outside the mapped window.
    WriteRefused { register: &'static str },
    /// `timing.rs` refused the mode.  It refuses an interlaced timing and a
    /// malformed one; both are deliberate and neither is worked around here.
    Timing(timing::TimingError),
    /// Source-derived latency watermarks could not be calculated for this DDB.
    Watermark(i32),
    /// There is no visible plane to plan.
    NoVisiblePlanes,
    /// The plane index does not exist in the translated watermark model.
    PlaneIndexOutOfRange { plane_index: usize },
    /// Two candidates claim one hardware-plane slot.
    DuplicatePlaneIndex { plane_index: usize },
    /// A visible source or destination rectangle has zero extent.
    EmptyPlaneRect { plane_index: usize },
    /// The destination rectangle overflows or extends past the active mode.
    PlaneRectOutOfBounds { plane_index: usize },
    /// Two destination rectangles overlap; this seam does not model z-order.
    PlaneRectOverlap { first: usize, second: usize },
    /// The planner admits no scaling or implicit source crop.
    PlaneScalingUnsupported { plane_index: usize },
    /// The source surface's stride cannot hold one packed-RGB row.
    FramebufferStrideTooShort { plane_index: usize },
    /// Calculating the last visible byte overflowed.
    FramebufferExtentInvalid { plane_index: usize },
    /// The declared framebuffer allocation ends before the scanout extent.
    FramebufferAllocationTooSmall {
        plane_index: usize,
        required_bytes: u64,
        allocation_bytes: u64,
    },
    /// Source policy returned no minimum allocation for a visible plane.
    PlaneDdbMinimumZero { plane_index: usize },
    /// The summed per-plane minimum allocations overflowed their counter.
    DdbRequirementsOverflow,
    /// The planes' minimum watermarks do not fit the display buffer.
    DdbInsufficient {
        required_blocks: u32,
        available_blocks: u32,
    },
    /// A planner-generated DDB range is empty or outside the display buffer.
    DdbAllocationOutOfBounds { start: u32, end: u32 },
    /// Two planes claim overlapping DBUF blocks.
    DdbAllocationOverlap { first: usize, second: usize },
    /// The surface stride was zero.
    StrideZero,
    /// The stride is not a multiple of [`PLANE_STRIDE_UNIT_BYTES`], which
    /// section 11 phase 3.2 requires and the register's encoding needs.
    StrideNotAMultipleOf64 { stride_bytes: u32 },
    /// The stride does not fit `PLANE_STRIDE`'s twelve-bit field once divided
    /// into its 64-byte units.
    StrideTooWide { stride_bytes: u32, units: u32 },
    /// The surface address is zero, which arms a plane that reads nothing.
    SurfaceAddressZero,
    /// The surface address is not 4 KiB-aligned, so `PLANE_SURF`'s `[31:12]`
    /// cannot carry it.
    SurfaceMisaligned { address: u64 },
    /// The surface address needs more than `PLANE_SURF`'s 32 bits.
    SurfaceAboveAddressWindow { address: u64 },
}

impl PipeError {
    /// The error as one actionable line.
    ///
    /// Every variant says what was wrong, what the reference requires, and what
    /// to check -- because on this machine the reader of this line has a screen
    /// showing either a picture or nothing, and nothing else to go on.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::UnsupportedFormat { pixel_format } => format!(
                "primary-plane pixel format {pixel_format:#x} has no admitted linear source \
                 encoding"
            ),
            Self::Unreadable { register } => format!(
                "{register} could not be read: it is outside the mapped register window, so the \
                 display engine's state cannot be established.  Reference section 2.2"
            ),
            Self::WriteRefused { register } => format!(
                "{register} refused the write: either it is not declared writable in the register \
                 table, or it lies outside the mapped register window.  Nothing after it was \
                 written, so the plane is not armed"
            ),
            Self::Timing(error) => format!(
                "the mode could not be turned into timing registers: {error}.  Reference sections \
                 6.1 and 11 phase 3.4; this module does not compute a timing of its own"
            ),
            Self::Watermark(errno) => format!(
                "i915 primary-plane watermark calculation failed with errno {errno}; the plane is \
                 not armed"
            ),
            Self::NoVisiblePlanes => String::from(
                "the DBUF planner needs at least one visible plane; no empty atomic state is \
                 inferred",
            ),
            Self::PlaneIndexOutOfRange { plane_index } => format!(
                "plane index {plane_index} is outside the translated i915 watermark model; no \
                 allocation is produced"
            ),
            Self::DuplicatePlaneIndex { plane_index } => format!(
                "plane index {plane_index} appears more than once; one DDB range cannot describe \
                 two plane states"
            ),
            Self::EmptyPlaneRect { plane_index } => {
                format!("plane {plane_index} has an empty source or destination rectangle")
            }
            Self::PlaneRectOutOfBounds { plane_index } => format!(
                "plane {plane_index}'s destination rectangle overflows or extends beyond the \
                 active mode"
            ),
            Self::PlaneRectOverlap { first, second } => format!(
                "planes {first} and {second} have overlapping destination rectangles; this \
                 planner does not guess z-order or blend semantics"
            ),
            Self::PlaneScalingUnsupported { plane_index } => format!(
                "plane {plane_index} requests scaling or source crop, neither of which this \
                 linear packed-RGB planner models"
            ),
            Self::FramebufferStrideTooShort { plane_index } => format!(
                "plane {plane_index}'s framebuffer stride is shorter than its packed-RGB row"
            ),
            Self::FramebufferExtentInvalid { plane_index } => format!(
                "plane {plane_index}'s framebuffer scanout extent overflowed; no allocation is \
                 produced"
            ),
            Self::FramebufferAllocationTooSmall {
                plane_index,
                required_bytes,
                allocation_bytes,
            } => format!(
                "plane {plane_index} needs {required_bytes} bytes for its declared scanout \
                 extent, but the framebuffer allocation is {allocation_bytes} bytes"
            ),
            Self::PlaneDdbMinimumZero { plane_index } => format!(
                "plane {plane_index}'s translated watermark state has no nonzero minimum DBUF \
                 allocation; planner refuses an empty range"
            ),
            Self::DdbRequirementsOverflow => String::from(
                "the summed per-plane DBUF minimums overflowed; no wrapped allocation is used",
            ),
            Self::DdbInsufficient {
                required_blocks,
                available_blocks,
            } => format!(
                "visible planes require {required_blocks} DBUF blocks for all calculated \
                 watermarks, but only {available_blocks} blocks exist"
            ),
            Self::DdbAllocationOutOfBounds { start, end } => {
                format!("planner DBUF range [{start}, {end}) is empty or outside [0, {DDB_BLOCKS})")
            }
            Self::DdbAllocationOverlap { first, second } => format!(
                "planes {first} and {second} claim overlapping DBUF blocks; no DDB or watermark \
                 register was written"
            ),
            Self::StrideZero => String::from(
                "the surface stride is zero, which describes no scanline at all.  Reference \
                 section 11 phase 3.2: the stride is the framebuffer's pitch in bytes and must be \
                 a multiple of 64",
            ),
            Self::StrideNotAMultipleOf64 { stride_bytes } => format!(
                "the surface stride {stride_bytes} is not a multiple of {PLANE_STRIDE_UNIT_BYTES} \
                 bytes.  Reference section 11 phase 3.2 requires it, and PLANE_STRIDE counts \
                 64-byte units for a linear surface, so there is no way to express this stride: \
                 reallocate the framebuffer with a 64-byte-multiple pitch (256 is safest)"
            ),
            Self::StrideTooWide {
                stride_bytes,
                units,
            } => format!(
                "the surface stride {stride_bytes} is {units} 64-byte units, which does not fit \
                 PLANE_STRIDE's twelve-bit field (maximum {} units, {} bytes).  Reference section \
                 5.4 for the field, section 11 phase 3.2 for the alignment rule it comes from",
                PLANE_STRIDE_MAX,
                PLANE_STRIDE_MAX * PLANE_STRIDE_UNIT_BYTES,
            ),
            Self::SurfaceAddressZero => String::from(
                "the surface address is zero, which is how section 5.6 disables a plane rather \
                 than how it enables one: PLANE_SURF would arm a plane pointed at nothing.  \
                 Reference section 11 phase 4.3",
            ),
            Self::SurfaceMisaligned { address } => format!(
                "the surface address {} is not {PLANE_SURF_ALIGNMENT}-byte aligned, so \
                 PLANE_SURF's [31:12] cannot carry it.  Reference section 11 phase 3.2 requires a \
                 4 KiB-aligned base; a misaligned address is one of the two reasons step 4.3 \
                 gives for a PLANE_SURFLIVE that reads back zero",
                hex(*address, 8),
            ),
            Self::SurfaceAboveAddressWindow { address } => format!(
                "the surface address {} needs more than the 32 bits PLANE_SURF has.  Reference \
                 section 5.4: the register carries [31:12] of a GGTT address, so a scanout \
                 surface has to live in the first 4 GiB of the graphics address space",
                hex(*address, 16),
            ),
        }
    }
}

impl From<timing::TimingError> for PipeError {
    fn from(error: timing::TimingError) -> Self {
        Self::Timing(error)
    }
}

/// Read a register this sequence cannot do without.
fn read(regs: &impl Registers, register: Register) -> Result<u32, PipeError> {
    regs.read(register).ok_or(PipeError::Unreadable {
        register: register.name(),
    })
}

/// Write one planned register, refusing to continue if it did not happen.
fn write(regs: &impl Registers, planned: PlannedWrite) -> Result<(), PipeError> {
    if regs.write(planned.register, planned.value) {
        Ok(())
    } else {
        Err(PipeError::WriteRefused {
            register: planned.register.name(),
        })
    }
}

/// What phase 4's shadow half wrote, in the order it wrote it.
///
/// Kept rather than discarded so that the boot log can show what actually
/// happened next to what was planned, and so that a caller can hand the same
/// program to [`arm`] and then to [`prove`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PipeState {
    pub(crate) pipe: Pipe,
    /// `PIPE_MISC` as it read before the program's read-modify-write.
    pub(crate) pipe_misc_before: u32,
    /// `PIPE_MISC` as written.
    pub(crate) pipe_misc_after: u32,
    /// `PIPE_ARB_CTL` as it read before the program's read-modify-write.
    pub(crate) arb_ctl_before: u32,
    /// `PIPE_ARB_CTL` as written.
    pub(crate) arb_ctl_after: u32,
    /// Every shadow write that happened, in order.
    pub(crate) writes: Vec<PlannedWrite>,
}

impl PipeState {
    /// The same text the log carries, for `/sys/kernel/debug/dri/0/intel_gpu`.
    pub(crate) fn render(&self) -> String {
        let mut out = format!(
            "intel-pipe: pipe {} programmed with {} shadow writes (reference section 11 phases \
             3.4 and 4), not yet armed\n",
            self.pipe,
            self.writes.len(),
        );
        out.push_str(&format!(
            "intel-pipe: pipe {} PIPE_MISC {:#010x} -> {:#010x}\n",
            self.pipe, self.pipe_misc_before, self.pipe_misc_after,
        ));
        out.push_str(&format!(
            "intel-pipe: pipe {} PIPE_ARB_CTL {:#010x} -> {:#010x}\n",
            self.pipe, self.arb_ctl_before, self.arb_ctl_after,
        ));
        for write in &self.writes {
            out.push_str(&format!(
                "intel-pipe:   {} <- {:#010x}\n",
                write.register.name(),
                write.value
            ));
        }
        out
    }

    /// Put [`Self::render`] into the kernel log.
    pub(crate) fn log(&self) {
        for line in self.render().lines() {
            axlog::debug!("{line}");
        }
    }
}

/// What the arm step wrote, in order: `PLANE_CTL`, then `PLANE_SURF`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArmState {
    pub(crate) pipe: Pipe,
    /// The pair, in the order it was written.
    pub(crate) writes: [PlannedWrite; PLANE_ARM_WRITES],
}

impl ArmState {
    /// The same text the log carries, for `/sys/kernel/debug/dri/0/intel_gpu`.
    pub(crate) fn render(&self) -> String {
        let mut out = format!(
            "intel-pipe: pipe {} armed with {} writes (PLANE_CTL then PLANE_SURF)\n",
            self.pipe,
            self.writes.len(),
        );
        for write in &self.writes {
            out.push_str(&format!(
                "intel-pipe:   {} <- {:#010x}\n",
                write.register.name(),
                write.value
            ));
        }
        out
    }

    /// Put [`Self::render`] into the kernel log.
    pub(crate) fn log(&self) {
        for line in self.render().lines() {
            axlog::debug!("{line}");
        }
    }
}

/// Reference section 11 phase 4: write the whole shadow program, in order.
///
/// The write list is built before the first write, so a failure part-way
/// through cannot leave a value that was computed against a register that no
/// longer reads the same way, and so a caller can log exactly what will happen
/// before it happens with [`PipeProgram::render`].
///
/// A refused write stops the sequence and is reported by name.  Stopping is the
/// point: the values after the failure are the ones that arm the plane, and a
/// plane armed with a half-written configuration is the black screen section
/// 7.1 spends its words on.  Nothing written here has taken effect yet: every
/// register in the list is double buffered and [`arm`] is what latches them.
///
/// Nothing here enables the pipe either.  `TRANSCONF` is section 11 step 5.6
/// and belongs to the output workstream; until it is written, this program has
/// configured a pipe that is not scanning -- which is exactly the state the arm
/// step waits for the end of.
pub(crate) fn program(regs: &impl Registers, plan: &PipeProgram) -> Result<PipeState, PipeError> {
    // The two reads have to happen before the list is built, because
    // `PIPE_MISC` and `PIPE_ARB_CTL` are read-modify-writes; everything else in
    // the list is already in `plan`.
    let pipe_misc_before = read(regs, plan.pipe.misc())?;
    let arb_ctl_before = read(regs, plan.pipe.arb_ctl())?;
    let writes = plan.writes(pipe_misc_before, arb_ctl_before);
    let pipe_misc_after = plan.pipe_misc_value(pipe_misc_before);
    let arb_ctl_after = plan.arb_ctl_value(arb_ctl_before);

    for planned in &writes {
        write(regs, *planned)?;
    }

    Ok(PipeState {
        pipe: plan.pipe,
        pipe_misc_before,
        pipe_misc_after,
        arb_ctl_before,
        arb_ctl_after,
        writes,
    })
}

/// Arm the plane: `PLANE_CTL` and `PLANE_SURF`, adjacent, in that order.
///
/// **This is a separate step from [`program`], and the order of the two is the
/// point.**  `PLANE_SURF` is only the arm: the shadow registers `program`
/// writes are latched at the plane's update event, which is the pipe's vblank,
/// and while the transcoder is disabled there is no vblank to latch them at --
/// "Until the pipe starts PIPEDSL reads will return a stale value"
/// (`display/intel_display.c:478-486`).  A `PLANE_SURF` written before the pipe
/// runs therefore cannot take effect until after `TRANSCONF` is enabled, which
/// is step 5.6, and the sequence should not *depend* on a pre-enable write
/// latching later: `[I915]` never does.  It enables the crtc first
/// (`intel_enable_crtc`, `display/intel_display.c:7200`) and arms the plane
/// afterwards (`intel_update_crtc`, `:7249`), and its arm function writes the
/// two registers adjacently with the comment that the control register
/// self-arms a previously disabled plane
/// (`display/skl_universal_plane.c:1525-1532`).
///
/// Coreboot's libgfxinit arms before the enable instead and ships that order,
/// so the reference's ordering is probably not fatal -- but whether a pending
/// arm survives the enable is not documented anywhere this workstream could
/// find, which is exactly why the sequence no longer relies on it.  Callers run
/// [`program`], then the output workstream's `TRANSCONF`, then this.
///
/// A refused write stops the pair and is reported by name; a `PLANE_CTL` that
/// did not land means `PLANE_SURF` is not written at all, because a surface
/// address written over a plane control word that did not take is the
/// half-armed plane this module's ordering exists to prevent.
pub(crate) fn arm(regs: &impl Registers, plan: &PipeProgram) -> Result<ArmState, PipeError> {
    let writes = plan.writes_arm();
    for planned in &writes {
        write(regs, *planned)?;
    }
    Ok(ArmState {
        pipe: plan.pipe,
        writes,
    })
}

#[cfg(test)]
mod multi_plane_plan_tests {
    use alloc::vec;
    use super::*;
    use crate::drm::modes::CTA_VIC_TIMINGS;

    fn mode_1080p() -> Mode {
        CTA_VIC_TIMINGS
            .iter()
            .find(|entry| entry.vic == 16)
            .expect("VIC 16 exists in the CTA timing table")
            .mode
    }

    fn config() -> WatermarkConfig {
        WatermarkConfig {
            display_ver: 13,
            latencies: [2, 4, 6, 8, 14, 16, 0, 0],
            num_levels: 6,
            sagv_block_time_us: 0,
        }
    }

    #[test]
    fn multi_plane_pipe_config_programs_only_pipe_and_timing_registers() {
        let regs = crate::drm::intel::regs::mock::MockRegisters::new();
        program_multi_plane_pipe_config(&regs, Pipe::B, &mode_1080p())
            .expect("pipe timing and per-pipe configuration should be writable");

        let writes = regs.writes();
        assert_eq!(writes.len(), 2 + 7);
        assert_eq!(writes[0].0, "PIPE_MISC_B");
        assert_eq!(writes[1].0, "PIPE_ARB_CTL_B");
        assert_eq!(writes[2].0, "TRANS_HTOTAL_B");
        assert_eq!(writes.last().unwrap().0, "PIPESRC_B");
        assert!(writes.iter().all(|(name, _)| {
            !name.contains("PLANE") && !name.contains("WM") && !name.contains("BUF_CFG")
        }));
    }

    #[test]
    fn multi_plane_pipe_config_rejects_invalid_timing_before_mmio() {
        let regs = crate::drm::intel::regs::mock::MockRegisters::new();
        let mut mode = mode_1080p();
        mode.htotal = 0;
        assert!(matches!(
            program_multi_plane_pipe_config(&regs, Pipe::A, &mode),
            Err(PipeError::Timing(_))
        ));
        assert!(regs.writes().is_empty());
    }

    fn half_screen(plane_index: usize, dst_x: u32, ggtt_address: u64) -> PlaneScanout {
        PlaneScanout {
            plane_index,
            surface: PlaneSurface {
                ggtt_address,
                stride_bytes: 1920,
            },
            allocation_bytes: 1920 * 1080,
            pixel_format: intel_display::universal_plane::RGB565,
            source_width: 960,
            source_height: 1080,
            dst_x,
            dst_y: 0,
            dst_width: 960,
            dst_height: 1080,
        }
    }

    #[test]
    fn plans_two_disjoint_rgb565_planes_with_nonoverlapping_ddb_ranges() {
        let mode = mode_1080p();
        let plan = plan_multi_plane_dbuf(
            &mode,
            &[
                half_screen(0, 0, 0x0100_0000),
                half_screen(1, 960, 0x0200_0000),
            ],
            config(),
        )
        .expect("simple disjoint packed RGB plane set should be plannable");

        assert_eq!(plan.planes.len(), 2);
        assert_eq!(plan.planes[0].ddb.start(), 0);
        assert_eq!(plan.planes[0].ddb.end(), plan.planes[1].ddb.start());
        assert_eq!(plan.planes[1].ddb.end() + plan.unused_blocks, DDB_BLOCKS);
        assert!(
            plan.planes
                .iter()
                .all(|plane| plane.watermark.levels()[0].is_enabled())
        );
    }

    #[test]
    fn writes_each_plane_ddb_before_its_source_watermarks() {
        let mode = mode_1080p();
        let plan = plan_multi_plane_dbuf(
            &mode,
            &[
                half_screen(0, 0, 0x0100_0000),
                half_screen(1, 960, 0x0200_0000),
            ],
            config(),
        )
        .expect("two packed RGB planes should have a DDB plan");
        let regs = crate::drm::intel::regs::mock::MockRegisters::new();

        program_multi_plane_dbuf(&regs, Pipe::A, &plan)
            .expect("bounded DDB and watermark writes should be admitted");

        let writes = regs.writes();
        assert_eq!(writes.len(), 2 * (1 + PLANE_WM_LEVELS + 3));
        assert_eq!(writes[0].0, "PLANE_BUF_CFG_MULTI_1");
        assert_eq!(writes[1].0, "PLANE_WM_MULTI_0");
        assert_eq!(
            writes[1].1,
            plan.planes[0].watermark.levels()[0].register_value()
        );
        assert_eq!(writes[7].0, "PLANE_WM_MULTI_TRANS");
        assert_eq!(writes[8].0, "PLANE_WM_MULTI_SAGV");
        assert_eq!(writes[9].0, "PLANE_WM_MULTI_SAGV_TRANS");
        assert_eq!(writes[10].0, "PLANE_BUF_CFG_MULTI_2");
        assert_eq!(writes[10].1, plan.planes[1].ddb.register_value());

        let primary = plane_registers(Pipe::A, 0).unwrap();
        let sprite = plane_registers(Pipe::A, 1).unwrap();
        let pipe_b_primary = plane_registers(Pipe::B, 0).unwrap();
        assert_eq!(primary.ddb.offset(), 0x7027c);
        assert_eq!(primary.level(0).offset(), 0x70240);
        assert_eq!(primary.trans.offset(), 0x70268);
        assert_eq!(sprite.ddb.offset(), 0x7037c);
        assert_eq!(sprite.level(0).offset(), 0x70340);
        assert_eq!(pipe_b_primary.ddb.offset(), 0x7127c);
    }

    #[test]
    fn multi_plane_update_arms_only_after_each_plane_shadow_and_watermarks() {
        let mode = mode_1080p();
        let scanouts = [
            half_screen(0, 0, 0x0100_0000),
            half_screen(1, 960, 0x0200_0000),
        ];
        let regs = crate::drm::intel::regs::mock::MockRegisters::new();

        let plan = update_multi_plane_scanout(&regs, Pipe::A, &mode, &scanouts, config())
            .expect("validated two-plane update should complete its checked register writes");

        let writes = regs.writes();
        let second_arm = writes
            .iter()
            .position(|(name, _)| *name == "PLANE_CTL_MULTI")
            .expect("first control arm is present");
        assert_eq!(second_arm, 20);
        assert!(
            writes[..second_arm]
                .iter()
                .all(|(name, _)| *name != "PLANE_SURF_MULTI" && *name != "PLANE_CTL_MULTI")
        );
        assert_eq!(writes[second_arm + 1].0, "PLANE_SURF_MULTI");
        assert_eq!(writes[second_arm + 2].0, "PLANE_CTL_MULTI");
        assert_eq!(writes[second_arm + 3].0, "PLANE_SURF_MULTI");
        assert_eq!(plan.planes.len(), 2);
        assert_eq!(
            plane_scanout_registers(Pipe::B, 1).unwrap().ctl.offset(),
            0x71280
        );
    }

    #[test]
    fn multi_plane_disable_clears_only_enable_after_readback() {
        let regs = crate::drm::intel::regs::mock::MockRegisters::new();
        let ctl = plane_scanout_registers(Pipe::A, 2).unwrap().ctl;
        regs.set(ctl, 0x8123_4567);

        disable_multi_plane(&regs, Pipe::A, 2).expect("bounded plane disable write");

        assert_eq!(regs.writes(), vec![("PLANE_CTL_MULTI", 0x0123_4567)]);
    }

    #[test]
    fn rejects_overlapping_ddb_before_any_mmio_write() {
        let mode = mode_1080p();
        let mut plan = plan_multi_plane_dbuf(
            &mode,
            &[
                half_screen(0, 0, 0x0100_0000),
                half_screen(1, 960, 0x0200_0000),
            ],
            config(),
        )
        .expect("two packed RGB planes should have a DDB plan");
        plan.planes[1].ddb = plan.planes[0].ddb;
        let regs = crate::drm::intel::regs::mock::MockRegisters::new();

        assert!(matches!(
            program_multi_plane_dbuf(&regs, Pipe::A, &plan),
            Err(PipeError::DdbAllocationOverlap { .. })
        ));
        assert!(regs.writes().is_empty());
    }

    #[test]
    fn refuses_overlapping_or_out_of_bounds_plane_rectangles() {
        let mode = mode_1080p();
        let overlap = [
            half_screen(0, 0, 0x0100_0000),
            half_screen(1, 900, 0x0200_0000),
        ];
        assert_eq!(
            plan_multi_plane_dbuf(&mode, &overlap, config()),
            Err(PipeError::PlaneRectOverlap {
                first: 0,
                second: 1
            })
        );

        let mut out_of_bounds = half_screen(0, 960, 0x0100_0000);
        out_of_bounds.dst_width = 961;
        assert_eq!(
            plan_multi_plane_dbuf(&mode, &[out_of_bounds], config()),
            Err(PipeError::PlaneRectOutOfBounds { plane_index: 0 })
        );
    }

    #[test]
    fn refuses_duplicate_plane_indices_and_unsupported_formats() {
        let mode = mode_1080p();
        assert_eq!(
            plan_multi_plane_dbuf(
                &mode,
                &[
                    half_screen(0, 0, 0x0100_0000),
                    half_screen(0, 960, 0x0200_0000),
                ],
                config(),
            ),
            Err(PipeError::DuplicatePlaneIndex { plane_index: 0 })
        );

        let mut unsupported = half_screen(0, 0, 0x0100_0000);
        unsupported.pixel_format = 0x3231564e; // NV12 is not admitted.
        assert_eq!(
            plan_multi_plane_dbuf(&mode, &[unsupported], config()),
            Err(PipeError::UnsupportedFormat {
                pixel_format: unsupported.pixel_format
            })
        );
    }

    #[test]
    fn refuses_short_backing_allocation_and_scaling() {
        let mode = mode_1080p();
        let mut undersized = half_screen(0, 0, 0x0100_0000);
        undersized.allocation_bytes -= 1;
        assert!(matches!(
            plan_multi_plane_dbuf(&mode, &[undersized], config()),
            Err(PipeError::FramebufferAllocationTooSmall { .. })
        ));

        let mut scaled = half_screen(0, 0, 0x0100_0000);
        scaled.dst_width = 959;
        assert_eq!(
            plan_multi_plane_dbuf(&mode, &[scaled], config()),
            Err(PipeError::PlaneScalingUnsupported { plane_index: 0 })
        );
    }
}

mod checks;
pub(crate) use checks::*;

#[cfg(test)]
mod tests;
