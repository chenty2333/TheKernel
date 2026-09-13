//! The pipe sequence driven against the mock register file.
//!
//! What a test here is worth: [`MockRegisters`] proves which registers were
//! written, in which order, with which values, and what the sequence does when
//! a write is refused or a status bit never appears.  It cannot prove that any
//! of those values is right, and nothing in this file has been near real
//! silicon.  So the tests assert the properties the sequence exists to
//! guarantee -- the timing values are `timing.rs`'s, the plane is armed after
//! the watermarks and not before, a failure leaves no surface address written,
//! an underrun names the watermarks -- rather than "the function returned `Ok`".

use alloc::vec::Vec;
use core::cell::Cell;

use super::*;
use crate::drm::{
    intel::regs::{Register, mock::MockRegisters},
    modes::{CTA_VIC_TIMINGS, DMT_TIMINGS},
};

// ---------------------------------------------------------------------------
// The two things a test supplies
// ---------------------------------------------------------------------------

/// CTA-861 VIC 16: 1920x1080@60, 148.5 MHz, htotal 2200, vtotal 1125.
///
/// Section 11 phase 3.1 names it as the mode to prefer for a first light-up and
/// section 6.3 uses it as its worked example, so it is the mode every value in
/// these tests is checked against.  VESA DMT 0x52 publishes the same timing, and
/// `the_same_timing_from_two_tables_packs_identically` in `timing.rs` is why
/// either can stand for the other.
fn vic16() -> Mode {
    CTA_VIC_TIMINGS
        .iter()
        .find(|entry| entry.vic == 16)
        .expect("VIC 16 is in the kernel's CTA table")
        .mode
}

/// VESA DMT 0x04: 640x480@60, 25.175 MHz.
fn dmt_0x04() -> Mode {
    DMT_TIMINGS
        .iter()
        .find(|entry| entry.code == 0x04)
        .expect("DMT 0x04 is in the kernel's table")
        .mode
}

/// The surface these tests scan out: 16 MiB into the graphics address space,
/// which is 4 KiB-aligned, and a 7680-byte stride, which is what a 1920-wide
/// XRGB8888 surface needs and is a multiple of 64.
const SURFACE: PlaneSurface = PlaneSurface {
    ggtt_address: 0x0100_0000,
    stride_bytes: 7680,
};

/// A clock the test owns, so a millisecond costs no wall time.
struct FakeClock {
    micros: Cell<u64>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            micros: Cell::new(0),
        }
    }
}

impl PollTimer for FakeClock {
    fn now_micros(&self) -> u64 {
        self.micros.get()
    }

    fn pause(&self) {
        // Two microseconds, the same step the machine's own timer takes.
        self.micros.set(self.micros.get() + 2);
    }
}

/// A clock that never advances, which is the one way a `PollTimer` can break
/// its contract.
struct StuckClock;

impl PollTimer for StuckClock {
    fn now_micros(&self) -> u64 {
        0
    }

    fn pause(&self) {}
}

/// A program for VIC 16 on pipe A, plus the register file it was written to.
struct Programmed {
    plan: PipeProgram,
    regs: MockRegisters,
    state: PipeState,
}

impl Programmed {
    fn new() -> Self {
        Self::on(Pipe::A, SURFACE, &vic16())
    }

    fn on(pipe: Pipe, surface: PlaneSurface, mode: &Mode) -> Self {
        let plan = compute(pipe, mode, surface).expect("a well-formed progressive mode");
        let regs = MockRegisters::new();
        let state = program(&regs, &plan).expect("the mock refuses nothing");
        Self { plan, regs, state }
    }

    /// The write log as `(name, value)`, which is what the order tests assert
    /// on.
    fn writes(&self) -> Vec<(&'static str, u32)> {
        self.regs.writes()
    }

    /// The index of a register's write in the log.
    fn index_of(&self, register: Register) -> usize {
        self.writes()
            .iter()
            .position(|(name, _)| *name == register.name())
            .unwrap_or_else(|| panic!("{} was never written", register.name()))
    }
}

// ---------------------------------------------------------------------------
// The write order
// ---------------------------------------------------------------------------

