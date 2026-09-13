//! One block of the display engine's register table.
//!
//! Owner: [`super`].  Every offset here cites the reference section it came
//! from, and the access classification is the sequence's: a register the
//! bring-up writes is `read_write`, one it only reads back is `read_only`.
//!
//! Three registers section 5.4's tables do not place are declared from
//! `[I915]` instead -- `PLANE_WM_TRANS`, `PLANE_WM_SAGV` and
//! `PLANE_WM_SAGV_TRANS` -- because the reference either names them without an
//! offset or reaches their offsets with a formula that is wrong about what
//! lives there.  The transcription notes at the end of this file record each
//! one with the file and line it came from.

use super::{Meaning, Register};

/// `PIPECONF` (pipe A), reference section 5.2.
///
/// The pipe's own enable: `ENABLE[31]` and `STATE_ENABLE[30]`, with the
/// interlace mode in `[23:21]`.  Section 11 step 5.6 writes
/// `ENABLE | STATE_ENABLE | progressive` here after the plane is programmed.
/// Section 5.2 warns that i915 v6.12 renames this register `TRANSCONF` while
/// the PRM and every older source call it `PIPECONF`: one register, one
/// offset, not two.  Section 8.4 adds that on Gen12 the output bit depth and
/// dithering are **not** taken from here -- they live in `PIPE_MISC`.
pub(crate) const PIPECONF_A: Register =
    Register::read_write("PIPECONF_A", 0x7_0008, Meaning::BringUp, None);

/// `PIPECONF` (pipe B), reference section 5.2.
///
/// Pipe B's instance of the pipe enable and state-enable bits that section 11
/// step 5.6 writes, at the section 5.2 per-pipe stride of `+0x1000` from
/// pipe A.
pub(crate) const PIPECONF_B: Register =
    Register::read_write("PIPECONF_B", 0x7_1008, Meaning::BringUp, None);

/// `PIPECONF` (pipe C), reference section 5.2.
///
/// Pipe C's instance of the pipe enable and state-enable bits that section 11
/// step 5.6 writes, at the section 5.2 per-pipe stride of `+0x2000` from
/// pipe A.
pub(crate) const PIPECONF_C: Register =
    Register::read_write("PIPECONF_C", 0x7_2008, Meaning::BringUp, None);

/// `PIPECONF` (pipe D), reference section 5.2.
///
/// Pipe D's instance of the pipe enable and state-enable bits that section 11
/// step 5.6 writes, at the section 5.2 per-pipe stride of `+0x3000` from
/// pipe A.
pub(crate) const PIPECONF_D: Register =
    Register::read_write("PIPECONF_D", 0x7_3008, Meaning::BringUp, None);

/// `PIPESRC` (pipe A / transcoder A), reference section 5.2.
///
/// The pipe's source size: `WIDTH[31:16]` and `HEIGHT[15:0]`, each storing
/// `value - 1`.  Section 6.1 -- reached from section 11 step 3.4 -- writes
/// `((hdisplay - 1) << 16) | (vdisplay - 1)`.  Section 5.2 lists it among the
/// pipe registers while its offset lies in the transcoder block of section 5.3,
/// and section 6.1 writes it per transcoder `T`; section 5.1 ties transcoders
/// 1:1 to pipes, so the instance is the same either way.
pub(crate) const PIPESRC_A: Register =
    Register::read_write("PIPESRC_A", 0x6_001c, Meaning::BringUp, None);

/// `PIPESRC` (pipe B / transcoder B), reference section 5.2.
///
/// Pipe B's source size, written with pipe B's timing registers; the document
/// states the stride (`+0x1000` per pipe) rather than a second table.
pub(crate) const PIPESRC_B: Register =
    Register::read_write("PIPESRC_B", 0x6_101c, Meaning::BringUp, None);

/// `PIPESRC` (pipe C / transcoder C), reference section 5.2.
///
/// Pipe C's source size, written with pipe C's timing registers; the document
/// states the stride (`+0x2000` per pipe) rather than a second table.
pub(crate) const PIPESRC_C: Register =
    Register::read_write("PIPESRC_C", 0x6_201c, Meaning::BringUp, None);

/// `PIPESRC` (pipe D / transcoder D), reference section 5.2.
///
/// Pipe D's source size, written with pipe D's timing registers; the document
/// states the stride (`+0x3000` per pipe) rather than a second table.
pub(crate) const PIPESRC_D: Register =
    Register::read_write("PIPESRC_D", 0x6_301c, Meaning::BringUp, None);

/// `PIPEDSL` (pipe A), reference section 5.2.
///
/// The live scanline counter, `LINE[19:0]`.  It is the document's single most
/// useful sanity check: section 11 step 6.1 reads it twice a few milliseconds
/// apart and requires the value to change, because if the pipe is not scanning
/// nothing downstream matters.  Read-only here; the bring-up never writes it.
pub(crate) const PIPEDSL_A: Register =
    Register::read_only("PIPEDSL_A", 0x7_0000, Meaning::BringUp, None);

/// `PIPEDSL` (pipe B), reference section 5.2.
///
/// Pipe B's live scanline counter; the same proof that pipe B is scanning that
/// step 6.1 reads for pipe A, at the section 5.2 per-pipe stride of `+0x1000`.
pub(crate) const PIPEDSL_B: Register =
    Register::read_only("PIPEDSL_B", 0x7_1000, Meaning::BringUp, None);

/// `PIPEDSL` (pipe C), reference section 5.2.
///
/// Pipe C's live scanline counter; the same proof that pipe C is scanning that
/// step 6.1 reads for pipe A, at the section 5.2 per-pipe stride of `+0x2000`.
pub(crate) const PIPEDSL_C: Register =
    Register::read_only("PIPEDSL_C", 0x7_2000, Meaning::BringUp, None);

/// `PIPEDSL` (pipe D), reference section 5.2.
///
/// Pipe D's live scanline counter; the same proof that pipe D is scanning that
/// step 6.1 reads for pipe A, at the section 5.2 per-pipe stride of `+0x3000`.
pub(crate) const PIPEDSL_D: Register =
    Register::read_only("PIPEDSL_D", 0x7_3000, Meaning::BringUp, None);

/// `PIPESTAT` (pipe A), reference section 5.2.
///
/// The per-pipe interrupt status and enable halves; bit 31 is
/// `PIPE_FIFO_UNDERRUN_STATUS`, which section 11 step 6.4 reads as the verdict
/// on the DDB and watermark programming of phase 4.  Read-only on purpose: the
/// section 11 sequence only reads it, and step 6.6 leaves interrupts to the
/// hardening pass, so nothing here writes the enable half yet.
pub(crate) const PIPESTAT_A: Register =
    Register::read_only("PIPESTAT_A", 0x7_0024, Meaning::BringUp, None);

