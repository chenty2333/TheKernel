//! Reference section 11 phases 3.4 and 4: the pipe's timings, its share of the
//! display buffer, its watermarks, and its primary plane.
//!
//! [`compute`] turns a [`Mode`] plus a [`PlaneSurface`] into a [`PipeProgram`],
//! which is every register value the pipe half of a modeset needs; [`program`]
//! writes that program in one order; [`prove`] performs the pipe-side half of
//! phase 6 -- the three reads that say whether any of it worked.
//!
//! ```text
//! —    PIPE_MISC(A)  BPC 8, dither off, pixel rounding truncated
//! —    PIPE_ARB_CTL(A)  USE_PROG_SLOTS                     Wa_22012358565:adl-p
//! 3.4  the timing registers                                    from timing.rs
//! 4.1  PLANE_BUF_CFG(A,1) = 0x0fff0000                         the whole DDB
//! 4.2  PLANE_WM(A,1,0..5)  level 0 generous, 1..5 disabled     section 7.3
//!      PLANE_WM_TRANS disabled, PLANE_WM_SAGV and
//!      PLANE_WM_SAGV_TRANS at level 0's value                  [I915]
//! 4.3  PLANE_STRIDE, PLANE_POS, PLANE_SIZE,
//!      PLANE_OFFSET, PLANE_COLOR_CTL, PLANE_CTL, PLANE_SURF     section 5.6
//! 6.1  PIPEDSL(A) four times, a millisecond apart, must change
//! 6.2  PLANE_SURFLIVE(A,1) must equal what PLANE_SURF was written with
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
//! That matters more than it sounds, because section 7.1 says the failure it
//! prevents is invisible: "The default settings of the watermark configuration
//! registers **will not allow the display engine to operate**", and section 11
//! step 4.2 adds that a plane whose `PLANE_WM_EN` is 0 "reads nothing".  A pipe
//! with correct timings, a correct DDI and a correct surface address shows a
//! black screen if the watermarks are missing, which is why [`program`] builds
//! the entire write list -- including the values `PIPE_MISC` and `PIPE_ARB_CTL`
//! will end up with, which need a read each first -- *before* it writes any of
//! it, and why the tests assert on the write log rather than on the return
//! value.
//!
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
//! **The generous level-0 watermark cannot name the whole allocation.**
//! Section 11 step 4.2 and section 7.3 both say level 0's `BLOCKS` should be
//! the plane's whole DDB allocation, and section 7.2 makes that allocation 4096
//! blocks -- but `PLANE_WM_BLOCKS` is `[11:0]` (section 7.3;
//! `skl_universal_plane_regs.h:325`), whose largest value is 4095.  This module
//! writes 4095, which is also the largest value `[I915]` itself considers
//! valid: its watermark computation turns "value >= plane ddb allocation" into
//! a rejected level with the comment "Bspec says: value >= plane ddb allocation
//! -> invalid, hence the +1 here" (`display/skl_watermark.c:1992-1993`).  So
//! 4095 is one block short of the instruction and the largest legal value at
//! the same time.  See `docs/design/intel-pipe.md`.
//!
//! One further source disagreement is recorded in the design document rather
//! than resolved here: `PLANE_WM_LINES`' maximum (31 in section 7.3, 255 in
//! `[I915]`'s `skl_wm_max_lines` for display version 13, which ADL-N is).  The
//! generous level's 31 is legal under both readings, so nothing here turns on
//! it.
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
//! * **The full watermark algorithm.**  Section 7.4 needs memory latency from
//!   the PCode mailbox, `WM_LINETIME`, and a per-level calculation this module
//!   does not implement.  Level 0 is section 7.3's generous version; levels 1-5
//!   are written *disabled* rather than left alone, because section 7.3's third
//!   option is "never: leave the watermark registers at their reset values".
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
    regs::{Register, Registers, ddi, pipe as pipe_regs},
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

/// The largest `PLANE_WM_LINES` section 7.3 allows: "a hardware limit the PRM
/// states explicitly".
///
/// `[I915]`'s `skl_wm_max_lines` returns 31 only below display version 13 and
/// 255 from 13 on (`display/skl_watermark.c:1858-1864`), and ADL-N reports
/// display version 13 (`display/intel_display_device.c:1051`).  The generous
/// level is written with 31, which is legal under either reading; the
/// disagreement is recorded in `docs/design/intel-pipe.md`.
pub(crate) const PLANE_WM_LINES_MAX: u32 = 31;