/// The exact write order of section 11 phases 3.4 and 4.
///
/// Asserted as a whole sequence rather than as a handful of relative positions,
/// because the property section 5.6 establishes is about the sequence: the
/// `noarm` values, the DDB and the watermarks are all double buffered, and the
/// only write that makes any of them take effect is the last one.
#[test]
fn the_write_order_is_phase_3_4_then_4_1_then_4_2_then_4_3() {
    let programmed = Programmed::new();
    let names: Vec<&'static str> = programmed.writes().iter().map(|(name, _)| *name).collect();

    assert_eq!(
        names,
        [
            // The pipe's output depth, first so that `PLANE_SURF` is the last
            // write of the whole sequence; see `PipeProgram::writes`.
            "PIPE_MISC_A",
            // Phase 3.4, in `timing.rs`'s own order (section 5.3's table).
            "TRANS_HTOTAL(A)",
            "TRANS_HBLANK(A)",
            "TRANS_HSYNC(A)",
            "TRANS_VTOTAL(A)",
            "TRANS_VBLANK(A)",
            "TRANS_VSYNC(A)",
            "PIPESRC_A",
            // Phase 4.1.
            "PLANE_BUF_CFG_A",
            // Phase 4.2: every level, including the disabled ones.
            "PLANE_WM_0_A",
            "PLANE_WM_1_A",
            "PLANE_WM_2_A",
            "PLANE_WM_3_A",
            "PLANE_WM_4_A",
            "PLANE_WM_5_A",
            "PLANE_WM_6_A",
            "PLANE_WM_7_A",
            // Phase 4.3, the `noarm` half.
            "PLANE_STRIDE_A",
            "PLANE_POS_A",
            "PLANE_SIZE_A",
            // Phase 4.3, the `arm` half, `PLANE_CTL` immediately before the
            // commit.
            "PLANE_OFFSET_A",
            "PLANE_COLOR_CTL_A",
            "PLANE_CTL_A",
            "PLANE_SURF_A",
        ]
    );
    assert_eq!(programmed.state.writes.len(), names.len());
}

/// The write list is complete before the first write happens, which is the
/// property that lets a caller log the whole program first.
#[test]
fn computing_the_write_list_writes_nothing() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();

    let planned = plan.writes(0);
    assert_eq!(
        planned.len(),
        24,
        "one write per register the sequence owns"
    );
    assert_eq!(
        planned.last().map(|write| write.register),
        Some(Pipe::A.plane_surf()),
        "the last planned write is the commit"
    );
    assert!(
        regs.writes().is_empty(),
        "planning a write must not perform one"
    );

    // And `program` performs exactly that list, in that order, with those
    // values.
    program(&regs, &plan).unwrap();
    let performed: Vec<(&'static str, u32)> = regs.writes();
    let expected: Vec<(&'static str, u32)> = planned
        .iter()
        .map(|write| (write.register.name(), write.value))
        .collect();
    assert_eq!(performed, expected);
}

/// `PLANE_SURF` is written after the watermarks have their enable bit, which is
/// the black-screen-with-correct-timings failure section 7.1 names.
///
/// The assertion is on the write log, not on the return value: the failure this
/// prevents is not an error, it is a plane that reads nothing while every
/// register reads back as written.
#[test]
fn the_plane_is_armed_only_after_the_watermark_enable_bit() {
    let programmed = Programmed::new();
    let writes = programmed.writes();

    let ddb = programmed.index_of(Pipe::A.plane_buf_cfg());
    let watermark = programmed.index_of(Pipe::A.plane_wm(0));
    let surface = programmed.index_of(Pipe::A.plane_surf());

    assert!(
        writes[watermark].1 & PLANE_WM_EN != 0,
        "level 0 must have PLANE_WM_EN set, or the plane reads nothing (section 7.1)"
    );
    assert_eq!(
        writes[watermark].1 & PLANE_WM_BLOCKS_MAX,
        0xfff,
        "level 0's block count is the allocation, capped at the field's maximum"
    );
    assert_eq!(
        writes[watermark].1 >> PLANE_WM_LINES_SHIFT & 0x1fff,
        PLANE_WM_LINES_MAX
    );
    assert!(
        ddb < watermark,
        "the DDB allocation precedes the watermarks"
    );
    assert!(
        watermark < surface,
        "PLANE_SURF arms everything, so it comes after the watermarks"
    );
}

/// A refused watermark write stops the sequence with the plane unarmed.
///
/// This is the ordered-sequence property as a failure case: not "the error was
/// reported" but "no surface address was ever written", because a `PLANE_SURF`
/// written after a failed watermark leaves a plane that is committed and reads
/// nothing.
#[test]
fn a_refused_watermark_write_leaves_the_plane_unarmed() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    regs.refuse(Pipe::A.plane_wm(0));

    let error = program(&regs, &plan).expect_err("the write is refused");
    assert_eq!(
        error,
        PipeError::WriteRefused {
            register: "PLANE_WM_0_A"
        }
    );
    let names: Vec<&'static str> = regs.writes().iter().map(|(name, _)| *name).collect();
    assert!(
        !names.contains(&"PLANE_SURF_A"),
        "the plane was armed despite the watermarks failing: {names:?}"
    );
    // Everything before the failure did happen, and nothing after it did.
    assert_eq!(names.last(), Some(&"PLANE_BUF_CFG_A"));
    assert!(error.describe().contains("PLANE_WM_0_A"));
    assert!(error.describe().contains("not armed"));
}