/// `PIPESTAT` (pipe B), reference sections 5.2 and 10.4.
///
/// Pipe B's status register, carrying the same FIFO-underrun bit 31 that step
/// 6.4 reads on pipe A; section 10.4 gives the per-pipe form directly as
/// `0x70024 + pipe * 0x1000`.
pub(crate) const PIPESTAT_B: Register =
    Register::read_only("PIPESTAT_B", 0x7_1024, Meaning::BringUp, None);

/// `PIPESTAT` (pipe C), reference sections 5.2 and 10.4.
///
/// Pipe C's status register, carrying the same FIFO-underrun bit 31 that step
/// 6.4 reads on pipe A; section 10.4 gives the per-pipe form directly as
/// `0x70024 + pipe * 0x1000`.
pub(crate) const PIPESTAT_C: Register =
    Register::read_only("PIPESTAT_C", 0x7_2024, Meaning::BringUp, None);

/// `PIPESTAT` (pipe D), reference sections 5.2 and 10.4.
///
/// Pipe D's status register, carrying the same FIFO-underrun bit 31 that step
/// 6.4 reads on pipe A; section 10.4 gives the per-pipe form directly as
/// `0x70024 + pipe * 0x1000`.
pub(crate) const PIPESTAT_D: Register =
    Register::read_only("PIPESTAT_D", 0x7_3024, Meaning::BringUp, None);

/// `PIPE_MISC` (pipe A), reference section 5.2.
///
/// Where the pipe's output bit depth and dithering actually live on Gen12:
/// `BPC[7:5]` (8 bpc = 0, and 12 bpc is ADL-P+ only), `DITHER_ENABLE[4]`,
/// `DITHER_TYPE[3:2]`, alongside `HDR_MODE_PRECISION[23]` and
/// `PIXEL_ROUNDING_TRUNC[8]`.  Section 8.4 corrects an earlier draft that put
/// the bit depth in `TRANSCONF`, and section 11 step 5.6 writes this register
/// for 8 bpc with dithering off; it is written in the same step as the pipe
/// enable, after the plane.
pub(crate) const PIPE_MISC_A: Register =
    Register::read_write("PIPE_MISC_A", 0x7_0030, Meaning::BringUp, None);

/// `PIPE_MISC` (pipe B), reference section 5.2.
///
/// Pipe B's output bit depth, dithering and rounding control; same fields as
/// pipe A, at the section 5.2 per-pipe stride of `+0x1000`.
pub(crate) const PIPE_MISC_B: Register =
    Register::read_write("PIPE_MISC_B", 0x7_1030, Meaning::BringUp, None);

/// `PIPE_MISC` (pipe C), reference section 5.2.
///
/// Pipe C's output bit depth, dithering and rounding control; same fields as
/// pipe A, at the section 5.2 per-pipe stride of `+0x2000`.
pub(crate) const PIPE_MISC_C: Register =
    Register::read_write("PIPE_MISC_C", 0x7_2030, Meaning::BringUp, None);

/// `PIPE_MISC` (pipe D), reference section 5.2.
///
/// Pipe D's output bit depth, dithering and rounding control; same fields as
/// pipe A, at the section 5.2 per-pipe stride of `+0x3000`.
pub(crate) const PIPE_MISC_D: Register =
    Register::read_write("PIPE_MISC_D", 0x7_3030, Meaning::BringUp, None);

/// `PIPE_ARB_CTL` (pipe A), reference section 5.2.
///
/// `USE_PROG_SLOTS[13]` makes the pipe take the plane's arbiter slot count
/// from `PLANE_CTL`'s `ARB_SLOTS[30:28]` rather than from its own default, and
/// the two halves are one workaround: `Wa_22012358565:adl-p`.  `[I915]` writes
/// the pipe half in `intel_enable_transcoder` (`display/intel_display.c:441-445`,
/// next to `:442`'s `DISPLAY_VER(dev_priv) == 13`) and the plane half in
/// `skl_plane_ctl` (`display/skl_universal_plane.c:1090-1092`).  Section 5.2
/// states the offset and the field, which matching `[I915]`'s `0x70028` and
/// `REG_BIT(13)` (`i915_reg.h:1704-1706`) is what makes it transcribable; §5.6
/// and §11 never name it, which is why this table's first draft left it out.
pub(crate) const PIPE_ARB_CTL_A: Register =
    Register::read_write("PIPE_ARB_CTL_A", 0x7_0028, Meaning::BringUp, None);

/// `PIPE_ARB_CTL` (pipe B), reference section 5.2.
///
/// Pipe B's arbiter control, `USE_PROG_SLOTS[13]`; section 5.2 per-pipe stride
/// `+0x1000`.
pub(crate) const PIPE_ARB_CTL_B: Register =
    Register::read_write("PIPE_ARB_CTL_B", 0x7_1028, Meaning::BringUp, None);

/// `PIPE_ARB_CTL` (pipe C), reference section 5.2.
///
/// Pipe C's arbiter control, `USE_PROG_SLOTS[13]`; section 5.2 per-pipe stride
/// `+0x2000`.
pub(crate) const PIPE_ARB_CTL_C: Register =
    Register::read_write("PIPE_ARB_CTL_C", 0x7_2028, Meaning::BringUp, None);

/// `PIPE_ARB_CTL` (pipe D), reference section 5.2.
///
/// Pipe D's arbiter control, `USE_PROG_SLOTS[13]`; section 5.2 per-pipe stride
/// `+0x3000`.
pub(crate) const PIPE_ARB_CTL_D: Register =
    Register::read_write("PIPE_ARB_CTL_D", 0x7_3028, Meaning::BringUp, None);

/// `PLANE_CTL` (plane 1, pipe A), reference section 5.4.
///
/// The plane's format, tiling, alpha and rotation control plus its `ENABLE`
/// bit.  Section 5.5 gives the fields for the linear XRGB8888 scanout of a
/// first bring-up -- `ENABLE[31]`, format `[27:23]` with
/// `PLANE_CTL_FORMAT_XRGB_8888` at `4 << 24`, tiling `[12:10]` zero for
/// linear, alpha `[5:4]` disabled, rotation `[1:0]` zero.  Section 11 step 4.3
/// writes `ENABLE | FORMAT_XRGB8888 | TILED_LINEAR` immediately before
/// `PLANE_SURF`, because the register self-arms a plane that was previously
/// disabled, and step 6.2 re-reads it when `PLANE_SURFLIVE` does not match.
pub(crate) const PLANE_CTL_A: Register =
    Register::read_write("PLANE_CTL_A", 0x7_0180, Meaning::BringUp, None);

/// `PLANE_CTL` (plane 1, pipe B), reference section 5.4.
///
/// Pipe B's primary-plane format, tiling and enable control, with the same
/// fields as pipe A, at the section 5.4 per-pipe stride of `+0x1000`.
pub(crate) const PLANE_CTL_B: Register =
    Register::read_write("PLANE_CTL_B", 0x7_1180, Meaning::BringUp, None);