/// `PLANE_WM_BLOCKS`'s largest value, `[11:0]`.  Reference section 7.3.
pub(crate) const PLANE_WM_BLOCKS_MAX: u32 = 0xfff;

/// `PLANE_WM_LINES`'s field width, `[26:14]`: thirteen bits, so the largest
/// value the register can hold is `0x1fff`.
///
/// Section 7.3 says the field is 13 bits wide and that the hardware honours
/// only 31 of them, which is the gap [`PLANE_WM_LINES_MAX`] exists for.  This
/// constant is the field's width, not the hardware's limit, and it is what
/// [`WatermarkLevel::generous`]'s compile-time check holds the limit against.
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
/// The fields are private and [`Self::WHOLE_BUFFER`] is the only constructor,
/// because that is the only allocation this bring-up makes: section 7.2's
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
/// `ignore_lines` is part of the field layout and is always false here:
/// section 7.3's generous level does not use it, and nothing else in this
/// bring-up programs a watermark level.
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

    /// Section 7.3's generous level: `PLANE_WM_EN` set, `blocks` blocks and
    /// [`PLANE_WM_LINES_MAX`] scanlines.
    ///
    /// `lines` is not a parameter, and that is the fix rather than an
    /// oversight.  The first version of this file had an `enabled(blocks,
    /// lines)` returning `Option`, with an `unwrap_or_else` fallback to
    /// [`Self::disabled`] in the one caller: the `None` arm could not be
    /// reached while [`PLANE_WM_LINES_MAX`] is what it is, and had it ever
    /// been reached it would have written `PLANE_WM_EN = 0` over level 0 --
    /// section 7.1's plane that reads nothing, silently, on the machine whose
    /// only console is that plane.  Taking the line count from the constant
    /// makes a level above the hardware maximum *unrepresentable* rather than
    /// refused, and removes the fallback with it.
    ///
    /// The block count is masked to `PLANE_WM_BLOCKS[11:0]`, so a count that
    /// does not fit the field cannot bleed into the line field.
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

/// `PLANE_WM_LINES_MAX` fits the field it is written into.
///
/// A compile-time check, not a runtime one: [`PLANE_WM_LINES_MAX`] is 31,
/// which section 7.3's hardware limit makes true by hand, and
/// `PLANE_WM_LINES_MASK_MAX` is the thirteen-bit field's largest value.  If
/// either constant is ever edited apart, this is a build failure rather than a
/// line count the hardware silently truncates.
const _: () = assert!(PLANE_WM_LINES_MAX <= PLANE_WM_LINES_MASK_MAX);

/// Every watermark value one plane has, which is more than its levels.
///
/// This bring-up programs section 7.3's first option for the levels: "**Simplest
/// that can work:** program level 0 only, with generous values (`BLOCKS` = the
/// plane's whole DDB allocation, `LINES` = 31), `EN = 1`; disable levels 1..n."
/// Every level is written, including the disabled ones, because section 7.3's
/// third option is "Never: leave the watermark registers at their reset
/// values" -- and section 7.1 says those reset values are the ones that do not
/// work.
///
/// Three more registers belong to the plane's watermarks and are not levels:
/// `PLANE_WM_TRANS`, and the SAGV pair `PLANE_WM_SAGV` and
/// `PLANE_WM_SAGV_TRANS`.  The first version of this module wrote the eight
/// registers the reference's `0x70240 + level*4` formula names and no others,
/// which left the SAGV pair at their reset values and, for the two offsets the
/// formula only *looks* like levels at, wrote a disabled watermark over them.
///
/// * **The SAGV pair gets level 0's value**, not zero.  This kernel never
///   writes `SAGV`'s control, so it cannot know whether the hardware is using
///   these registers; `[I915]` programs them on this platform because
///   `HAS_HW_SAGV_WM` is true there (`display/skl_universal_plane.c:743-748`,
///   `display/skl_watermark.c:3378-3383`).  A watermark with `PLANE_WM_EN`
///   clear is a plane that reads nothing, so the only safe value for a register
///   the pipe may be reading is the generous one level 0 already uses: a plane
///   whose SAGV watermark is *too generous* underruns later and says so in
///   `PIPESTAT`, and one whose SAGV watermark is disabled shows nothing and
///   says nothing.
/// * **`PLANE_WM_TRANS` is written disabled**, and that is `[I915]`'s value
///   here rather than a fallback.  `[I915]` computes the transition watermark
///   from level 0 as `wm0.blocks - 1 + trans_offset + 1`, with `trans_offset`
///   14 on this generation, and then `skl_check_wm_level` clears it when its
///   `min_ddb_alloc` is larger than the plane's DDB allocation
///   (`display/skl_watermark.c:2041-2100`, `:1437-1441`).  With level 0 at the
///   largest legal 4095 blocks inside a 4096-block allocation, the value is
///   4109 blocks -- past the allocation and past `PLANE_WM_BLOCKS`' twelve-bit
///   field -- so the register `[I915]` would write here is zero.  It is a
///   transition watermark, not a level: `EN = 0` on it is what
///   `skl_check_wm_level` itself writes, and what `[I915]` leaves on a machine
///   with IPC disabled (`:3238-3241`), not the "reads nothing" state section
///   7.1 describes.
///
/// **What the generous version costs**, which section 7.3 also says: it
/// "over-allocates and may under-run on a busy memory system".  It does not
/// implement the memory-latency calculation of section 7.4, so the level-0
/// number is not a bound on anything -- it is the largest legal value, which
/// makes an underrun less likely and hides a marginal one behind a bigger
/// margin than the hardware needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatermarkProgram {
    levels: [WatermarkLevel; PLANE_WM_LEVELS],
    transition: WatermarkLevel,
    sagv: WatermarkLevel,
    sagv_transition: WatermarkLevel,
}