/// The same property one register later: `PLANE_CTL` is the write immediately
/// before the commit, and losing it must not commit.
#[test]
fn a_refused_plane_control_write_leaves_the_plane_unarmed() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    regs.refuse(Pipe::A.plane_ctl());

    let error = program(&regs, &plan).expect_err("the write is refused");
    assert_eq!(
        error,
        PipeError::WriteRefused {
            register: "PLANE_CTL_A"
        }
    );
    let names: Vec<&'static str> = regs.writes().iter().map(|(name, _)| *name).collect();
    assert!(!names.contains(&"PLANE_SURF_A"));
    assert_eq!(names.last(), Some(&"PLANE_COLOR_CTL_A"));
}

/// A register the mock cannot read is a named error, not a zero.
#[test]
fn an_unreadable_pipe_misc_is_a_named_error() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    regs.hide(Pipe::A.misc());

    let error = program(&regs, &plan).expect_err("the read fails");
    assert_eq!(
        error,
        PipeError::Unreadable {
            register: "PIPE_MISC_A"
        }
    );
    assert!(
        regs.writes().is_empty(),
        "nothing may be written before PIPE_MISC has been read"
    );
}

// ---------------------------------------------------------------------------
// The values
// ---------------------------------------------------------------------------

/// The timing registers are written with exactly what `timing.rs` produced,
/// for a published mode.
///
/// The first assertion is the reference's own arithmetic for VIC 16 (§6.1), so
/// a reader can check it by eye; the second is the property that matters, that
/// this module adds nothing to those seven values and takes nothing away.
#[test]
fn the_timing_registers_are_exactly_what_timing_produced() {
    let programmed = Programmed::new();
    let writes = programmed.writes();

    assert_eq!(
        [
            writes[1].1,
            writes[2].1,
            writes[3].1,
            writes[4].1,
            writes[5].1,
            writes[6].1,
            writes[7].1,
        ],
        [
            0x0897_077f, // HTOTAL:  (2200-1) << 16 | (1920-1)
            0x0897_077f, // HBLANK:  blanking runs to the end of the line
            0x0803_07d7, // HSYNC:   (2052-1) << 16 | (2008-1)
            0x0464_0437, // VTOTAL:  (1125-1) << 16 | (1080-1)
            0x0464_0437, // VBLANK
            0x0440_043b, // VSYNC:   (1089-1) << 16 | (1084-1)
            0x077f_0437, // PIPESRC: (1920-1) << 16 | (1080-1)
        ]
    );

    for (index, (register, value)) in programmed.plan.timings.in_write_order().iter().enumerate() {
        let expected = Pipe::A.timing(*register);
        let (name, written) = writes[index + 1];
        assert_eq!(name, expected.name(), "{register:?}");
        assert_eq!(written, *value, "{register:?} was not written verbatim");
    }
}

/// `PLANE_SIZE`'s halves are `PIPESRC`'s the other way round, and both are
/// counts minus one -- which is the whole point of taking them from
/// `timing.rs` rather than subtracting here.
#[test]
fn the_plane_size_is_the_source_size_with_its_halves_swapped() {
    let programmed = Programmed::new();
    assert_eq!(
        programmed.plan.plane.size, 0x0437_077f,
        "height-1 | width-1"
    );
    assert_eq!(
        timing::unpack(programmed.plan.plane.size),
        (1080, 1920),
        "PLANE_SIZE is [31:16] height-1, [15:0] width-1 (section 5.4)"
    );
    assert_eq!(
        timing::unpack(programmed.plan.timings.pipesrc()),
        (1920, 1080),
        "PIPESRC is [31:16] width-1, [15:0] height-1 (section 5.2)"
    );
    assert_eq!(
        programmed.plan.plane.size,
        programmed.plan.timings.pipesrc().rotate_left(16)
    );
    assert_eq!(
        programmed.regs.read(Pipe::A.plane_size()),
        Some(programmed.plan.plane.size)
    );
}

/// Every number phase 4 writes, for the mode section 11 phase 3.1 prefers.
#[test]
fn the_published_mode_program_matches_the_reference_numbers() {
    let programmed = Programmed::new();
    let writes = programmed.writes();
    let value_of = |register: Register| {
        let index = programmed.index_of(register);
        writes[index].1
    };

    // Phase 4.1: section 11 step 4.1's literal value.
    assert_eq!(
        value_of(Pipe::A.plane_buf_cfg()),
        0x0fff_0000,
        "((4096-1) << 16) | 0"
    );
    assert_eq!(programmed.plan.ddb.start(), 0);
    assert_eq!(programmed.plan.ddb.end(), DDB_BLOCKS);
    assert_eq!(programmed.plan.ddb.blocks(), 4096);

    // Phase 4.2: EN | BLOCKS(4095) | LINES(31).
    assert_eq!(value_of(Pipe::A.plane_wm(0)), 0x8007_cfff);
    for level in 1..PLANE_WM_LEVELS {
        assert_eq!(
            value_of(Pipe::A.plane_wm(level)),
            0,
            "level {level} must be written disabled, not left at reset (section 7.3)"
        );
    }

    // Phase 4.3.
    assert_eq!(value_of(Pipe::A.plane_stride()), 120, "7680 bytes / 64");
    assert_eq!(value_of(Pipe::A.plane_pos()), 0);
    assert_eq!(value_of(Pipe::A.plane_offset()), 0);
    assert_eq!(
        value_of(Pipe::A.plane_color_ctl()),
        PLANE_COLOR_CTL_ALPHA_DISABLE
    );
    assert_eq!(
        value_of(Pipe::A.plane_ctl()),
        0x8400_0000,
        "ENABLE[31] | FORMAT_XRGB_8888 (4 << 24) | TILED_LINEAR (0)"
    );
    assert_eq!(value_of(Pipe::A.plane_surf()), 0x0100_0000);

    // The plane is the whole mode: no scaling, so the plane's size and the
    // pipe's source size describe the same rectangle in opposite half orders.
    assert_eq!(
        timing::unpack(value_of(Pipe::A.plane_size())),
        (
            u32::from(programmed.plan.mode.vdisplay),
            u32::from(programmed.plan.mode.hdisplay)
        )
    );
}

