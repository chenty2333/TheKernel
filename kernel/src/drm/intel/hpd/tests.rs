//! Hotplug detection, driven through an ordinary buffer standing in for the
//! aperture.
//!
//! The tests here are about arithmetic and about restraint: the bit positions
//! are checked against the tables in the reference document and in `[I915]`,
//! and the tests that matter most are the ones that assert what this module
//! *does not* write -- the latched detect field, the pulse filter, and the
//! board-inversion bit that i915 applies only on DG1.

use alloc::vec;

use super::*;
use crate::{
    drm::intel::regs::{PROBE_WINDOW, Register, RegisterWindow},
    test_support::scheduler_test_context,
};

/// A buffer standing in for the aperture, with a window over it.
struct Scratch {
    words: vec::Vec<u32>,
    window_len: usize,
}

impl Scratch {
    fn new() -> Self {
        Self {
            words: vec![0; PROBE_WINDOW / 4],
            window_len: PROBE_WINDOW,
        }
    }

    fn window(&mut self) -> RegisterWindow {
        // SAFETY: `words` is a live, 4-byte aligned buffer of exactly
        // `PROBE_WINDOW` bytes, and `window_len` never exceeds it.
        unsafe { RegisterWindow::from_mapped(self.words.as_mut_ptr() as usize, self.window_len) }
    }

    fn set(&mut self, register: Register, value: u32) {
        self.words[register.offset() as usize / 4] = value;
    }

    fn get(&self, register: Register) -> u32 {
        self.words[register.offset() as usize / 4]
    }
}

#[test]
fn the_hotplug_bit_positions_match_the_sources() {
    // `[I915]` `i915_reg.h:3079-3085` (`SHOTPLUG_CTL_DDI_HPD_ENABLE` and the
    // detect field), `:3001` (`SDE_DDI_HOTPLUG_ICP`), `:3367-3370`
    // (`INVERT_DDIA_HPD`..`INVERT_DDID_HPD`).  Reference §9.5 gives the
    // four-bit-per-DDI layout and §10.5 the `SDEISR` bits.
    let table: [(Ddi, u32, u32, u32, u32, u32); 4] = [
        (Ddi::A, 0, 0x0000_0008, 0x0000_0003, 1 << 16, 1 << 15),
        (Ddi::B, 1, 0x0000_0080, 0x0000_0030, 1 << 17, 1 << 16),
        (Ddi::C, 2, 0x0000_0800, 0x0000_0300, 1 << 18, 1 << 17),
        (Ddi::D, 3, 0x0000_8000, 0x0000_3000, 1 << 19, 1 << 18),
    ];
    for (ddi, index, enable, detect, live, invert) in table {
        assert_eq!(ddi.index(), index, "{ddi}");
        assert_eq!(ddi.enable_bit(), enable, "{ddi}");
        assert_eq!(ddi.detect_field(), detect, "{ddi}");
        // The four bits of one DDI: output data, then the two-bit detect
        // field, then enable at the top ([I915] `i915_reg.h:3079-3085`).
        assert_eq!(ddi.output_data_bit(), enable >> 1, "{ddi}");
        assert_eq!(ddi.live_bit(), live, "{ddi}");
        assert_eq!(ddi.invert_bit(), invert, "{ddi}");
        // The five fields of one DDI never overlap: a shared bit would mean one
        // write changing two things.
        for (left, right) in [
            (enable, detect),
            (enable, live),
            (enable, invert),
            (detect, live),
            (detect, invert),
            (live, invert),
        ] {
            assert_eq!(left & right, 0, "{ddi}: {left:#x} and {right:#x} overlap");
        }
    }
    assert_eq!(Ddi::ALL, [Ddi::A, Ddi::B, Ddi::C, Ddi::D]);
    assert_eq!(format!("{}", Ddi::C), "DDI C");
    // DDI D has a hotplug index but no DDC pin in the ICP table, which is why
    // hotplug and GMBUS pins are not the same type.
    assert_eq!(Ddi::D.pin(), None);
}