impl WatermarkProgram {
    /// Section 7.3's generous level 0 over a DDB allocation, every other level
    /// disabled, and the three non-level watermark registers as described on
    /// the type.
    ///
    /// The block count is the allocation, capped at [`PLANE_WM_BLOCKS_MAX`] --
    /// see the module documentation for why the cap is not optional and why
    /// 4095 is the value `[I915]` would also consider the largest valid one.
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

    /// Level 0's register value, for a log line or an error message.
    pub(crate) const fn level_zero_value(&self) -> u32 {
        self.levels[0].register_value()
    }

    /// `PLANE_WM_TRANS`'s value: disabled, for the reason on the type.
    pub(crate) const fn transition_value(&self) -> u32 {
        self.transition.register_value()
    }

    /// `PLANE_WM_SAGV`'s value: level 0's, for the reason on the type.
    pub(crate) const fn sagv_value(&self) -> u32 {
        self.sagv.register_value()
    }

    /// `PLANE_WM_SAGV_TRANS`'s value: level 0's, for the reason on the type.
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
    /// `PLANE_CTL`: enable, XRGB8888, linear.
    pub(crate) ctl: u32,
    /// `PLANE_SURF`: the GGTT address masked to `[31:12]`.  The commit.
    pub(crate) surf: u32,
}

impl PlaneProgram {
    /// The `PLANE_SURF` field for a GGTT address, or why it cannot be one.
    fn surface_field(address: u64) -> Result<u32, PipeError> {
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

/// How many writes [`PipeProgram::writes`] performs.
///
/// The terms are that function's groups, in order: the pipe's own two
/// ([`Pipe::misc`] and [`Pipe::arb_ctl`]), the timing table's
/// ([`timing::TIMING_REGISTERS`]), `PLANE_BUF_CFG`, the six watermark levels,
/// `PLANE_WM_TRANS` and the SAGV pair, the plane's three `noarm` registers and
/// its four `arm` ones.  It exists so that the `Vec`'s capacity and the tests'
/// count are the same number as the list rather than a second, hand-kept one:
/// the first version of this file reserved 23 slots for 24 writes.
pub(crate) const PIPE_PROGRAM_WRITES: usize =
    2 + timing::TIMING_REGISTERS + 1 + PLANE_WM_LEVELS + 3 + 3 + 4;

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

    /// Every write this program performs, in the order it performs them, given
    /// `PIPE_MISC`'s current contents.
    ///
    /// The order is section 11's phases -- 3.4 timings, 4.1 DDB, 4.2 watermarks,
    /// 4.3 plane -- with `PIPE_MISC` first and `PLANE_SURF` last.  Section 5.6
    /// groups the plane registers into "noarm" and "arm" rather than ordering
    /// them absolutely, and both groups take effect at the same instant, so
    /// section 11's phase order is the one used; what section 5.6 *does* order
    /// absolutely is that `PLANE_CTL` is written immediately before
    /// `PLANE_SURF` and that `PLANE_SURF` is the last of them, which this list
    /// keeps.
    ///
    /// `PIPE_MISC` is first rather than last for a reason that is worth stating,
    /// because section 11 puts it in step 5.6, after the plane: it is not double
    /// buffered, it arms nothing, and the property section 5.6 cares about is
    /// that nothing in the plane's double-buffered state is committed before the
    /// watermarks are -- which `PLANE_SURF` being last is what guarantees.
    /// Keeping it here means one call programs everything the pipe owns.
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

        // Phase 4.3, the `arm` half: everything from here takes effect when
        // `PLANE_SURF` lands.  The key and auxiliary-plane registers section 5.6
        // lists between these two never existed in the table -- no section gives
        // them an offset -- so the run is `PLANE_OFFSET`, `PLANE_COLOR_CTL`,
        // `PLANE_CTL`, `PLANE_SURF`.
        writes.push(PlannedWrite {
            register: self.pipe.plane_offset(),
            value: self.plane.offset,
        });
        writes.push(PlannedWrite {
            register: self.pipe.plane_color_ctl(),
            value: self.plane.color_ctl,
        });
        writes.push(PlannedWrite {
            register: self.pipe.plane_ctl(),
            value: self.plane.ctl,
        });
        // Section 5.6: "PLANE_SURF <- GGTT address <-- THIS ARMS EVERYTHING".
        writes.push(PlannedWrite {
            register: self.pipe.plane_surf(),
            value: self.plane.surf,
        });

        writes
    }