/// Every watermark level is written exactly once, so nothing is left at a reset
/// value that section 7.3 says cannot work.
#[test]
fn every_watermark_level_is_written_once() {
    let programmed = Programmed::new();
    for level in 0..PLANE_WM_LEVELS {
        assert_eq!(
            programmed.regs.write_count(Pipe::A.plane_wm(level)),
            1,
            "level {level}"
        );
    }
    // And `PLANE_SURF` is written exactly once: the commit is not repeated.
    assert_eq!(programmed.regs.write_count(Pipe::A.plane_surf()), 1);
}

/// The stride is written in 64-byte units, which is the one place this module
/// departs from section 5.4's "stride in bytes".
///
/// 7680 would not fit in twelve bits, and 120 does: the mode section 11 phase
/// 3.1 prefers could not be programmed at all if the field were in bytes.
#[test]
fn the_stride_is_written_in_sixty_four_byte_units() {
    let programmed = Programmed::new();
    assert_eq!(programmed.plan.plane.stride_bytes, 7680);
    assert_eq!(programmed.plan.plane.stride, 120);
    assert!(programmed.plan.plane.stride <= PLANE_STRIDE_MAX);
    assert_eq!(
        programmed.regs.read(Pipe::A.plane_stride()),
        Some(120),
        "PLANE_STRIDE holds units, not bytes"
    );

    // A second, smaller mode: 640x480 at 32 bpp is a 2560-byte stride, 40
    // units.
    let small = Programmed::on(
        Pipe::A,
        PlaneSurface {
            ggtt_address: 0x0100_0000,
            stride_bytes: 2560,
        },
        &dmt_0x04(),
    );
    assert_eq!(small.plan.plane.stride, 40);
}

/// `PIPE_MISC` is a read-modify-write of the bits this bring-up owns, so a
/// firmware value the reference does not mention survives.
///
/// Two of those bits are real: `HDR_MODE_PRECISION[23]` (§5.2) and
/// `PIXEL_ROUNDING_TRUNC[8]`, which `[I915]`'s `bdw_set_pipe_misc` sets for
/// display version 12 and later (`display/intel_display.c:3289-3290`) and the
/// reference never mentions.  Writing the whole register would clear both.
#[test]
fn pipe_misc_keeps_the_bits_this_bring_up_does_not_own() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    // What firmware might have left: YUV420 full blend, HDR precision, pixel
    // rounding truncation, 10 bpc and temporal dithering.
    let firmware = (1 << 27) | (1 << 23) | (1 << 8) | (1 << 5) | (1 << 4) | (3 << 2);
    regs.set(Pipe::A.misc(), firmware);

    let state = program(&regs, &plan).unwrap();
    let after = regs.read(Pipe::A.misc()).unwrap();

    assert_eq!(
        after & !PIPE_MISC_OWNED_MASK,
        (1 << 27) | (1 << 23) | (1 << 8),
        "the bits this bring-up does not own must survive"
    );
    assert_eq!(after & PIPE_MISC_BPC_MASK, PIPE_MISC_BPC_8, "8 bpc");
    assert_eq!(after & PIPE_MISC_DITHER_ENABLE, 0, "dithering off");
    assert_eq!(after & PIPE_MISC_DITHER_TYPE_MASK, 0);
    assert_eq!(state.pipe_misc_before, firmware);
    assert_eq!(state.pipe_misc_after, after);
    assert_eq!(
        regs.writes()[0].0,
        "PIPE_MISC_A",
        "PIPE_MISC is the first write, so PLANE_SURF stays last"
    );
}

/// `PIPE_MISC`'s depth is set for the mode's 8-bit components and for nothing
/// else -- there is no depth conversion to dither.
#[test]
fn pipe_misc_is_eight_bpc_with_dithering_off() {
    let programmed = Programmed::new();
    assert_eq!(programmed.plan.pipe_misc, PIPE_MISC_BPC_8);
    assert_eq!(
        programmed.regs.read(Pipe::A.misc()),
        Some(PIPE_MISC_BPC_8),
        "a mock that started at zero keeps only the program's value"
    );
}