/// `PLANE_CTL` (plane 1, pipe C), reference section 5.4.
///
/// Pipe C's primary-plane format, tiling and enable control, with the same
/// fields as pipe A, at the section 5.4 per-pipe stride of `+0x2000`.
pub(crate) const PLANE_CTL_C: Register =
    Register::read_write("PLANE_CTL_C", 0x7_2180, Meaning::BringUp, None);

/// `PLANE_CTL` (plane 1, pipe D), reference section 5.4.
///
/// Pipe D's primary-plane format, tiling and enable control, with the same
/// fields as pipe A, at the section 5.4 per-pipe stride of `+0x3000`.
pub(crate) const PLANE_CTL_D: Register =
    Register::read_write("PLANE_CTL_D", 0x7_3180, Meaning::BringUp, None);

/// `PLANE_STRIDE` (plane 1, pipe A), reference section 5.4.
///
/// The surface stride in bytes, `[11:0]`.  It belongs to the `noarm` half of
/// the plane commit (section 5.6) because it is double buffered behind
/// `PLANE_SURF`; section 11 step 3.2 requires the allocation's stride to be a
/// multiple of 64 bytes, with 256 the safest.
pub(crate) const PLANE_STRIDE_A: Register =
    Register::read_write("PLANE_STRIDE_A", 0x7_0188, Meaning::BringUp, None);

/// `PLANE_STRIDE` (plane 1, pipe B), reference section 5.4.
///
/// Pipe B's primary-plane surface stride in bytes, at the section 5.4 per-pipe
/// stride of `+0x1000`.
pub(crate) const PLANE_STRIDE_B: Register =
    Register::read_write("PLANE_STRIDE_B", 0x7_1188, Meaning::BringUp, None);

/// `PLANE_STRIDE` (plane 1, pipe C), reference section 5.4.
///
/// Pipe C's primary-plane surface stride in bytes, at the section 5.4 per-pipe
/// stride of `+0x2000`.
pub(crate) const PLANE_STRIDE_C: Register =
    Register::read_write("PLANE_STRIDE_C", 0x7_2188, Meaning::BringUp, None);

/// `PLANE_STRIDE` (plane 1, pipe D), reference section 5.4.
///
/// Pipe D's primary-plane surface stride in bytes, at the section 5.4 per-pipe
/// stride of `+0x3000`.
pub(crate) const PLANE_STRIDE_D: Register =
    Register::read_write("PLANE_STRIDE_D", 0x7_3188, Meaning::BringUp, None);

/// `PLANE_POS` (plane 1, pipe A), reference section 5.4.
///
/// Where the plane sits on screen: `[31:16]` is Y and `[15:0]` is X.  `noarm`
/// (section 5.6), so it may be written before the plane is armed; section 11
/// step 4.3 writes it with the other `noarm` registers, ahead of `PLANE_SURF`.
pub(crate) const PLANE_POS_A: Register =
    Register::read_write("PLANE_POS_A", 0x7_018c, Meaning::BringUp, None);

/// `PLANE_POS` (plane 1, pipe B), reference section 5.4.
///
/// Pipe B's primary-plane screen position, at the section 5.4 per-pipe stride
/// of `+0x1000`; `noarm`, like pipe A's.
pub(crate) const PLANE_POS_B: Register =
    Register::read_write("PLANE_POS_B", 0x7_118c, Meaning::BringUp, None);

/// `PLANE_POS` (plane 1, pipe C), reference section 5.4.
///
/// Pipe C's primary-plane screen position, at the section 5.4 per-pipe stride
/// of `+0x2000`; `noarm`, like pipe A's.
pub(crate) const PLANE_POS_C: Register =
    Register::read_write("PLANE_POS_C", 0x7_218c, Meaning::BringUp, None);

/// `PLANE_POS` (plane 1, pipe D), reference section 5.4.
///
/// Pipe D's primary-plane screen position, at the section 5.4 per-pipe stride
/// of `+0x3000`; `noarm`, like pipe A's.
pub(crate) const PLANE_POS_D: Register =
    Register::read_write("PLANE_POS_D", 0x7_318c, Meaning::BringUp, None);

/// `PLANE_SIZE` (plane 1, pipe A), reference section 5.4.
///
/// The plane's visible size: `[31:16]` is `height - 1` and `[15:0]` is
/// `width - 1`, the same minus-one convention as the timing registers
/// (section 6.1).  `noarm`; section 11 step 4.3 writes it for the single
/// full-screen plane the bring-up assumes.
pub(crate) const PLANE_SIZE_A: Register =
    Register::read_write("PLANE_SIZE_A", 0x7_0190, Meaning::BringUp, None);

/// `PLANE_SIZE` (plane 1, pipe B), reference section 5.4.
///
/// Pipe B's primary-plane visible size, `height - 1` / `width - 1`, at the
/// section 5.4 per-pipe stride of `+0x1000`.
pub(crate) const PLANE_SIZE_B: Register =
    Register::read_write("PLANE_SIZE_B", 0x7_1190, Meaning::BringUp, None);

/// `PLANE_SIZE` (plane 1, pipe C), reference section 5.4.
///
/// Pipe C's primary-plane visible size, `height - 1` / `width - 1`, at the
/// section 5.4 per-pipe stride of `+0x2000`.
pub(crate) const PLANE_SIZE_C: Register =
    Register::read_write("PLANE_SIZE_C", 0x7_2190, Meaning::BringUp, None);

/// `PLANE_SIZE` (plane 1, pipe D), reference section 5.4.
///
/// Pipe D's primary-plane visible size, `height - 1` / `width - 1`, at the
/// section 5.4 per-pipe stride of `+0x3000`.
pub(crate) const PLANE_SIZE_D: Register =
    Register::read_write("PLANE_SIZE_D", 0x7_3190, Meaning::BringUp, None);

/// `PLANE_OFFSET` (plane 1, pipe A), reference section 5.4.
///
/// The source offset inside the surface: `[31:16]` is Y and `[15:0]` is X.  It
/// is in the `arm` half of the commit (section 5.6) and section 11 step 4.3
/// writes it; it selects the first pixel the plane reads, so anything but zero
/// crops the surface.
pub(crate) const PLANE_OFFSET_A: Register =
    Register::read_write("PLANE_OFFSET_A", 0x7_01a4, Meaning::BringUp, None);

/// `PLANE_OFFSET` (plane 1, pipe B), reference section 5.4.
///
/// Pipe B's primary-plane source offset within the surface, at the section 5.4
/// per-pipe stride of `+0x1000`.
pub(crate) const PLANE_OFFSET_B: Register =
    Register::read_write("PLANE_OFFSET_B", 0x7_11a4, Meaning::BringUp, None);

/// `PLANE_OFFSET` (plane 1, pipe C), reference section 5.4.
///
/// Pipe C's primary-plane source offset within the surface, at the section 5.4
/// per-pipe stride of `+0x2000`.
pub(crate) const PLANE_OFFSET_C: Register =
    Register::read_write("PLANE_OFFSET_C", 0x7_21a4, Meaning::BringUp, None);