    /// The whole program as log lines, one per write.
    ///
    /// Built as a string rather than logged as it goes because the sequence
    /// cannot be run on the target yet, so the text has to be something a host
    /// test can assert on.  [`Self::log`] puts the same text in the kernel log.
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
        out.push_str(&format!(
            "intel-pipe: pipe {} watermark level 0: {:#010x} (blocks {}, lines {}), levels 1..{} \
             disabled\n",
            self.pipe,
            self.watermark.level_zero_value(),
            self.watermark.levels()[0].blocks(),
            self.watermark.levels()[0].lines(),
            PLANE_WM_LEVELS - 1,
        ));
        out.push_str(&format!(
            "intel-pipe: pipe {} watermarks SAGV {:#010x} (level 0's value), SAGV_TRANS {:#010x}, \
             TRANS {:#010x} (disabled: no transition watermark fits a whole-DBUF level 0)\n",
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
        out
    }

    /// Put [`Self::render`] into the kernel log.
    pub(crate) fn log(&self, pipe_misc_before: u32, arb_ctl_before: u32) {
        for line in self.render(pipe_misc_before, arb_ctl_before).lines() {
            axlog::info!("{line}");
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
pub(crate) fn compute(
    pipe: Pipe,
    mode: &Mode,
    surface: PlaneSurface,
) -> Result<PipeProgram, PipeError> {
    let timings = timing::timing_registers(mode)?;
    let ddb = DdbAllocation::WHOLE_BUFFER;
    let watermark = WatermarkProgram::generous(ddb);
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
        ctl: PLANE_CTL_LINEAR_XRGB8888,
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
    /// A register the sequence cannot do without was outside the mapped window.
    Unreadable { register: &'static str },
    /// A write did not happen: the register is not declared writable, or it is
    /// outside the mapped window.
    WriteRefused { register: &'static str },
    /// `timing.rs` refused the mode.  It refuses an interlaced timing and a
    /// malformed one; both are deliberate and neither is worked around here.
    Timing(timing::TimingError),
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

/// What phase 4 wrote, in the order it wrote it.
///
/// Kept rather than discarded so that the boot log can show what actually
/// happened next to what was planned, and so that a caller can hand the same
/// program to [`prove`].
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
    /// Every write that happened, in order.
    pub(crate) writes: Vec<PlannedWrite>,
}

impl PipeState {
    /// The same text the log carries, for `/sys/kernel/debug/dri/0/intel_gpu`.
    pub(crate) fn render(&self) -> String {
        let mut out = format!(
            "intel-pipe: pipe {} programmed with {} writes (reference section 11 phases 3.4 and \
             4)\n",
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
            axlog::info!("{line}");
        }
    }
}

/// Reference section 11 phase 4: write the whole program, in order.
///
/// The write list is built before the first write, so a failure part-way
/// through cannot leave a value that was computed against a register that no
/// longer reads the same way, and so a caller can log exactly what will happen
/// before it happens with [`PipeProgram::render`].
///
/// A refused write stops the sequence and is reported by name.  Stopping is the
/// point: the values after the failure are the ones that arm the plane, and a
/// plane armed with a half-written configuration is the black screen section
/// 7.1 spends its words on.
///
/// Nothing here enables the pipe.  `TRANSCONF` is section 11 step 5.6 and
/// belongs to the output workstream; until it is written, this program has
/// configured a pipe that is not scanning.
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

// ---------------------------------------------------------------------------
// Phase 6: the pipe-side read-backs
// ---------------------------------------------------------------------------

/// Section 11 phase 6.1's verdict on `PIPEDSL`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScanlineCheck {
    /// At least two of the samples differed, so the pipe is counting lines.
    ///
    /// The samples carry their timestamps as well as their line counts, so the
    /// verdict can report the line rate it observed and not only the fact that
    /// the counter moved.  Section 12.3 calls `PIPEDSL` sampled over time "the
    /// only way to verify your PLL arithmetic against reality without a
    /// scope"; a verdict that discarded the timestamps threw that measurement
    /// away.
    Scanning {
        samples: [ScanlineSample; SCANLINE_SAMPLES],
    },
    /// Every sample was identical: the pipe is not scanning.
    NotScanning {
        samples: [ScanlineSample; SCANLINE_SAMPLES],
    },
    /// `PIPEDSL` could not be read at all.
    Unreadable { register: &'static str },
}

/// One `PIPEDSL` reading: the line the pipe was on, and when that was read.
///
/// Both fields are raw: `line` is `PIPEDSL`'s `LINE[19:0]` and `micros` is the
/// timer's reading, so the difference between two samples is the only thing
/// with a meaning and [`ScanlineCheck`] is where the arithmetic over them
/// lives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScanlineSample {
    /// `PIPEDSL`'s `LINE[19:0]`.
    pub(crate) line: u32,
    /// The timer's reading when the register was read, in microseconds.
    pub(crate) micros: u64,
}

impl ScanlineSample {
    /// The lines between this sample and `next`, signed: negative means the
    /// counter went backwards, which is what a frame wrap looks like from
    /// inside one sampling window.
    pub(crate) const fn line_delta(self, next: Self) -> i64 {
        (next.line as i64) - (self.line as i64)
    }

    /// How long after this sample `next` was taken, in microseconds.
    ///
    /// Saturating rather than wrapping: a timer that went backwards is a
    /// broken timer, and the interval it produces is one no rate should be
    /// computed from.
    pub(crate) const fn micros_delta(self, next: Self) -> u64 {
        next.micros.saturating_sub(self.micros)
    }
}

impl ScanlineCheck {
    pub(crate) const fn is_ok(self) -> bool {
        matches!(self, Self::Scanning { .. })
    }

    /// Every interval between consecutive samples, as `(line delta, micros)`.
    ///
    /// Three entries for four samples, in order.  This is what the log renders,
    /// and it is here rather than in the log because it is also the raw
    /// material of [`Self::observed_lines_per_second`].
    pub(crate) fn intervals(self) -> [(i64, u64); SCANLINE_SAMPLES - 1] {
        let mut out = [(0i64, 0u64); SCANLINE_SAMPLES - 1];
        match self {
            Self::Scanning { samples } | Self::NotScanning { samples } => {
                for (index, pair) in samples.windows(2).enumerate() {
                    out[index] = (pair[0].line_delta(pair[1]), pair[0].micros_delta(pair[1]));
                }
            }
            Self::Unreadable { .. } => {}
        }
        out
    }

    /// The line rate the samples measured, in lines per second, or `None` when
    /// they cannot give one.
    ///
    /// A rate over the intervals that moved forward and took time: an interval
    /// whose counter did not move is not a measurement of zero lines per
    /// second, it is not a measurement, and one that went backwards is a frame
    /// wrap or a counter that is not counting.  `None` therefore means "no
    /// measurement", never "zero", and it is what a timer that does not advance
    /// produces.
    ///
    /// This is section 12.3's observation: the delta over a known interval
    /// gives the line rate, and the line rate is the pixel clock divided by the
    /// line total -- 67.5 klines/s at 148.5 MHz over 2200 pixels.  The number
    /// reported here is measured, not derived; comparing it with the mode is
    /// the reader's step, and the boot log prints both.
    pub(crate) fn observed_lines_per_second(self) -> Option<u32> {
        let mut lines = 0u64;
        let mut micros = 0u64;
        for (delta, span) in self.intervals() {
            if delta > 0 && span > 0 {
                lines += delta as u64;
                micros += span;
            }
        }
        if micros == 0 || lines == 0 {
            return None;
        }
        u32::try_from(lines * 1_000_000 / micros).ok()
    }

    /// What to do about it, in the reference's own terms.
    pub(crate) fn describe(self) -> String {
        match self {
            Self::Scanning { samples } => format!(
                "PIPEDSL sampled {} times over {} ms and changed: {}.  Intervals: {}.  Observed \
                 line rate: {}.  The pipe is scanning (reference section 11 phase 6.1, and \
                 section 12.3 for the rate).",
                SCANLINE_SAMPLES,
                (SCANLINE_SAMPLES as u64 - 1) * SCANLINE_INTERVAL_MICROS / 1_000,
                render_samples(&samples),
                render_intervals(self.intervals()),
                render_rate(self.observed_lines_per_second()),
            ),
            Self::NotScanning { samples } => format!(
                "PIPEDSL read {} on every one of {SCANLINE_SAMPLES} samples over {} ms: the value \
                 never changed, so the pipe is not scanning and nothing downstream of it matters \
                 (reference section 11 phase 6.1).  Intervals: {}.  Observed line rate: {}.  \
                 Check, in this order: TRANSCONF is written with ENABLE | STATE_ENABLE (section \
                 11 step 5.6); TRANS_CLK_SEL names the port PLL the DDI is using (step 5.4); the \
                 PLL locked (step 5.1)",
                render_samples(&samples),
                (SCANLINE_SAMPLES as u64 - 1) * SCANLINE_INTERVAL_MICROS / 1_000,
                render_intervals(self.intervals()),
                render_rate(self.observed_lines_per_second()),
            ),
            Self::Unreadable { register } => format!(
                "{register} could not be read, so whether the pipe is scanning is unknown.  It is \
                 outside the mapped register window, which is a fault in this kernel's mapping \
                 rather than in the mode"
            ),
        }
    }
}

/// The samples as `line@micros`, in order.
fn render_samples(samples: &[ScanlineSample; SCANLINE_SAMPLES]) -> String {
    let mut out = String::new();
    for (index, sample) in samples.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("{}@{}us", sample.line, sample.micros));
    }
    out
}

/// The intervals as `+lines/micros`, in order.
fn render_intervals(intervals: [(i64, u64); SCANLINE_SAMPLES - 1]) -> String {
    let mut out = String::new();
    for (index, (lines, micros)) in intervals.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("{lines:+}/{micros}us"));
    }
    out
}

/// The measured rate as a log phrase, or why there is none.
fn render_rate(rate: Option<u32>) -> String {
    match rate {
        Some(rate) => format!("{rate} lines/s"),
        None => String::from(
            "none: no interval both moved the counter forward and took time, so nothing was \
             measured",
        ),
    }
}

/// Section 11 phase 6.2's verdict on `PLANE_SURFLIVE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceCheck {
    /// The live address is the one that was written.
    Armed { live: u32, wrote: u32 },
    /// The live address is not the one that was written; zero means the plane
    /// never armed at all.
    NotArmed { live: u32, wrote: u32 },
    /// `PLANE_SURFLIVE` could not be read at all.
    Unreadable { register: &'static str },
}

impl SurfaceCheck {
    pub(crate) const fn is_ok(self) -> bool {
        matches!(self, Self::Armed { .. })
    }