// ---------------------------------------------------------------------------
// The register mapping
// ---------------------------------------------------------------------------

/// Every pipe's registers are the ones the reference's stride rules produce.
///
/// The table in `regs/pipe.rs` already asserts its own offsets; this asserts
/// the *mapping* -- that pipe D's program touches pipe D -- because a
/// transcription error here would program pipe A three times and look, on the
/// machine, exactly like a monitor that never sees a signal.
#[test]
fn each_pipe_has_its_own_registers() {
    for pipe in Pipe::ALL {
        let offset = pipe.index() * 0x1000;
        assert_eq!(pipe.misc().offset(), 0x7_0030 + offset, "PIPE_MISC({pipe})");
        assert_eq!(
            pipe.pipedsl().offset(),
            0x7_0000 + offset,
            "PIPEDSL({pipe})"
        );
        assert_eq!(
            pipe.pipestat().offset(),
            0x7_0024 + offset,
            "PIPESTAT({pipe}), which section 10.4 gives directly"
        );
        assert_eq!(
            pipe.plane_ctl().offset(),
            0x7_0180 + offset,
            "PLANE_CTL({pipe},1)"
        );
        assert_eq!(pipe.plane_stride().offset(), 0x7_0188 + offset);
        assert_eq!(pipe.plane_pos().offset(), 0x7_018c + offset);
        assert_eq!(pipe.plane_size().offset(), 0x7_0190 + offset);
        assert_eq!(pipe.plane_surf().offset(), 0x7_019c + offset);
        assert_eq!(pipe.plane_offset().offset(), 0x7_01a4 + offset);
        assert_eq!(pipe.plane_surflive().offset(), 0x7_01ac + offset);
        assert_eq!(pipe.plane_color_ctl().offset(), 0x7_01cc + offset);
        assert_eq!(
            pipe.plane_buf_cfg().offset(),
            0x7_027c + offset,
            "PLANE_BUF_CFG({pipe},1)"
        );
        for level in 0..PLANE_WM_LEVELS {
            assert_eq!(
                pipe.plane_wm(level).offset(),
                0x7_0240 + level as u32 * 4 + offset,
                "PLANE_WM({pipe},1,{level})"
            );
        }
        // The transcoder timing registers live in the other block, at
        // section 5.3's base.
        assert_eq!(
            pipe.timing(TimingRegister::Htotal).offset(),
            0x6_0000 + offset
        );
        assert_eq!(
            pipe.timing(TimingRegister::Hblank).offset(),
            0x6_0004 + offset
        );
        assert_eq!(
            pipe.timing(TimingRegister::Hsync).offset(),
            0x6_0008 + offset
        );
        assert_eq!(
            pipe.timing(TimingRegister::Vtotal).offset(),
            0x6_000c + offset
        );
        assert_eq!(
            pipe.timing(TimingRegister::Vblank).offset(),
            0x6_0010 + offset
        );
        assert_eq!(
            pipe.timing(TimingRegister::Vsync).offset(),
            0x6_0014 + offset
        );
        assert_eq!(
            pipe.timing(TimingRegister::Pipesrc).offset(),
            0x6_001c + offset
        );
    }
}