/// `PLANE_OFFSET` (plane 1, pipe D), reference section 5.4.
///
/// Pipe D's primary-plane source offset within the surface, at the section 5.4
/// per-pipe stride of `+0x3000`.
pub(crate) const PLANE_OFFSET_D: Register =
    Register::read_write("PLANE_OFFSET_D", 0x7_31a4, Meaning::BringUp, None);

/// `PLANE_SURF` (plane 1, pipe A), reference section 5.4.
///
/// The GGTT address the plane scans out from, `[31:12]`, with bit 2 as the
/// decrypt flag.  Writing it arms every other double-buffered register of the
/// plane: section 5.6 and the PRM both state that `PLANE_SURF` is the commit,
/// which is why section 11 step 4.3 writes it last, after `PLANE_CTL`.
pub(crate) const PLANE_SURF_A: Register =
    Register::read_write("PLANE_SURF_A", 0x7_019c, Meaning::BringUp, None);

/// `PLANE_SURF` (plane 1, pipe B), reference section 5.4.
///
/// Pipe B's primary-plane surface address and commit point, at the section 5.4
/// per-pipe stride of `+0x1000`.
pub(crate) const PLANE_SURF_B: Register =
    Register::read_write("PLANE_SURF_B", 0x7_119c, Meaning::BringUp, None);

/// `PLANE_SURF` (plane 1, pipe C), reference section 5.4.
///
/// Pipe C's primary-plane surface address and commit point, at the section 5.4
/// per-pipe stride of `+0x2000`.
pub(crate) const PLANE_SURF_C: Register =
    Register::read_write("PLANE_SURF_C", 0x7_219c, Meaning::BringUp, None);

/// `PLANE_SURF` (plane 1, pipe D), reference section 5.4.
///
/// Pipe D's primary-plane surface address and commit point, at the section 5.4
/// per-pipe stride of `+0x3000`.
pub(crate) const PLANE_SURF_D: Register =
    Register::read_write("PLANE_SURF_D", 0x7_319c, Meaning::BringUp, None);

/// `PLANE_SURFLIVE` (plane 1, pipe A), reference sections 5.4 and 11.
///
/// The surface address the plane is actually scanning, read from the live
/// state rather than from the double-buffered register.  Section 11 step 6.2
/// compares it with what was written to `PLANE_SURF`; zero means the plane
/// never armed, which step 4.3 attributes to a rejected address (alignment, or
/// a GGTT entry that is not valid).  Read-only.
pub(crate) const PLANE_SURFLIVE_A: Register =
    Register::read_only("PLANE_SURFLIVE_A", 0x7_01ac, Meaning::BringUp, None);

/// `PLANE_SURFLIVE` (plane 1, pipe B), reference section 5.4.
///
/// Pipe B's live primary-plane surface address, the readback that proves pipe
/// B's plane armed; section 5.4 per-pipe stride `+0x1000`.  Read-only.
pub(crate) const PLANE_SURFLIVE_B: Register =
    Register::read_only("PLANE_SURFLIVE_B", 0x7_11ac, Meaning::BringUp, None);

/// `PLANE_SURFLIVE` (plane 1, pipe C), reference section 5.4.
///
/// Pipe C's live primary-plane surface address, the readback that proves pipe
/// C's plane armed; section 5.4 per-pipe stride `+0x2000`.  Read-only.
pub(crate) const PLANE_SURFLIVE_C: Register =
    Register::read_only("PLANE_SURFLIVE_C", 0x7_21ac, Meaning::BringUp, None);

/// `PLANE_SURFLIVE` (plane 1, pipe D), reference section 5.4.
///
/// Pipe D's live primary-plane surface address, the readback that proves pipe
/// D's plane armed; section 5.4 per-pipe stride `+0x3000`.  Read-only.
pub(crate) const PLANE_SURFLIVE_D: Register =
    Register::read_only("PLANE_SURFLIVE_D", 0x7_31ac, Meaning::BringUp, None);

/// `PLANE_COLOR_CTL` (plane 1, pipe A), reference section 5.4.
///
/// The plane's alpha mode and colour-management enables: `ALPHA[5:4]`, CSC and
/// gamma.  It is in the `arm` half of the commit (section 5.6); section 11
/// step 4.3 writes it with alpha disabled and no CSC for the linear RGB
/// bring-up.
pub(crate) const PLANE_COLOR_CTL_A: Register =
    Register::read_write("PLANE_COLOR_CTL_A", 0x7_01cc, Meaning::BringUp, None);

/// `PLANE_COLOR_CTL` (plane 1, pipe B), reference section 5.4.
///
/// Pipe B's primary-plane alpha and colour-management control, at the section
/// 5.4 per-pipe stride of `+0x1000`.
pub(crate) const PLANE_COLOR_CTL_B: Register =
    Register::read_write("PLANE_COLOR_CTL_B", 0x7_11cc, Meaning::BringUp, None);

/// `PLANE_COLOR_CTL` (plane 1, pipe C), reference section 5.4.
///
/// Pipe C's primary-plane alpha and colour-management control, at the section
/// 5.4 per-pipe stride of `+0x2000`.
pub(crate) const PLANE_COLOR_CTL_C: Register =
    Register::read_write("PLANE_COLOR_CTL_C", 0x7_21cc, Meaning::BringUp, None);

/// `PLANE_COLOR_CTL` (plane 1, pipe D), reference section 5.4.
///
/// Pipe D's primary-plane alpha and colour-management control, at the section
/// 5.4 per-pipe stride of `+0x3000`.
pub(crate) const PLANE_COLOR_CTL_D: Register =
    Register::read_write("PLANE_COLOR_CTL_D", 0x7_31cc, Meaning::BringUp, None);

/// `PLANE_BUF_CFG` (plane 1, pipe A), reference sections 5.4 and 7.2.
///
/// The plane's DDB allocation: `PLANE_BUF_START[11:0]` and
/// `PLANE_BUF_END[27:16]`, encoded `((end - 1) << 16) | start`, with 12-bit
/// fields on ADL-P+ (section 7.2).  It is `noarm`, and section 11 step 4.1
/// gives the one plane the whole 4096-block DBUF as `0x0fff0000`; section 7.1
/// is explicit that a plane whose `PLANE_BUF_CFG` and `PLANE_WM` are left at
/// their reset values reads nothing, so this is not a tuning register.
pub(crate) const PLANE_BUF_CFG_A: Register =
    Register::read_write("PLANE_BUF_CFG_A", 0x7_027c, Meaning::BringUp, None);

/// `PLANE_BUF_CFG` (plane 1, pipe B), reference sections 5.4 and 7.2.
///
/// Pipe B's primary-plane DDB allocation, same start/end encoding as pipe A,
/// at the section 5.4 per-pipe stride of `+0x1000`.
pub(crate) const PLANE_BUF_CFG_B: Register =
    Register::read_write("PLANE_BUF_CFG_B", 0x7_127c, Meaning::BringUp, None);