#[test]
fn enabling_hotplug_touches_only_this_ddis_enable_bit() {
    let _guard = scheduler_test_context();
    let mut scratch = Scratch::new();
    // The firmware left DDI B enabled with a latched long detect, DDI C with
    // an output-data bit set, and DDI D disabled with a short-pulse latch.
    let before = Ddi::B.enable_bit()
        | (2 << (Ddi::B.index() * 4))
        | Ddi::C.output_data_bit()
        | (1 << (Ddi::D.index() * 4));
    scratch.set(SHOTPLUG_CTL_DDI, before);
    let window = scratch.window();

    let status = enable_and_read(&window, Ddi::A).expect("DDI A must be enableable");
    assert!(status.enabled);
    assert_eq!(status.control_before, before);
    assert_eq!(status.control_after, before | Ddi::A.enable_bit());
    // Every other DDI's bits are exactly as they were, and DDI A's detect field
    // was not written -- that field is a status latch (see the module
    // documentation), so writing it would destroy evidence.
    assert_eq!(scratch.get(SHOTPLUG_CTL_DDI), before | Ddi::A.enable_bit());
    assert_eq!(
        scratch.get(SHOTPLUG_CTL_DDI) & Ddi::A.detect_field(),
        0,
        "the detect field of the DDI being enabled must not be programmed"
    );
    assert_eq!(
        scratch.get(SHOTPLUG_CTL_DDI) & Ddi::B.enable_bit(),
        Ddi::B.enable_bit(),
        "another DDI's enable must survive"
    );
    assert_eq!(
        scratch.get(SHOTPLUG_CTL_DDI) & Ddi::C.output_data_bit(),
        Ddi::C.output_data_bit()
    );
}

#[test]
fn enabling_every_ddi_in_turn_leaves_them_all_enabled() {
    let _guard = scheduler_test_context();
    let mut scratch = Scratch::new();
    let window = scratch.window();
    for ddi in Ddi::ALL {
        let status = enable_and_read(&window, ddi).expect("every DDI must be enableable");
        assert!(status.enabled, "{ddi}");
    }
    let all = Ddi::ALL.iter().fold(0, |bits, ddi| bits | ddi.enable_bit());
    assert_eq!(scratch.get(SHOTPLUG_CTL_DDI), all);
    // Their detect fields are all still zero, because none of them was written.
    assert_eq!(scratch.get(SHOTPLUG_CTL_DDI) & 0x3333, 0);
}

#[test]
fn the_live_connect_state_is_read_from_sdeisr() {
    let _guard = scheduler_test_context();
    let mut scratch = Scratch::new();
    // A monitor on DDI B, as the live state bit would say.
    scratch.set(SDEISR, Ddi::B.live_bit());
    let window = scratch.window();
    let status = enable_and_read(&window, Ddi::B).expect("DDI B");
    assert!(status.connected);
    assert_eq!(status.interrupt_status, Ddi::B.live_bit());
    assert!(status.describe().contains("a sink is connected"));
    // And nothing else looks connected.
    for ddi in [Ddi::A, Ddi::C, Ddi::D] {
        assert!(!enable_and_read(&window, ddi).unwrap().connected, "{ddi}");
    }
    assert!(live_state(&window, Ddi::B).unwrap());
    assert!(!live_state(&window, Ddi::A).unwrap());
}

#[test]
fn a_polarity_bit_is_reported_and_explains_a_status_that_never_changes() {
    let _guard = scheduler_test_context();
    let mut scratch = Scratch::new();
    scratch.set(SOUTH_CHICKEN1, Ddi::A.invert_bit());
    scratch.set(SDEISR, Ddi::A.live_bit());
    let window = scratch.window();
    let status = enable_and_read(&window, Ddi::A).expect("DDI A");
    assert!(status.polarity_inverted);
    assert!(polarity_inverted(&window, Ddi::A).unwrap());
    assert!(!polarity_inverted(&window, Ddi::B).unwrap());
    // With the board inverting hotplug, the bit being set means the opposite of
    // what it means elsewhere, and the report says both readings instead of
    // picking one: no source read for this workstream establishes which boards
    // invert (reference §9.5 and item 4 of §13.2).
    let text = status.describe();
    assert!(text.contains("board-inversion bit is also set"), "{text}");
    assert!(text.contains("means nothing is connected"), "{text}");
}