/// A program for pipe B writes pipe B's registers and no others.
#[test]
fn a_pipe_b_program_touches_only_pipe_b_registers() {
    let programmed = Programmed::on(Pipe::B, SURFACE, &vic16());
    for (name, _) in programmed.writes() {
        assert!(
            name.ends_with("_B") || name.contains("(B)"),
            "{name} is not a pipe B register"
        );
    }
    assert_eq!(
        programmed.plan.timings.htotal(),
        programmed
            .regs
            .read(Pipe::B.timing(TimingRegister::Htotal))
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Every surface the reference's phase 3.2 rules out is refused by name, and
/// nothing is written when it is.
#[test]
fn a_surface_that_cannot_be_scanned_out_is_refused() {
    let cases: [(PlaneSurface, PipeError); 6] = [
        (
            PlaneSurface {
                ggtt_address: 0,
                stride_bytes: 7680,
            },
            PipeError::SurfaceAddressZero,
        ),
        (
            PlaneSurface {
                ggtt_address: 0x0100_0800,
                stride_bytes: 7680,
            },
            PipeError::SurfaceMisaligned {
                address: 0x0100_0800,
            },
        ),
        (
            PlaneSurface {
                ggtt_address: 1 << 32,
                stride_bytes: 7680,
            },
            PipeError::SurfaceAboveAddressWindow { address: 1 << 32 },
        ),
        (
            PlaneSurface {
                ggtt_address: 0x0100_0000,
                stride_bytes: 0,
            },
            PipeError::StrideZero,
        ),
        (
            PlaneSurface {
                ggtt_address: 0x0100_0000,
                stride_bytes: 100,
            },
            PipeError::StrideNotAMultipleOf64 { stride_bytes: 100 },
        ),
        (
            // A 7680-byte pitch is the one a 1920-wide XRGB8888 surface has,
            // and it is the value the coordinator's finding and these tests
            // both turn on: one byte wider is not expressible, so it is
            // refused rather than truncated to 120.
            PlaneSurface {
                ggtt_address: 0x0100_0000,
                stride_bytes: 7681,
            },
            PipeError::StrideNotAMultipleOf64 { stride_bytes: 7681 },
        ),
    ];

    for (surface, expected) in cases {
        assert_eq!(
            compute(Pipe::A, &vic16(), surface),
            Err(expected),
            "{surface:?}"
        );
    }

    // A stride that is a multiple of 64 but too large for the twelve-bit field:
    // 4096 units is one past the maximum.
    let too_wide = PlaneSurface {
        ggtt_address: 0x0100_0000,
        stride_bytes: 4096 * PLANE_STRIDE_UNIT_BYTES,
    };
    assert_eq!(
        compute(Pipe::A, &vic16(), too_wide),
        Err(PipeError::StrideTooWide {
            stride_bytes: 262_144,
            units: 4096,
        })
    );
    // And the largest one that does fit is accepted.
    assert!(
        compute(
            Pipe::A,
            &vic16(),
            PlaneSurface {
                ggtt_address: 0x0100_0000,
                stride_bytes: PLANE_STRIDE_MAX * PLANE_STRIDE_UNIT_BYTES,
            }
        )
        .is_ok()
    );
}

/// `timing.rs`'s refusals reach the caller through this module, and the message
/// still says which of the two it was.
#[test]
fn a_mode_timing_rs_refuses_is_refused_here_too() {
    let interlaced = CTA_VIC_TIMINGS
        .iter()
        .find(|entry| entry.vic == 5)
        .expect("VIC 5 is 1080i")
        .mode;
    assert!(interlaced.is_interlaced());
    assert_eq!(
        compute(Pipe::A, &interlaced, SURFACE),
        Err(PipeError::Timing(timing::TimingError::InterlaceNotSourced))
    );
    assert!(
        PipeError::Timing(timing::TimingError::InterlaceNotSourced)
            .describe()
            .contains("interlaced"),
        "the reason has to survive the wrapping"
    );

    let mut malformed = dmt_0x04();
    malformed.htotal = 0;
    assert_eq!(
        compute(Pipe::A, &malformed, SURFACE),
        Err(PipeError::Timing(timing::TimingError::Malformed))
    );
}

/// Every error describes itself well enough to read from a boot log on a
/// machine whose only output is the screen.
#[test]
fn errors_describe_themselves() {
    let errors = [
        PipeError::Unreadable {
            register: "PIPEDSL_A",
        },
        PipeError::WriteRefused {
            register: "PLANE_CTL_A",
        },
        PipeError::Timing(timing::TimingError::Malformed),
        PipeError::StrideZero,
        PipeError::StrideNotAMultipleOf64 { stride_bytes: 100 },
        PipeError::StrideTooWide {
            stride_bytes: 262_144,
            units: 4096,
        },
        PipeError::SurfaceAddressZero,
        PipeError::SurfaceMisaligned { address: 4 },
        PipeError::SurfaceAboveAddressWindow { address: 1 << 40 },
    ];
    for error in errors {
        let text = error.describe();
        assert!(text.len() > 60, "{error:?} rendered as {text:?}");
        assert!(!text.contains("Err("), "{error:?} rendered as {text:?}");
        assert!(
            !text.contains("Rust"),
            "a boot log line must not be a Rust diagnostic: {text}"
        );
    }
    // Spot-check the two that carry the most important advice.
    assert!(
        PipeError::SurfaceMisaligned { address: 0x800 }
            .describe()
            .contains("SURFLIVE"),
        "a rejected address is one of the two causes step 4.3 gives for SURFLIVE reading zero"
    );
    assert!(
        PipeError::StrideNotAMultipleOf64 { stride_bytes: 100 }
            .describe()
            .contains("64"),
    );
}

// ---------------------------------------------------------------------------
// Phase 6: the read-backs
// ---------------------------------------------------------------------------

/// The success path: the pipe counts lines, the plane is armed at the address
/// that was written, and the FIFO never underran.
#[test]
fn a_scanning_pipe_an_armed_plane_and_no_underrun_all_pass() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    // A scanline counter that advances on every read, and a live surface
    // address that is the one the plane was given.
    let scanline = Cell::new(0u32);
    regs.on_read(Pipe::A.pipedsl(), move |_| {
        scanline.set(scanline.get() + 1);
        scanline.get() * 100
    });
    regs.set(Pipe::A.plane_surflive(), plan.plane.surf);

    let clock = FakeClock::new();
    let checks = prove(&regs, &plan, &clock);

    assert!(checks.ok(), "{}", checks.render());
    assert_eq!(
        checks.scanline,
        ScanlineCheck::Scanning {
            samples: [100, 200, 300, 400]
        }
    );
    assert_eq!(
        checks.surface,
        SurfaceCheck::Armed {
            live: plan.plane.surf,
            wrote: plan.plane.surf
        }
    );
    assert_eq!(checks.underrun, UnderrunCheck::Clear { stat: 0 });
    // Section 11 step 6.1's "a few milliseconds apart": the samples cost three
    // intervals of the reference's unit.
    assert!(
        clock.now_micros() >= 3 * SCANLINE_INTERVAL_MICROS,
        "the scanline samples were {} us apart in total",
        clock.now_micros()
    );
    assert!(
        regs.writes().is_empty(),
        "proving a pipe must not write to it"
    );
}

/// A `PIPEDSL` that does not change is a named error, and the other two checks
/// still run.
#[test]
fn a_pipe_that_is_not_scanning_is_a_named_failure() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    // PIPEDSL reads zero forever, but the plane is armed and nothing underran:
    // the point is that all three verdicts are still produced.
    regs.set(Pipe::A.plane_surflive(), plan.plane.surf);

    let checks = prove(&regs, &plan, &FakeClock::new());
    assert!(!checks.ok());
    assert_eq!(
        checks.scanline,
        ScanlineCheck::NotScanning {
            samples: [0, 0, 0, 0]
        }
    );
    assert!(checks.surface.is_ok(), "the surface check still ran");
    assert!(checks.underrun.is_ok(), "the underrun check still ran");
    let text = checks.scanline.describe();
    for expected in ["PIPEDSL", "not scanning", "TRANSCONF", "TRANS_CLK_SEL"] {
        assert!(text.contains(expected), "{expected} missing from {text}");
    }
}