/// `PLANE_BUF_CFG` (plane 1, pipe C), reference sections 5.4 and 7.2.
///
/// Pipe C's primary-plane DDB allocation, same start/end encoding as pipe A,
/// at the section 5.4 per-pipe stride of `+0x2000`.
pub(crate) const PLANE_BUF_CFG_C: Register =
    Register::read_write("PLANE_BUF_CFG_C", 0x7_227c, Meaning::BringUp, None);

/// `PLANE_BUF_CFG` (plane 1, pipe D), reference sections 5.4 and 7.2.
///
/// Pipe D's primary-plane DDB allocation, same start/end encoding as pipe A,
/// at the section 5.4 per-pipe stride of `+0x3000`.
pub(crate) const PLANE_BUF_CFG_D: Register =
    Register::read_write("PLANE_BUF_CFG_D", 0x7_327c, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe A, latency level 0), reference sections 5.4 and 7.3.
///
/// One watermark level: `PLANE_WM_EN[31]`, `PLANE_WM_IGNORE_LINES[30]`,
/// `PLANE_WM_LINES[26:14]` and `PLANE_WM_BLOCKS[11:0]`.  Section 11 step 4.2
/// programs level 0 with `EN`, the plane's whole DDB allocation and
/// `LINES(31)`; section 7.3 notes that the line field is 13 bits wide while 31
/// is the hardware maximum, and that a level which exceeds it must have `EN`
/// cleared.  A plane whose watermarks are left at reset reads nothing.
pub(crate) const PLANE_WM_0_A: Register =
    Register::read_write("PLANE_WM_0_A", 0x7_0240, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe A, latency level 1), reference sections 5.4 and 7.3.
///
/// Watermark level 1 of the levels the algorithm programs (section 7.4 step 1
/// reads latency levels 0-3 and 4-7 from PCode).  Section 7.3's generous
/// bring-up leaves it with `EN` cleared, because a level is usable only while
/// its line count stays inside the hardware maximum of 31.
pub(crate) const PLANE_WM_1_A: Register =
    Register::read_write("PLANE_WM_1_A", 0x7_0244, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe A, latency level 2), reference sections 5.4 and 7.3.
///
/// Watermark level 2 of the levels the algorithm programs.  Section 7.3's
/// generous bring-up leaves it with `EN` cleared; it becomes live only when
/// the real latency calculation of section 7.4 enables it.
pub(crate) const PLANE_WM_2_A: Register =
    Register::read_write("PLANE_WM_2_A", 0x7_0248, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe A, latency level 3), reference sections 5.4 and 7.3.
///
/// Watermark level 3 of the levels the algorithm programs, the last one PCode
/// returns in its first latency response.  Section 7.3's generous bring-up
/// leaves it with `EN` cleared.
pub(crate) const PLANE_WM_3_A: Register =
    Register::read_write("PLANE_WM_3_A", 0x7_024c, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe A, latency level 4), reference sections 5.4 and 7.3.
///
/// Watermark level 4 of the levels the algorithm programs, the first of the
/// second PCode latency response.  Section 7.3's generous bring-up leaves it
/// with `EN` cleared.
pub(crate) const PLANE_WM_4_A: Register =
    Register::read_write("PLANE_WM_4_A", 0x7_0250, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe A, latency level 5), reference sections 5.4 and 7.3.
///
/// Watermark level 5 of the levels the algorithm programs.  Section 7.3's
/// generous bring-up leaves it with `EN` cleared; it becomes live only when
/// the real latency calculation of section 7.4 enables it.
pub(crate) const PLANE_WM_5_A: Register =
    Register::read_write("PLANE_WM_5_A", 0x7_0254, Meaning::BringUp, None);

/// `PLANE_WM_SAGV` (plane 1, pipe A), `[I915]` `skl_universal_plane_regs.h:327-333`.
///
/// The watermark a plane uses while SAGV has the memory at a reduced
/// frequency.  It is **not** watermark level 6: the six levels section 7.3
/// describes end at `PLANE_WM_5_A` (`0x70254`), and `0x70258` is
/// `_PLANE_WM_SAGV_1_A`, a register with the same field layout and a different
/// meaning.  `[I915]` programs it only when `HAS_HW_SAGV_WM`
/// (`display/skl_watermark.c:743-748`), which is this platform exactly --
/// display version 13 and not a discrete GPU
/// (`display/intel_display_device.h:141`) -- so leaving it at reset is leaving
/// a watermark the pipe may be reading at reset, which section 7.1 says does
/// not work.
pub(crate) const PLANE_WM_SAGV_A: Register =
    Register::read_write("PLANE_WM_SAGV_A", 0x7_0258, Meaning::BringUp, None);

/// `PLANE_WM_SAGV_TRANS` (plane 1, pipe A), `[I915]` `skl_universal_plane_regs.h:335-341`.
///
/// The transition half of [`PLANE_WM_SAGV_A`], at the level-7 offset the
/// reference's `0x70240 + level*4` formula would name; `[I915]` writes it with
/// the SAGV pair (`display/skl_universal_plane.c:743-748`).
pub(crate) const PLANE_WM_SAGV_TRANS_A: Register =
    Register::read_write("PLANE_WM_SAGV_TRANS_A", 0x7_025c, Meaning::BringUp, None);

/// `PLANE_WM_TRANS` (plane 1, pipe A), `[I915]` `skl_universal_plane_regs.h:343-349`.
///
/// The transition watermark: the level at which the pipe starts the transition
/// between watermark levels, which `[I915]` computes from level 0 and writes
/// immediately after the levels (`display/skl_universal_plane.c:735-749`).
/// Section 5.6 names it in its `noarm` order next to `PLANE_WM(0..n)` and gives
/// no offset anywhere; the offset is `[I915]`'s, and the `_B`/`_C`/`_D`
/// instances below are that offset plus the section 5.4 per-pipe stride, like
/// every other register in this file.
pub(crate) const PLANE_WM_TRANS_A: Register =
    Register::read_write("PLANE_WM_TRANS_A", 0x7_0268, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe B, latency level 0), reference sections 5.4 and 7.3.
///
/// Pipe B's level-0 watermark, the level section 11 step 4.2 must program for
/// a plane on pipe B to read anything; section 5.4 per-pipe stride `+0x1000`.
pub(crate) const PLANE_WM_0_B: Register =
    Register::read_write("PLANE_WM_0_B", 0x7_1240, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe B, latency level 1), reference sections 5.4 and 7.3.
///
/// Pipe B's watermark level 1, left with `EN` cleared by the generous
/// bring-up of section 7.3 until the real latency calculation uses it.
pub(crate) const PLANE_WM_1_B: Register =
    Register::read_write("PLANE_WM_1_B", 0x7_1244, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe B, latency level 2), reference sections 5.4 and 7.3.
///
/// Pipe B's watermark level 2, left with `EN` cleared by the generous
/// bring-up of section 7.3 until the real latency calculation uses it.
pub(crate) const PLANE_WM_2_B: Register =
    Register::read_write("PLANE_WM_2_B", 0x7_1248, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe B, latency level 3), reference sections 5.4 and 7.3.
