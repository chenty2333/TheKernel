//! One block of the display engine's register table.
//!
//! Owner: [`super`].  Every offset here cites the reference section it came
//! from, and the access classification is the sequence's: a register the
//! bring-up writes is `read_write`, one it only reads back is `read_only`.

use super::{Meaning, Register};

/// `DEIMR`, the display engine's top-level interrupt mask, reference section 10.2.
///
/// One bit per first-level block: `GEN8_DE_MISC_IRQ`, `GEN8_DE_PORT_IRQ` and
/// `GEN8_DE_PIPE_x_IRQ` (section 10.3).  A set bit means masked, so a handler
/// writes 1 here before enabling a block and 0 once the source is quiescent,
/// which is the section 10.6 ordering.
pub(crate) const DEIMR: Register = Register::read_write("DEIMR", 0x4_4004, Meaning::BringUp, None);

/// `DEIIR`, the display engine's first-level pending interrupts, reference section 10.2.
///
/// Reading it gives the currently pending *unmasked* interrupts and writing 1 to
/// a bit clears it, which is the snapshot-then-clear pattern section 10.7 uses.
pub(crate) const DEIIR: Register = Register::read_write("DEIIR", 0x4_4008, Meaning::BringUp, None);

/// `DEIER`, the display engine's top-level interrupt enable, reference section 10.2.
///
/// Section 10.4 routes `GEN8_DE_PIPE_A_IRQ` up through this register, while
/// section 10.3 says i915 never writes it on Gen12 because `DISPLAY_INT_CTL`
/// is the gate; the disagreement is recorded in the notes below, and this
/// declaration follows section 10.4.
pub(crate) const DEIER: Register = Register::read_write("DEIER", 0x4_400c, Meaning::BringUp, None);