    pub(crate) fn describe(self) -> String {
        match self {
            Self::Armed { live, wrote } => format!(
                "PLANE_SURFLIVE {live:#010x} equals the address written to PLANE_SURF \
                 {wrote:#010x}: the plane is armed (reference section 11 phase 6.2)"
            ),
            Self::NotArmed { live, wrote } => format!(
                "PLANE_SURFLIVE reads {live:#010x} but PLANE_SURF was written with {wrote:#010x}, \
                 so the plane is not scanning the surface this kernel gave it (reference section \
                 11 phase 6.2).  {} ",
                if live == 0 {
                    "A zero means the plane never armed: re-read PLANE_CTL, and if ENABLE is set \
                     then the surface address was rejected -- section 11 step 4.3 names alignment \
                     and an invalid GGTT entry as the two causes"
                } else {
                    "A different non-zero address means the plane is scanning something else -- \
                     most likely the firmware's surface, so the write did not take"
                }
            ),
            Self::Unreadable { register } => {
                format!("{register} could not be read, so whether the plane armed is unknown")
            }
        }
    }
}

/// Section 11 phase 6.4's verdict on the FIFO underrun bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnderrunCheck {
    /// `PIPESTAT` bit 31 is clear.
    Clear { stat: u32 },
    /// `PIPESTAT` bit 31 is set: the watermarks or the DDB are wrong.
    Underrun {
        stat: u32,
        /// The level-0 `PLANE_WM` value this program wrote, so the reader can
        /// compare it with the register without another lookup.
        watermark: u32,
        /// The `PLANE_BUF_CFG` value this program wrote.
        ddb: u32,
    },
    /// `PIPESTAT` could not be read at all.
    Unreadable { register: &'static str },
}

impl UnderrunCheck {
    pub(crate) const fn is_ok(self) -> bool {
        matches!(self, Self::Clear { .. })
    }