/// A single `PIPEDSL` value that repeats by coincidence does not look like a
/// stopped pipe, because the samples are compared pairwise.
#[test]
fn a_repeated_scanline_sample_is_not_a_stopped_pipe() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    let reads = Cell::new(0u32);
    // First and last samples equal, the middle two different: a two-read check
    // would call this stopped.
    regs.on_read(Pipe::A.pipedsl(), move |_| {
        reads.set(reads.get() + 1);
        match reads.get() {
            1 | 4 => 500,
            _ => 900,
        }
    });

    let checks = prove(&regs, &plan, &FakeClock::new());
    assert_eq!(
        checks.scanline,
        ScanlineCheck::Scanning {
            samples: [500, 900, 900, 500]
        }
    );
}

/// An underrun is a named result that points at the watermarks and the DDB,
/// with the values this program wrote.
#[test]
fn an_underrun_names_the_watermarks_and_the_ddb() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    let scanline = Cell::new(0u32);
    regs.on_read(Pipe::A.pipedsl(), move |_| {
        scanline.set(scanline.get() + 1);
        scanline.get()
    });
    regs.set(Pipe::A.plane_surflive(), plan.plane.surf);
    // Bit 31 plus an unrelated vblank status bit, so the verdict cannot be
    // "the whole register was non-zero".
    regs.set(Pipe::A.pipestat(), PIPE_FIFO_UNDERRUN_STATUS | 0x2);

    let checks = prove(&regs, &plan, &FakeClock::new());
    assert!(!checks.ok());
    assert_eq!(
        checks.underrun,
        UnderrunCheck::Underrun {
            stat: PIPE_FIFO_UNDERRUN_STATUS | 0x2,
            watermark: 0x8007_cfff,
            ddb: 0x0fff_0000,
        }
    );
    let text = checks.underrun.describe();
    for expected in ["bit 31", "PLANE_WM", "PLANE_BUF_CFG", "4.2", "7.1"] {
        assert!(text.contains(expected), "{expected} missing from {text}");
    }
    // The other two checks are unaffected and still reported.
    assert!(checks.scanline.is_ok());
    assert!(checks.surface.is_ok());
}

/// A plane that did not arm is a named failure, and the two shapes of it -- a
/// zero and somebody else's address -- say different things.
#[test]
fn a_plane_that_did_not_arm_is_a_named_failure() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();

    // Never armed: SURFLIVE reads zero.
    let regs = MockRegisters::new();
    let checks = prove(&regs, &plan, &FakeClock::new());
    assert_eq!(
        checks.surface,
        SurfaceCheck::NotArmed {
            live: 0,
            wrote: plan.plane.surf
        }
    );
    let text = checks.surface.describe();
    for expected in ["never armed", "PLANE_CTL", "alignment", "GGTT"] {
        assert!(text.contains(expected), "{expected} missing from {text}");
    }

    // Armed at something else: the firmware's surface, most likely.
    let regs = MockRegisters::new();
    regs.set(Pipe::A.plane_surflive(), 0x0200_0000);
    let checks = prove(&regs, &plan, &FakeClock::new());
    assert_eq!(
        checks.surface,
        SurfaceCheck::NotArmed {
            live: 0x0200_0000,
            wrote: plan.plane.surf
        }
    );
    assert!(checks.surface.describe().contains("firmware"));
}