#[test]
fn the_latched_detect_field_is_reported_with_the_meaning_of_its_value() {
    let _guard = scheduler_test_context();
    for (field, name) in [
        (0u32, "no detect"),
        (1, "short pulse"),
        (2, "long pulse (a connect)"),
        (3, "short and long"),
    ] {
        let mut scratch = Scratch::new();
        scratch.set(SHOTPLUG_CTL_DDI, field << (Ddi::A.index() * 4));
        let window = scratch.window();
        let status = enable_and_read(&window, Ddi::A).expect("DDI A");
        assert_eq!(status.detect_field_name(), name, "field {field}");
    }
}

#[test]
fn the_pulse_filter_is_read_and_never_written() {
    let _guard = scheduler_test_context();
    let mut scratch = Scratch::new();
    // The two values the reference gives (§9.5): 500 us adjusted, and 250 us.
    scratch.set(SHPD_FILTER_CNT, 0x0001_d9);
    let window = scratch.window();
    let status = enable_and_read(&window, Ddi::A).expect("DDI A");
    assert_eq!(status.filter, Some(0x0001_d9));
    assert!(status.describe().contains("SHPD_FILTER_CNT"));
    // The filter shapes interrupt pulses and this workstream takes no
    // interrupt, so the register is left exactly as it was found.
    assert_eq!(scratch.get(SHPD_FILTER_CNT), 0x0001_d9);
}

#[test]
fn board_inversion_is_never_applied_unless_it_is_asked_for() {
    let _guard = scheduler_test_context();
    let mut scratch = Scratch::new();
    let window = scratch.window();
    // Nothing in enabling detection touches the polarity register: i915 applies
    // the inversion only for DG1 boards (`dg1_hpd_invert`, [I915]
    // `display/intel_hotplug_irq.c:883-904`), and this kernel has no evidence
    // that the target board needs it.
    for ddi in Ddi::ALL {
        enable_and_read(&window, ddi).unwrap();
    }
    assert_eq!(scratch.get(SOUTH_CHICKEN1), 0);
    assert!(!polarity_inverted(&window, Ddi::A).unwrap());

    // Asking for it sets one DDI's bit, and clears it again.
    let after = set_board_inversion(&window, Ddi::A, true).expect("SOUTH_CHICKEN1 is writable");
    assert_eq!(after & Ddi::A.invert_bit(), Ddi::A.invert_bit());
    assert_eq!(after & Ddi::B.invert_bit(), 0, "only the DDI asked for");
    assert!(polarity_inverted(&window, Ddi::A).unwrap());
    let cleared = set_board_inversion(&window, Ddi::A, false).expect("still writable");
    assert_eq!(cleared, 0);
}

#[test]
fn reading_the_live_state_writes_nothing() {
    let _guard = scheduler_test_context();
    let mut scratch = Scratch::new();
    scratch.set(SDEISR, Ddi::C.live_bit());
    scratch.set(SOUTH_CHICKEN1, 0xffff_ffff);
    let before = scratch.words.clone();
    let window = scratch.window();
    assert!(live_state(&window, Ddi::C).unwrap());
    assert!(polarity_inverted(&window, Ddi::C).unwrap());
    assert_eq!(scratch.words, before, "a read must not change a register");
}

#[test]
fn a_window_that_does_not_reach_the_hotplug_block_is_named() {
    let _guard = scheduler_test_context();
    let mut scratch = Scratch::new();
    scratch.window_len = 0x1000;
    let window = scratch.window();
    assert_eq!(
        enable_and_read(&window, Ddi::A),
        Err(HpdError::WindowTooSmall {
            register: "SHOTPLUG_CTL_DDI"
        })
    );
    assert!(
        enable_and_read(&window, Ddi::A)
            .unwrap_err()
            .describe()
            .contains("SHOTPLUG_CTL_DDI")
    );
    // And every status word is absent rather than zero, so a caller cannot
    // mistake "no monitor" for "the register read zero".
    assert!(live_state(&window, Ddi::A).is_err());
    assert!(polarity_inverted(&window, Ddi::A).is_err());
}