/// `DISPLAY_INT_CTL`, the ultimate display interrupt gate, reference section 10.3.
///
/// Bit 31 (`DISPLAY_IRQ_ENABLE`) must be set before any display interrupt
/// propagates, and it is *not* bit 31 of `DEISR`/`DEIER`: section 10.3 records
/// that the legacy `DE_MASTER_IRQ_CONTROL` is the easy wrong answer, and that
/// `gen11_de_irq_postinstall` writes this register and nothing else.
pub(crate) const DISPLAY_INT_CTL: Register =
    Register::read_write("DISPLAY_INT_CTL", 0x4_4200, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IMR` (pipe A), reference section 10.2.
///
/// Masks pipe A's per-pipe display interrupts at their source; set means masked,
/// so it is written 1 before the matching enable and 0 after it (section 10.6).
pub(crate) const GEN8_DE_PIPE_IMR_A: Register =
    Register::read_write("GEN8_DE_PIPE_IMR_A", 0x4_4404, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IIR` (pipe A), reference section 10.2.
///
/// Reading it snapshots what is pending and unmasked, and writing the value back
/// clears the handled bits (write-one-to-clear, section 10.7).
pub(crate) const GEN8_DE_PIPE_IIR_A: Register =
    Register::read_write("GEN8_DE_PIPE_IIR_A", 0x4_4408, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IER` (pipe A), reference section 10.2.
///
/// The enable half of the per-pipe block, below the top-level
/// `GEN8_DE_PIPE_A_IRQ` bit: section 10.4's bit table is where the vblank and
/// FIFO-underrun bits a handler reports on are named.
pub(crate) const GEN8_DE_PIPE_IER_A: Register =
    Register::read_write("GEN8_DE_PIPE_IER_A", 0x4_440c, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IMR` (pipe B), reference section 10.2.
///
/// Masks pipe B's per-pipe display interrupts at their source; set means masked,
/// so it is written 1 before the matching enable and 0 after it (section 10.6).
pub(crate) const GEN8_DE_PIPE_IMR_B: Register =
    Register::read_write("GEN8_DE_PIPE_IMR_B", 0x4_4414, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IIR` (pipe B), reference section 10.2.
///
/// Reading it snapshots what is pending and unmasked, and writing the value back
/// clears the handled bits (write-one-to-clear, section 10.7).
pub(crate) const GEN8_DE_PIPE_IIR_B: Register =
    Register::read_write("GEN8_DE_PIPE_IIR_B", 0x4_4418, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IER` (pipe B), reference section 10.2.
///
/// The enable half of pipe B's per-pipe block; a pipe that is fused off (section
/// 13.1 item 3 makes which pipes exist a `SKL_DFSM` read) has no event to take.
pub(crate) const GEN8_DE_PIPE_IER_B: Register =
    Register::read_write("GEN8_DE_PIPE_IER_B", 0x4_441c, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IMR` (pipe C), reference section 10.2.
///
/// Masks pipe C's per-pipe display interrupts at their source; set means masked,
/// so it is written 1 before the matching enable and 0 after it (section 10.6).
pub(crate) const GEN8_DE_PIPE_IMR_C: Register =
    Register::read_write("GEN8_DE_PIPE_IMR_C", 0x4_4424, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IIR` (pipe C), reference section 10.2.
///
/// Reading it snapshots what is pending and unmasked, and writing the value back
/// clears the handled bits (write-one-to-clear, section 10.7).
pub(crate) const GEN8_DE_PIPE_IIR_C: Register =
    Register::read_write("GEN8_DE_PIPE_IIR_C", 0x4_4428, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IER` (pipe C), reference section 10.2.
///
/// The enable half of pipe C's per-pipe block, which the bring-up does not use
/// (it programs pipe A alone) but which has to exist as a name before a second
/// pipe can be brought up without inventing an offset.
pub(crate) const GEN8_DE_PIPE_IER_C: Register =
    Register::read_write("GEN8_DE_PIPE_IER_C", 0x4_442c, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IMR` (pipe D), reference section 10.2.
///
/// Masks pipe D's per-pipe display interrupts at their source; set means masked,
/// so it is written 1 before the matching enable and 0 after it (section 10.6).
pub(crate) const GEN8_DE_PIPE_IMR_D: Register =
    Register::read_write("GEN8_DE_PIPE_IMR_D", 0x4_4434, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IIR` (pipe D), reference section 10.2.
///
/// Reading it snapshots what is pending and unmasked, and writing the value back
/// clears the handled bits (write-one-to-clear, section 10.7).
pub(crate) const GEN8_DE_PIPE_IIR_D: Register =
    Register::read_write("GEN8_DE_PIPE_IIR_D", 0x4_4438, Meaning::BringUp, None);

/// `GEN8_DE_PIPE_IER` (pipe D), reference section 10.2.
///
/// The enable half of pipe D's per-pipe block; like pipe C it is declared for
/// completeness of the per-pipe set rather than because section 11 programs it.
pub(crate) const GEN8_DE_PIPE_IER_D: Register =
    Register::read_write("GEN8_DE_PIPE_IER_D", 0x4_443c, Meaning::BringUp, None);

/// `SDEIMR`, the south display's interrupt mask, reference section 10.2.
///
/// Masks the south display's sources -- the per-DDI hotplug pulses, the GMBUS
/// completion and PICA (section 10.5).  Section 10.6's mask, enable, unmask
/// ordering is stated for exactly this block, so it cannot be followed without
/// this register.
pub(crate) const SDEIMR: Register =
    Register::read_write("SDEIMR", 0xc_4004, Meaning::BringUp, None);

/// `SDEIIR`, the south display's pending interrupts, reference section 10.2.
///
/// Where a hotplug event is read and acknowledged: section 10.7 reads it,
/// handles what it names, and writes the value back to clear it.
pub(crate) const SDEIIR: Register =
    Register::read_write("SDEIIR", 0xc_4008, Meaning::BringUp, None);

/// `SDEIER`, the south display's interrupt enable, reference section 10.2.
///
/// One bit per DDI hotplug source (`SDE_DDI_HOTPLUG_ICP`, bits 16 and up), plus
/// the GMBUS-complete bit that section 10.5 says this driver polls instead of
/// taking.  `hpd` deliberately does not write it yet; this is the name the
/// interrupt step needs when it does.
pub(crate) const SDEIER: Register =
    Register::read_write("SDEIER", 0xc_400c, Meaning::BringUp, None);

/*
 * Transcription notes
 * ===================
 *
 * Already declared in `kernel/src/drm/intel/regs.rs`, so not declared again here:
 * - `GSMBASE` (`0x10_8100`, read-only 64-bit, `Meaning::GttBase`, in `NAMED`) is the document's
 *   `GEN6_GSMBASE` (section 1.1, base in bits `[63:20]`).  It is the only register of the
 *   graphics translation table the document names, so this group adds no GGTT register at all.
 * - `GGC` (`0x10_8040`) and `DSMBASE` (`0x10_80c0`) sit in the same stolen-memory block that
 *   section 2.1 calls forcewake-free; both are present already and neither is this group's.
 * - `GEN8_CHICKEN_DCPR_1` (`0x4_6430`, read-write) is section 4.5 item 1 and section 11 phase 1.3
 *   (`Wa_16013190616`, `DISABLE_FLR_SRC` bit 15); present already, and not repeated here.
 * - `GEN11_CHICKEN_DCPR_2` (`0x4_6434`, read-write) is section 4.5 item 2, section 4.9 step 10 and
 *   section 11 phase 1.6 (`Wa_14011508470`); present already.
 * - `XELPD_DISPLAY_ERR_FATAL_MASK` (`0x4_421c`, read-write) is section 4.9 step 11 and section 11
 *   phase 1.6 (`Wa_14011503030`); present already, at the offset the document gives in both places.
 * - `DBUF_CTL_S0`..`S3` (`0x4_5008`, `0x4_4fe8`, `0x4_4300`, `0x4_4304`, read-write) are section
 *   4.7's table character for character, with the slice-numbering trap written down beside them.
 * - `SDEISR` (`0xc4000`, read-only, `Meaning::Hotplug`, in `BUS`) is section 10.2's south display
 *   ISR and the live-connect read of sections 9.4 and 11 phase 2.2; present already, which is why
 *   only its IMR/IIR/IER siblings are new here.
 * - The rest of `regs.rs`'s tables (power wells, CDCLK, the combo PHYs, GMBUS, the hotplug-detect
 *   registers) are the other four groups' registers, and no offset in this file is one of them.
 *
 * Offsets I could not find in the document, and did not need:
 * - `TRANS_CMTG_CHICKEN` / `DISABLE_DPT_CLK_GATING` (section 4.5 item 3): the document gives no
 *   offset for it anywhere, and none was needed, because ADL-N never runs that workaround (its
 *   stepping table maps `0x0` to `STEP_D0`, outside the `STEP_A0`..`STEP_B0` gate).
 * - A register for the graphics translation table itself: the document names none.  Section 3.2
 *   describes the GGTT only as BAR 0's second 8 MiB (base offset `0x800000`) and as BAR 2 `GMADR`,
 *   the CPU aperture the document says not to map; section 11 phase 3.2 says "write the GGTT PTE"
 *   without naming a register or an offset for the page-table base beyond `GEN6_GSMBASE`.
 *
 * Where the document's two mentions of a register disagreed:
 * - `DEIER` (`0x4_400c`): section 10.4 says to route `GEN8_DE_PIPE_A_IRQ` "up through `DEIER`",
 *   while section 10.3 says "i915 never writes `DEIER` on Gen12" and names `DISPLAY_INT_CTL` as the
 *   gate; section 10.2 lists `DEIER` with the generic IER semantics.  Declared read-write on
 *   section 10.4's instruction, which is the one a bring-up of this kernel would follow; if the
 *   section 10.3 reading is preferred, this is the single constant to change to `read_only`.
 * - `DEISR` bit 31: section 10.3 records that an earlier draft of the document used it as the
 *   global enable, and that on Gen12 it is the legacy `DE_MASTER_IRQ_CONTROL` instead.  Nothing
 *   here declares `DEISR` at all, so no caller can reach for the wrong bit through this file.
 * - The register window inside BAR 0: section 1.1 says the first 8 MiB of BAR 0 is the register
 *   window, while section 3.2 says i915 maps only the first 2 MiB of it.  `regs.rs`'s
 *   `PROBE_WINDOW` (`0x20_0000`) follows section 3.2, and no register in this group is affected.
 * - The DBUF slices: section 4.7's prose calls them `DBUF_S1`..`DBUF_S4` while its own table names
 *   the registers `DBUF_CTL_S0`..`S3`; the document explains the renumbering itself, and `regs.rs`
 *   already follows the table's names and offsets.
 * - Section 10.4's bit table is captioned "Per-pipe interrupts", but its citation and the paragraph
 *   beneath it describe `PIPESTAT` (`0x70024 + pipe*0x1000`), whose status half is `[15:0]` and
 *   enable half `[30:16]` -- not the bit 31 and bit 26 the table lists.  No ISR bit is decoded in
 *   this file, so nothing here depends on which register that table is describing.
 *
 * Deliberately left out, and why:
 * - `DEISR` (`0x44000`), `GEN8_DE_PORT_ISR/IMR/IIR/IER` (`0x44440`/`0x44444`/`0x44448`/`0x4444c`),
 *   `GEN8_DE_MISC_ISR/IMR/IIR/IER` (`0x44460`/`0x44464`/`0x44468`/`0x4446c`) and
 *   `GEN8_DE_PIPE_ISR(A..D)` (`0x44400 + pipe*0x10`): section 10.2 documents all of them, but the
 *   hotplug step this group stocks writes enables, masks and clears rather than reading status,
 *   and nothing in section 11 or in step 6 reads them.  Section 10.7's clearing snippet would add
 *   `GEN8_DE_PORT_IIR` (`0x44448`) as one line if the handler ends up routing through that block.
 * - `BW_BUDDY_CTL` (`0x45130`/`0x45140`): named only in section 4.9 step 7, which is outside this
 *   group's sections, and written by display core init rather than by any sequence here.
 * - Step 6's read-backs -- `PIPEDSL(A)` (`0x70000`), `PLANE_SURFLIVE(A,1)`, `DDI_BUF_CTL(A)`'s
 *   `IS_IDLE` and `PIPESTAT(A)` (`0x70024`) bit 31 -- are pipe, plane and DDI registers; nothing in
 *   this file is what step 6 reads, so this group contributes no read-back register.
 * - BAR 0 `GTTMMADR` and BAR 2 `GMADR`: barriers, not registers, and a `Register` cannot express
 *   either.  The aperture's role, including the 2 MiB the probe maps of the 16 MiB BAR, is already
 *   written down in `regs.rs`'s `PROBE_WINDOW` documentation.
 *
 * Added beyond the brief's list, with the reason:
 * - `DISPLAY_INT_CTL` (`0x4_4200`): section 10.3 makes it the gate every other display interrupt
 *   register depends on ("enable this first"), so the enable set would be inert without it.
 * - `SDEIMR` (`0xc_4004`): section 10.6's mask, enable, unmask ordering is stated for the south
 *   display (SCDC/hotplug) interrupt, and it cannot be followed without this register.
 *
 * Naming: the per-pipe constants *and* their name strings carry the `_A`..`_D` suffix, as the pipe
 * group's `PIPESTAT_A`..`D` do, because the document's own name for these is parameterised
 * (`GEN8_DE_PIPE_IMR(pipe)`) and an instantiated name string has to be unique -- `regs.rs`'s mock
 * keys its write log and its refusals on the name.
 */