/// The comparison ignores bits outside the address field, so a decrypt flag in
/// the read-back is not reported as a wrong address.
#[test]
fn the_live_address_is_compared_on_the_address_field_alone() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    regs.set(
        Pipe::A.plane_surflive(),
        plan.plane.surf | (1 << 2), // PLANE_SURF_DECRYPT
    );

    let checks = prove(&regs, &plan, &FakeClock::new());
    assert!(checks.surface.is_ok(), "{}", checks.surface.describe());
}

/// A register outside the mapped window is an `Unreadable` verdict per check,
/// not a zero value and not a panic.
#[test]
fn a_register_outside_the_window_makes_its_check_unreadable() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();

    let regs = MockRegisters::new();
    regs.hide(Pipe::A.pipedsl());
    let checks = prove(&regs, &plan, &FakeClock::new());
    assert_eq!(
        checks.scanline,
        ScanlineCheck::Unreadable {
            register: "PIPEDSL_A"
        }
    );
    assert!(!checks.ok());

    let regs = MockRegisters::new();
    regs.hide(Pipe::A.plane_surflive());
    regs.hide(Pipe::A.pipestat());
    let checks = prove(&regs, &plan, &FakeClock::new());
    assert_eq!(
        checks.surface,
        SurfaceCheck::Unreadable {
            register: "PLANE_SURFLIVE_A"
        }
    );
    assert_eq!(
        checks.underrun,
        UnderrunCheck::Unreadable {
            register: "PIPESTAT_A"
        }
    );
    assert!(!checks.surface.describe().is_empty());
    assert!(!checks.underrun.describe().is_empty());
}

/// A timer that never advances must not hang the check.
///
/// `PollTimer`'s one contract is that `pause` advances `now_micros`; a test
/// double or a platform clock that broke it would otherwise spin in the boot
/// path with the screen off.  The pause budget bounds it.
#[test]
fn a_clock_that_never_advances_does_not_hang_the_check() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    let checks = prove(&regs, &plan, &StuckClock);
    assert_eq!(
        checks.scanline,
        ScanlineCheck::NotScanning {
            samples: [0, 0, 0, 0]
        }
    );
}

/// The whole phase-6 report renders without writing anything.
#[test]
fn the_phase_six_report_renders_every_check() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    regs.set(Pipe::A.pipestat(), PIPE_FIFO_UNDERRUN_STATUS);
    let checks = prove(&regs, &plan, &FakeClock::new());
    let text = checks.render();
    for expected in ["pipe A phase 6.1", "phase 6.2", "phase 6.4", "underran"] {
        assert!(text.contains(expected), "{expected} missing from {text}");
    }
    assert_eq!(text.lines().count(), 3);
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// The program renders as a log before it is written, and the rendering does
/// not touch the device.
#[test]
fn the_whole_program_can_be_logged_before_the_first_write() {
    let plan = compute(Pipe::A, &vic16(), SURFACE).unwrap();
    let regs = MockRegisters::new();
    let text = plan.render(0);

    for expected in [
        "pipe A",
        "reference section 11 phases 3.4 and 4",
        "PLANE_BUF_CFG = 0x0fff0000",
        "watermark level 0: 0x8007cfff",
        "levels 1..7 disabled",
        "PLANE_SURF_A <- 0x01000000",
        "TRANS_HTOTAL(A) <- 0x0897077f",
        "PLANE_MISC", // deliberately absent; asserted below
    ] {
        if expected == "PLANE_MISC" {
            assert!(!text.contains(expected), "there is no PLANE_MISC register");
            continue;
        }
        assert!(text.contains(expected), "{expected} missing from:\n{text}");
    }
    assert!(
        regs.writes().is_empty(),
        "rendering a program must not write it"
    );

    // The state's own rendering has one line per write plus the two headers.
    let state = program(&regs, &plan).unwrap();
    let rendered = state.render();
    assert_eq!(rendered.lines().count(), state.writes.len() + 2);
    assert!(rendered.contains("24 writes"));
}

/// The mock register file is only useful if it is the thing being driven, so
/// this pins the two facts the sequence relies on: an unwritten register reads
/// zero, and `PIPE_MISC`'s read before the write is what the value is built
/// from.
#[test]
fn the_mock_reads_zero_before_it_is_written() {
    let regs = MockRegisters::new();
    assert_eq!(regs.read(Pipe::A.plane_surf()), Some(0));
    assert_eq!(regs.read(Pipe::A.pipestat()), Some(0));
    assert!(regs.writes().is_empty());
    assert_eq!(regs.write_count(Pipe::A.plane_surf()), 0);
}