    pub(crate) fn describe(self) -> String {
        match self {
            Self::Clear { stat } => format!(
                "PIPESTAT {stat:#010x} has bit 31 clear: no FIFO underrun (reference section 11 \
                 phase 6.4)"
            ),
            Self::Underrun {
                stat,
                watermark,
                ddb,
            } => format!(
                "PIPESTAT {stat:#010x} has bit 31 set: the pipe's FIFO underran.  Reference \
                 section 11 step 6.4 says to go back to phase 4.2 before changing anything else, \
                 because the watermarks or the DDB are wrong: this program wrote PLANE_WM(0) = \
                 {watermark:#010x} and PLANE_BUF_CFG = {ddb:#010x}.  Reference section 7.1: with \
                 PLANE_WM_EN clear the plane reads nothing, and with the reset values the display \
                 engine does not operate at all.  The generous level 0 this bring-up uses is \
                 section 7.3's, not the section 7.4 latency calculation, so an underrun here is \
                 the expected cost of that choice rather than a surprising one"
            ),
            Self::Unreadable { register } => format!(
                "{register} could not be read, so whether the pipe underran is unknown.  \
                 Reference section 10.8: an underrun that is never seen is an underrun that gets \
                 misattributed to the timing or the monitor"
            ),
        }
    }
}

/// The three pipe-side reads of reference section 11 phase 6.
///
/// Every check runs, and every check reports, even when an earlier one failed:
/// on a machine whose only console is the screen, three findings are worth more
/// than one, and a caller that stops at the first failure learns less about
/// which half of the sequence is wrong.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PipeChecks {
    pub(crate) pipe: Pipe,
    pub(crate) scanline: ScanlineCheck,
    pub(crate) surface: SurfaceCheck,
    pub(crate) underrun: UnderrunCheck,
}