///
/// Pipe B's watermark level 3, the last one PCode returns in its first latency
/// response, left with `EN` cleared by the generous bring-up of section 7.3.
pub(crate) const PLANE_WM_3_B: Register =
    Register::read_write("PLANE_WM_3_B", 0x7_124c, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe B, latency level 4), reference sections 5.4 and 7.3.
///
/// Pipe B's watermark level 4, the first of the second PCode latency response,
/// left with `EN` cleared by the generous bring-up of section 7.3.
pub(crate) const PLANE_WM_4_B: Register =
    Register::read_write("PLANE_WM_4_B", 0x7_1250, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe B, latency level 5), reference sections 5.4 and 7.3.
///
/// Pipe B's watermark level 5, left with `EN` cleared by the generous
/// bring-up of section 7.3 until the real latency calculation uses it.
pub(crate) const PLANE_WM_5_B: Register =
    Register::read_write("PLANE_WM_5_B", 0x7_1254, Meaning::BringUp, None);

/// `PLANE_WM_SAGV` (plane 1, pipe B), `[I915]` `skl_universal_plane_regs.h:327-333`.
///
/// Pipe B's SAGV watermark, the register at level 6's offset that is not a
/// watermark level; section 5.4 per-pipe stride `+0x1000`.
pub(crate) const PLANE_WM_SAGV_B: Register =
    Register::read_write("PLANE_WM_SAGV_B", 0x7_1258, Meaning::BringUp, None);

/// `PLANE_WM_SAGV_TRANS` (plane 1, pipe B), `[I915]` `skl_universal_plane_regs.h:335-341`.
///
/// Pipe B's SAGV transition watermark, at level 7's offset rather than at a
/// level 7; section 5.4 per-pipe stride `+0x1000`.
pub(crate) const PLANE_WM_SAGV_TRANS_B: Register =
    Register::read_write("PLANE_WM_SAGV_TRANS_B", 0x7_125c, Meaning::BringUp, None);

/// `PLANE_WM_TRANS` (plane 1, pipe B), `[I915]` `skl_universal_plane_regs.h:343-349`.
///
/// Pipe B's transition watermark, the level `[I915]` writes straight after
/// pipe B's six watermark levels; section 5.4 per-pipe stride `+0x1000`.
pub(crate) const PLANE_WM_TRANS_B: Register =
    Register::read_write("PLANE_WM_TRANS_B", 0x7_1268, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe C, latency level 0), reference sections 5.4 and 7.3.
///
/// Pipe C's level-0 watermark, the level section 11 step 4.2 must program for
/// a plane on pipe C to read anything; section 5.4 per-pipe stride `+0x2000`.
pub(crate) const PLANE_WM_0_C: Register =
    Register::read_write("PLANE_WM_0_C", 0x7_2240, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe C, latency level 1), reference sections 5.4 and 7.3.
///
/// Pipe C's watermark level 1, left with `EN` cleared by the generous
/// bring-up of section 7.3 until the real latency calculation uses it.
pub(crate) const PLANE_WM_1_C: Register =
    Register::read_write("PLANE_WM_1_C", 0x7_2244, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe C, latency level 2), reference sections 5.4 and 7.3.
///
/// Pipe C's watermark level 2, left with `EN` cleared by the generous
/// bring-up of section 7.3 until the real latency calculation uses it.
pub(crate) const PLANE_WM_2_C: Register =
    Register::read_write("PLANE_WM_2_C", 0x7_2248, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe C, latency level 3), reference sections 5.4 and 7.3.
///
/// Pipe C's watermark level 3, the last one PCode returns in its first latency
/// response, left with `EN` cleared by the generous bring-up of section 7.3.
pub(crate) const PLANE_WM_3_C: Register =
    Register::read_write("PLANE_WM_3_C", 0x7_224c, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe C, latency level 4), reference sections 5.4 and 7.3.
///
/// Pipe C's watermark level 4, the first of the second PCode latency response,
/// left with `EN` cleared by the generous bring-up of section 7.3.
pub(crate) const PLANE_WM_4_C: Register =
    Register::read_write("PLANE_WM_4_C", 0x7_2250, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe C, latency level 5), reference sections 5.4 and 7.3.
///
/// Pipe C's watermark level 5, left with `EN` cleared by the generous
/// bring-up of section 7.3 until the real latency calculation uses it.
pub(crate) const PLANE_WM_5_C: Register =
    Register::read_write("PLANE_WM_5_C", 0x7_2254, Meaning::BringUp, None);

/// `PLANE_WM_SAGV` (plane 1, pipe C), `[I915]` `skl_universal_plane_regs.h:327-333`.
///
/// Pipe C's SAGV watermark, the register at level 6's offset that is not a
/// watermark level; section 5.4 per-pipe stride `+0x2000`.
pub(crate) const PLANE_WM_SAGV_C: Register =
    Register::read_write("PLANE_WM_SAGV_C", 0x7_2258, Meaning::BringUp, None);

/// `PLANE_WM_SAGV_TRANS` (plane 1, pipe C), `[I915]` `skl_universal_plane_regs.h:335-341`.
///
/// Pipe C's SAGV transition watermark, at level 7's offset rather than at a
/// level 7; section 5.4 per-pipe stride `+0x2000`.
pub(crate) const PLANE_WM_SAGV_TRANS_C: Register =
    Register::read_write("PLANE_WM_SAGV_TRANS_C", 0x7_225c, Meaning::BringUp, None);

/// `PLANE_WM_TRANS` (plane 1, pipe C), `[I915]` `skl_universal_plane_regs.h:343-349`.
///
/// Pipe C's transition watermark, the level `[I915]` writes straight after
/// pipe C's six watermark levels; section 5.4 per-pipe stride `+0x2000`.
pub(crate) const PLANE_WM_TRANS_C: Register =
    Register::read_write("PLANE_WM_TRANS_C", 0x7_2268, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe D, latency level 0), reference sections 5.4 and 7.3.
///
/// Pipe D's level-0 watermark, the level section 11 step 4.2 must program for
/// a plane on pipe D to read anything; section 5.4 per-pipe stride `+0x3000`.
pub(crate) const PLANE_WM_0_D: Register =
    Register::read_write("PLANE_WM_0_D", 0x7_3240, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe D, latency level 1), reference sections 5.4 and 7.3.
///
/// Pipe D's watermark level 1, left with `EN` cleared by the generous
/// bring-up of section 7.3 until the real latency calculation uses it.
pub(crate) const PLANE_WM_1_D: Register =
    Register::read_write("PLANE_WM_1_D", 0x7_3244, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe D, latency level 2), reference sections 5.4 and 7.3.
///
/// Pipe D's watermark level 2, left with `EN` cleared by the generous
/// bring-up of section 7.3 until the real latency calculation uses it.
pub(crate) const PLANE_WM_2_D: Register =
    Register::read_write("PLANE_WM_2_D", 0x7_3248, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe D, latency level 3), reference sections 5.4 and 7.3.
///
/// Pipe D's watermark level 3, the last one PCode returns in its first latency
/// response, left with `EN` cleared by the generous bring-up of section 7.3.
pub(crate) const PLANE_WM_3_D: Register =
    Register::read_write("PLANE_WM_3_D", 0x7_324c, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe D, latency level 4), reference sections 5.4 and 7.3.
///
/// Pipe D's watermark level 4, the first of the second PCode latency response,
/// left with `EN` cleared by the generous bring-up of section 7.3.
pub(crate) const PLANE_WM_4_D: Register =
    Register::read_write("PLANE_WM_4_D", 0x7_3250, Meaning::BringUp, None);

/// `PLANE_WM` (plane 1, pipe D, latency level 5), reference sections 5.4 and 7.3.
///
/// Pipe D's watermark level 5, left with `EN` cleared by the generous
/// bring-up of section 7.3 until the real latency calculation uses it.
pub(crate) const PLANE_WM_5_D: Register =
    Register::read_write("PLANE_WM_5_D", 0x7_3254, Meaning::BringUp, None);

/// `PLANE_WM_SAGV` (plane 1, pipe D), `[I915]` `skl_universal_plane_regs.h:327-333`.
///
/// Pipe D's SAGV watermark, the register at level 6's offset that is not a
/// watermark level; section 5.4 per-pipe stride `+0x3000`.
pub(crate) const PLANE_WM_SAGV_D: Register =
    Register::read_write("PLANE_WM_SAGV_D", 0x7_3258, Meaning::BringUp, None);

/// `PLANE_WM_SAGV_TRANS` (plane 1, pipe D), `[I915]` `skl_universal_plane_regs.h:335-341`.
///
/// Pipe D's SAGV transition watermark, at level 7's offset rather than at a
/// level 7; section 5.4 per-pipe stride `+0x3000`.
pub(crate) const PLANE_WM_SAGV_TRANS_D: Register =
    Register::read_write("PLANE_WM_SAGV_TRANS_D", 0x7_325c, Meaning::BringUp, None);

/// `PLANE_WM_TRANS` (plane 1, pipe D), `[I915]` `skl_universal_plane_regs.h:343-349`.
///
/// Pipe D's transition watermark, the level `[I915]` writes straight after
/// pipe D's six watermark levels; section 5.4 per-pipe stride `+0x3000`.
pub(crate) const PLANE_WM_TRANS_D: Register =
    Register::read_write("PLANE_WM_TRANS_D", 0x7_3268, Meaning::BringUp, None);