impl PipeChecks {
    /// Whether all three checks passed.
    pub(crate) const fn ok(&self) -> bool {
        self.scanline.is_ok() && self.surface.is_ok() && self.underrun.is_ok()
    }

    /// The three checks as log lines.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        for (phase, text) in [
            ("6.1", self.scanline.describe()),
            ("6.2", self.surface.describe()),
            ("6.4", self.underrun.describe()),
        ] {
            out.push_str(&format!(
                "intel-pipe: pipe {} phase {phase}: {text}\n",
                self.pipe
            ));
        }
        out
    }

    /// Put [`Self::render`] into the kernel log.
    pub(crate) fn log(&self) {
        for line in self.render().lines() {
            axlog::info!("{line}");
        }
    }
}

/// Reference section 11 phase 6.1, 6.2 and 6.4 against one pipe.
///
/// The clock is a parameter for the same reason `sink::probe_one`'s is: the
/// scanline check has to let a few milliseconds pass, and a host test cannot
/// wait for real ones, so the test supply their own [`PollTimer`] and the boot
/// path passes [`MonotonicTimer`].
///
/// This is the shape the top-level "prove it" step calls.  It reads registers
/// and writes nothing: it is safe to run at any point, including against a pipe
/// someone else programmed.
pub(crate) fn prove(
    regs: &impl Registers,
    plan: &PipeProgram,
    timer: &impl PollTimer,
) -> PipeChecks {
    let pipe = plan.pipe;

    let scanline = match sample_scanline(regs, pipe.pipedsl(), timer) {
        Ok(samples) => {
            // The verdict is on the *lines*.  Two samples with the same line
            // and different timestamps are a stopped counter, not a slow one:
            // the timestamps are the measurement, never the thing measured.
            if samples.windows(2).any(|pair| pair[0].line != pair[1].line) {
                ScanlineCheck::Scanning { samples }
            } else {
                ScanlineCheck::NotScanning { samples }
            }
        }
        Err(register) => ScanlineCheck::Unreadable { register },
    };

    // The comparison is on the address field only.  `PLANE_SURF` also carries
    // bit 2 as a decrypt flag (§5.4), which this bring-up writes as zero; a mask
    // that ignored it would call a decrypt-enabled read-back a mismatch, and one
    // that included it would call a decrypt flag a wrong address.
    let wrote = plan.plane.surf;
    let surface = match regs.read(pipe.plane_surflive()) {
        Some(live) if live & PLANE_SURF_ADDRESS_MASK == wrote & PLANE_SURF_ADDRESS_MASK => {
            SurfaceCheck::Armed { live, wrote }
        }
        Some(live) => SurfaceCheck::NotArmed { live, wrote },
        None => SurfaceCheck::Unreadable {
            register: pipe.plane_surflive().name(),
        },
    };

    let underrun = match regs.read(pipe.pipestat()) {
        Some(stat) if stat & PIPE_FIFO_UNDERRUN_STATUS != 0 => UnderrunCheck::Underrun {
            stat,
            watermark: plan.watermark.level_zero_value(),
            ddb: plan.ddb.register_value(),
        },
        Some(stat) => UnderrunCheck::Clear { stat },
        None => UnderrunCheck::Unreadable {
            register: pipe.pipestat().name(),
        },
    };

    PipeChecks {
        pipe,
        scanline,
        surface,
        underrun,
    }
}