/*
 * Transcription notes
 *
 * Offsets the document does not state, and which were therefore not transcribed:
 * - `PLANE_KEYMSK` and `PLANE_KEYMAX` (and `PLANE_KEYVAL` beside them): section 5.6's `arm` order
 *   writes all three to zero for no colour key and section 11 step 4.3 calls them "key registers =
 *   0", but no section gives any of the three an offset.  Not declared and not guessed, so a plane
 *   with an explicit colour key cannot be programmed from this transcription.
 * - `PLANE_AUX_DIST` and `PLANE_AUX_OFFSET`: named in section 5.6's `arm` order ("0 for
 *   single-plane formats") with no offset anywhere in the document.  Not declared.
 * - `PLANE_NV12_BUF_CFG`: the document never mentions this register -- the string `NV12` does not
 *   occur in it -- so there is neither a placement nor an offset to copy.  Not declared.
 * - `PIPE_FIFO_UNDERRUN_STATUS`: not a register of its own.  Sections 5.2, 10.4 and 10.8 all place
 *   it at bit 31 of the per-pipe `PIPESTAT` (`0x70024 + pipe * 0x1000`), so it is carried by the
 *   four `PIPESTAT_*` constants rather than by a constant of its own.
 * - `WM_LINETIME` (`0x45270` pipe A, `0x45274` pipe B, section 7.4 step 3) has offsets for pipes A
 *   and B and none stated for pipes C and D; it is also deliberately left out entirely -- see the
 *   last list below.
 * - The pipe B/C/D instance of every register in this file: the document tabulates pipe A and
 *   states the stride rather than tabulating the rest (section 5.2: "Pipe B adds `0x1000`, C adds
 *   `0x2000`, D adds `0x3000`"; section 5.4: "`+0x100` per plane, `+0x1000` per pipe"; section
 *   10.4: "`PIPESTAT(pipe)` is at `0x70024 + pipe*0x1000`").  Every `_B`/`_C`/`_D` offset above is
 *   pipe A plus that stated stride; recorded here so the arithmetic is visible rather than silent.
 *   The one instance the document states outright is `PIPESTAT`'s `0x70024 + pipe * 0x1000`.
 * - Sprite planes 2-5: section 5.4 states the `+0x100` per-plane stride and names one plane-2
 *   register family only through that stride, but tabulates no concrete plane-2..5 offset, so no
 *   instance beyond plane 1 could be transcribed even if the bring-up needed one.  It does not:
 *   section 11's preamble assumes "one plane" and phase 4.3 programs that plane.
 * - The offset of the `PLANE_WM` *level* is the one offset this file computes rather than copies
 *   character by character: the document gives it as a formula (`0x70240 + level*4`, sections 5.4
 *   and 7.3), and the constants are the six literal results of evaluating it for levels 0-5.  The
 *   formula's results for 6 and 7 are *not* levels: they are `PLANE_WM_SAGV` and
 *   `PLANE_WM_SAGV_TRANS` (`skl_universal_plane_regs.h:327`, `:335`), which is why the level count
 *   is six and those two offsets have constants of their own.
 *
 * Offsets the document does not state and which were therefore taken from `[I915]`:
 * - `PLANE_WM_TRANS` (`0x70268`): named in section 5.6's `noarm` order next to `PLANE_WM(0..n)`
 *   with no offset anywhere in the document.  `[I915]` defines
 *   `_PLANE_WM_TRANS_1_A 0x70268` and writes it immediately after the levels
 *   (`skl_universal_plane_regs.h:343-349`, `skl_universal_plane.c:735-749`), so the offset is
 *   sourced, not guessed.
 * - `PLANE_WM_SAGV` (`0x70258`) and `PLANE_WM_SAGV_TRANS` (`0x7025c`): the document's formula
 *   reaches them and calls them levels 6 and 7.  `[I915]` names them, programs them only under
 *   `HAS_HW_SAGV_WM`, and gives the six-level count that says the formula's last two results are
 *   not levels (`skl_universal_plane_regs.h:327-341`, `skl_watermark.c:3379-3383`,
 *   `intel_display_device.h:141`).  Declared, and recorded as a reference defect in
 *   `docs/design/intel-pipe.md`.
 * - `PIPE_ARB_CTL` (`0x70028`, `USE_PROG_SLOTS[13]`): section 5.2 states the offset and the field,
 *   so this one is the document's; `[I915]`'s `_PIPE_ARB_CTL_A 0x70028` and
 *   `PIPE_ARB_USE_PROG_SLOTS REG_BIT(13)` (`i915_reg.h:1704-1706`) agree with it.
 *
 * Places where the document's two mentions of a register disagree:
 * - `TRANSCONF`'s output bit depth: section 8.6's sequence writes
 *   `TRANSCONF = ENABLE | STATE_ENABLE | progressive | 8bpc`, while section 8.4 ("An earlier draft
 *   attributed these to `TRANSCONF`; that was wrong") and section 11 step 5.6 ("Output bit depth
 *   is **not** set here on Gen12") put the bit depth and dithering in `PIPE_MISC`; section 15's
 *   colophon lists the same correction.  The `PIPE_MISC` reading is the one this transcription
 *   assumes, which is why `PIPE_MISC_*` is declared writable and `PIPECONF_*`'s comment says what
 *   does not go into it.  Section 8.6 item 12 still needs correcting in the document.
 * - `PIPESTAT`'s field halves: section 10.4 says the status half is `[15:0]` and the enable half
 *   `[30:16]` (`PIPESTAT_INT_ENABLE_MASK = 0x7fff0000`), which leaves bit 31 in neither half,
 *   while sections 5.2 and 11 step 6.4 read `PIPE_FIFO_UNDERRUN_STATUS` from bit 31 of the same
 *   per-pipe register.  Flagged, not resolved; it becomes load-bearing only when a later phase
 *   writes the enable half.
 * - `PIPESRC`'s group and instance: section 5.2 lists it in the *pipe* register table, but its
 *   offset (`0x6001c`) is inside the transcoder block of section 5.3 and section 6.1 writes it per
 *   transcoder, `PIPESRC(T)`.  Section 5.1 ties transcoders 1:1 to pipes, so the instance is the
 *   same and the constants are named after the pipe; not a disagreement about the offset (both
 *   mentions, and section 5.2's `+0x1000` pipe stride, agree on `0x6001c` for pipe A).
 * - `PIPESRC`'s name: the name this task's group list uses is `PIPE_SRCSZ`, which does not occur
 *   anywhere in the document; the document's name is `PIPESRC` (sections 5.2 and 6.1).  Declared
 *   as `PIPESRC_*`.
 * - `PIPECONF` vs `TRANSCONF`: a rename, not a disagreement, but recorded because it reads like
 *   one.  Section 5.2's table row is `TRANSCONF` (a.k.a. `PIPECONF`; "i915 renamed it") at
 *   `0x70008`, its naming warning says the PRM and every older source call it `PIPECONF` and that
 *   there is no separate register, and section 11 step 5.6 writes it as `TRANSCONF(A)` at the same
 *   offset.  Declared once, as `PIPECONF_*`; a reader who prefers i915's name should rename the
 *   four constants, not add a second set.
 * - `PLANE_WM`'s level count: the document never states how many watermark levels exist.  Section
 *   7.4 step 1 has PCode return latency levels 0-3 and 4-7, and section 7.3 says each watermark
 *   level corresponds to a memory latency level, but `[I915]` programs six levels on this platform
 *   -- `skl_setup_wm_latency` sets `num_levels = 6` under `HAS_HW_SAGV_WM`
 *   (`skl_watermark.c:3379-3383`), which is `DISPLAY_VER >= 13 && !IS_DGFX`
 *   (`intel_display_device.h:141`), and ADL-N is both.  Six are declared per pipe; the document's
 *   levels 6 and 7 are the SAGV pair above, and no section states an offset, a field, or an
 *   existence for a level 8 or beyond.
 * - Checked and *not* disagreements, each mention compared: `PLANE_BUF_CFG` is `0x7027c` for plane
 *   1 of pipe A in both section 5.4's table and section 7.2 (it sits in the tail of plane 1's
 *   `0x100`-byte slot, not at the plane's base -- consistent, not contradictory); `PLANE_WM` is
 *   `0x70240 + level*4` in both section 5.4 and section 7.3; `PIPESTAT` is `0x70024` in section
 *   5.2 and `0x70024 + pipe*0x1000` in section 10.4; `PIPE_MISC` is `0x70030` in sections 5.2,
 *   8.4 and 11; `TRANSCONF` is `0x70008` in sections 5.2 and 8.6; `PIPEDSL` is `0x70000` in
 *   sections 5.2, 10.4 and 12.3.
 *
 * Registers deliberately left out:
 * - `PLANE_CUS_CTL` (`0x701c8`): offset stated in section 5.4, but it is the HDR-plane chroma
 *   upsampler and section 5.6 says the disable path that clears it "does not apply to a first
 *   light-up"; section 11 never writes it.
 * - `WM_LINETIME` (`0x45270` pipe A, `0x45274` pipe B, section 7.4 step 3): a *pipe*-level
 *   watermark register, not the plane's, and section 11 phase 4.2 points at section 7.3, whose
 *   generous level-0 approach uses only `PLANE_WM` and `PLANE_BUF_CFG`.  Section 7.4 -- the full
 *   algorithm the bring-up defers -- is where it is programmed, and the document states no offset
 *   for pipes C and D.  It is the first register to add when the real watermark calculation is
 *   implemented.
 * - The sprite planes 2-5 and the cursor plane: section 11's preamble assumes "one plane", and
 *   section 5.4 tabulates no concrete offsets for them (see the offsets list above).
 * - `PLANE_KEYVAL`: named beside `PLANE_KEYMSK`/`PLANE_KEYMAX` in section 5.6 with no offset; it
 *   was not in this group's list and shares their gap.
 * - `SEL_FETCH_PLANE_CTL` and the PSR2 selective-fetch path: section 5.6 says the ICL disable
 *   variant clears it only when PSR2 selective fetch was on, "neither applies to a first
 *   light-up", and the document gives it no offset.
 * - The DBUF slice registers `DBUF_CTL_S0`..`S3`: already declared in `regs.rs`
 *   (`POWER_AND_CLOCK_REGISTERS`) from section 4.7, and section 11 phase 1.5 -- enabling the
 *   slices -- is the power group's step; this group only *allocates* the buffer to a plane,
 *   through `PLANE_BUF_CFG_*`.
 * - A writable `PIPESTAT`: declared read-only because the section 11 sequence only reads it (step
 *   6.4) and defers interrupts to the hardening pass (step 6.6).  Section 10.4's vblank-enable
 *   write and section 10.8's underrun-interrupt enable are a later phase's addition to the table,
 *   not a change to this one.
 * - "Prove it" readbacks outside this group: section 11 phase 6.3's `DDI_BUF_CTL.IS_IDLE` is a DDI
 *   register and belongs to the DDI group.  This group's read-only constants are `PIPEDSL_*`,
 *   `PLANE_SURFLIVE_*` and `PIPESTAT_*`, which are the phase 6.1, 6.2 and 6.4 readbacks.
 */