/// Reference section 11 phase 6.1, 6.2 and 6.4 against one pipe, on the
/// machine's own clock.
///
/// This is the form the boot path calls; [`prove`] is the same three checks
/// with the clock supplied, which is what a host test calls so that the
/// scanline interval costs no wall time.
pub(crate) fn prove_at_boot(regs: &impl Registers, plan: &PipeProgram) -> PipeChecks {
    prove(regs, plan, &MonotonicTimer)
}

/// Read `PIPEDSL` [`SCANLINE_SAMPLES`] times, [`SCANLINE_INTERVAL_MICROS`]
/// apart.
///
/// Only `LINE[19:0]` is kept: the register's other bits are reserved, and a
/// changing reserved bit is not a scanning pipe.  Each sample keeps the timer's
/// reading as well, taken immediately before the register read, because the
/// line rate section 12.3 asks for is the delta between two of these and a
/// timestamp thrown away is an observation thrown away.
fn sample_scanline(
    regs: &impl Registers,
    register: Register,
    timer: &impl PollTimer,
) -> Result<[ScanlineSample; SCANLINE_SAMPLES], &'static str> {
    let mut samples = [ScanlineSample { line: 0, micros: 0 }; SCANLINE_SAMPLES];
    for (index, sample) in samples.iter_mut().enumerate() {
        if index > 0 {
            wait_micros(timer, SCANLINE_INTERVAL_MICROS);
        }
        // Stamped before the read, so the stamp is never later than the value
        // it belongs to; the read itself is microseconds of MMIO and the
        // interval is a millisecond.
        let micros = timer.now_micros();
        let Some(value) = regs.read(register) else {
            return Err(register.name());
        };
        *sample = ScanlineSample {
            line: value & PIPEDSL_LINE_MASK,
            micros,
        };
    }
    Ok(samples)
}

/// Let `micros` microseconds pass, as far as the timer can tell.
///
/// Bounded twice: by the clock, which is what makes it a wait, and by
/// [`SCANLINE_PAUSE_BUDGET`] pauses, which is what keeps a timer that does not
/// advance its own clock -- the one way a [`PollTimer`] can be wrong -- from
/// hanging the boot.
fn wait_micros(timer: &impl PollTimer, micros: u64) {
    let deadline = timer.now_micros().saturating_add(micros);
    let mut pauses = 0;
    while timer.now_micros() < deadline && pauses < SCANLINE_PAUSE_BUDGET {
        timer.pause();
        pauses += 1;
    }
}

#[cfg(test)]
mod tests;
