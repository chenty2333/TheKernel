//! Phase 2.1 in front of phases 2.2 and 2.3, on a modelled controller.
//!
//! What these tests establish is the composition rather than the pieces: that
//! the AUX/DDC power wells are requested before the first GMBUS transaction and
//! not after it, that a well which never comes up is recorded and does not stop
//! the probe, and that the pin a monitor answers on becomes a [`Connector`]
//! whose DDI is the one that pin carries.  The bus itself -- the transaction
//! state machine, the EDID checks, the mode layer -- belongs to
//! [`gmbus::tests`] and [`sink::tests`] and is deliberately not re-tested here;
//! this module drives it through the same double those modules use.
//!
//! The one piece of device state that is *this* module's is the well register,
//! so [`Bench`] models it explicitly: a request write sets a well's `STATE` bit
//! for the wells the model has, and never for the ones it does not.
//!
//! Nothing here says anything about an Alder Lake-N part.

use alloc::{format, vec, vec::Vec};
use core::cell::{Cell, RefCell};

use super::*;
use crate::{
    drm::intel::{
        gmbus,
        gmbus::tests::{FakeClock, FakeController},
        probe::BusFacts,
        regs::{GMBUS0, ICL_PWR_WELL_CTL_AUX2, Register},
    },
    test_support::scheduler_test_context,
};

/// A `Bdf` for a display function, for the report lines.
fn bdf() -> Bdf {
    Bdf::new(0, 2, 0)
}

/// An EDID whose preferred timing is 1920x1080@60, the timing reference §11
/// phase 3.1 says to prefer: 148.5 MHz, comfortably inside the HDMI table.
///
/// The 18 bytes are the same standard CEA-861 detailed timing descriptor
/// `sink::tests` builds, in the field layout `drm::modes::edid` parses: pixel
/// clock in 10 kHz units, then the active and blanking fields split across a
/// low byte and a high nibble, the sync offsets and widths, the image size in
/// millimetres, and the flags byte.
fn edid_1080p60() -> [u8; gmbus::EDID_BLOCK_LEN] {
    let mut block = gmbus::tests::valid_edid(0);
    let dtd: [u8; 18] = [
        0x02, 0x3a, // 14850 -> 148.5 MHz
        0x80, 0x18, 0x71, // hactive 1920, hblank 280
        0x38, 0x2d, 0x40, // vactive 1080, vblank 45
        0x58, 0x2c, 0x45, 0x00, // hfront 88, hsync 44, vfront 4, vsync 5
        0xfd, 0x1e, 0x11, // 509 mm x 286 mm
        0x00, 0x00, // no border
        0x1e, // digital separate, positive H and V
    ];
    block[0x36..0x36 + 18].copy_from_slice(&dtd);
    // The checksum is a property of the whole block, so it is recomputed after
    // the descriptor goes in.
    let sum = block[..gmbus::EDID_BLOCK_LEN - 1]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    block[gmbus::EDID_BLOCK_LEN - 1] = 0u8.wrapping_sub(sum);
    block
}

/// The GMBUS controller model, with the AUX/DDC power wells in front of it.
///
/// Everything the bus does is [`FakeController`]'s, including the two rules the
/// mapped window applies; only `ICL_PWR_WELL_CTL_AUX2` is answered from here,
/// because the request/state handshake is what phase 2.1 is.  A well with a
/// requester comes up unless this model says it does not, which is how a dead
/// well and a working one are told apart without either being assumed.
struct Bench {
    controller: FakeController,
    /// `ICL_PWR_WELL_CTL_AUX2` as the model drives it.
    aux: Cell<u32>,
    /// Whether each candidate well's `STATE` bit sets when its request bit is
    /// written, indexed the way [`CANDIDATES`] is: 0 is `AUX_A`, 1 is `AUX_B`.
    comes_up: [bool; 2],
    /// Every write, in order, as `(offset, value)`.  The order is what the
    /// first test is about, so it is recorded here rather than inferred from
    /// the controller's per-register storage.
    writes: RefCell<Vec<(u32, u32)>>,
}

impl Bench {
    /// A controller with a monitor on `pin`, and both wells working.
    fn with_monitor(pin: Pin, block: &[u8; gmbus::EDID_BLOCK_LEN]) -> Self {
        Self::over(FakeController::with_monitor(pin, block))
    }

    /// A controller with nothing behind it, and both wells working.
    fn bare() -> Self {
        Self::over(FakeController::bare())
    }

    fn over(controller: FakeController) -> Self {
        Self {
            controller,
            aux: Cell::new(0),
            comes_up: [true, true],
            writes: RefCell::new(Vec::new()),
        }
    }

    /// The firmware already left this pin pair's well on, under a request bit
    /// this driver does not own -- which is the case §11 phase 2.1 says a
    /// `STATE` bit that is already set cannot distinguish from its own request.
    fn well_already_on(&self, pin: Pin) {
        self.aux.set(self.aux.get() | pin.aux_well().state_bit());
    }

    /// This pin pair's well never reports its `STATE` bit, whatever is written.
    fn well_never_comes_up(&mut self, pin: Pin) {
        let index = pin.aux_well().index() as usize;
        self.comes_up[index] = false;
    }

    /// Every write to the AUX well register, as `(position, value)`, where
    /// `position` is the write's index in the whole ordered log.
    fn aux_writes(&self) -> Vec<(usize, u32)> {
        self.writes
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, (offset, _))| *offset == ICL_PWR_WELL_CTL_AUX2.offset())
            .map(|(position, (_, value))| (position, *value))
            .collect()
    }

    /// The position of the first write to `register`, if it was written.
    fn first_write_to(&self, register: Register) -> Option<usize> {
        self.writes
            .borrow()
            .iter()
            .position(|(offset, _)| *offset == register.offset())
    }

    /// Apply a write to the modelled well register.
    ///
    /// The register is a request/state pair per well: a well with no requester
    /// is off, and one with a requester comes up unless this model says it does
    /// not.  The handshake writes the whole word back, so the other well's bits
    /// arrive in `value` and are read from it here rather than remembered.
    fn model_aux_write(&self, value: u32) {
        let mut next = value;
        for (index, (pin, _)) in CANDIDATES.iter().enumerate() {
            let well = pin.aux_well();
            if value & well.request_bit() == 0 || !self.comes_up[index] {
                next &= !well.state_bit();
            } else {
                next |= well.state_bit();
            }
        }
        self.aux.set(next);
    }
}

impl Registers for Bench {
    fn read(&self, register: Register) -> Option<u32> {
        if register.offset() == ICL_PWR_WELL_CTL_AUX2.offset() {
            // Ask the controller whether the register has an address at all: a
            // window that does not reach it has to read the same way here as it
            // would on hardware.  Then answer from the well model.
            self.controller.read(register)?;
            return Some(self.aux.get());
        }
        self.controller.read(register)
    }

    fn read64(&self, register: Register) -> Option<u64> {
        self.controller.read64(register)
    }

    fn write(&self, register: Register, value: u32) -> bool {
        self.writes.borrow_mut().push((register.offset(), value));
        if !self.controller.write(register, value) {
            return false;
        }
        if register.offset() == ICL_PWR_WELL_CTL_AUX2.offset() {
            self.model_aux_write(value);
        }
        true
    }
}

#[test]
fn both_candidate_wells_are_requested_before_the_first_gmbus_transaction() {
    let _guard = scheduler_test_context();
    let bench = Bench::with_monitor(Pin::DdiB, &edid_1080p60());
    let connector = resolve(bdf(), &bench, &FakeClock::new()).expect("a monitor on DDI B");

    let first_gmbus = bench
        .first_write_to(GMBUS0)
        .expect("the probe selects a pin before it reads one");
    let aux = bench.aux_writes();
    assert_eq!(
        aux.len(),
        2,
        "one request per candidate pin, and no rollback: both came up: {aux:?}"
    );
    for (index, (position, value)) in aux.iter().enumerate() {
        assert!(
            *position < first_gmbus,
            "a well was requested at {position}, after GMBUS was first used at {first_gmbus}"
        );
        // The candidate order is the reference's: pin index 1 (DDI A) first,
        // then pin index 2 (DDI B).
        let well = CANDIDATES[index].1;
        assert_eq!(
            value & well.request_mask(),
            well.request_mask(),
            "the write at {position} does not ask for {}",
            well.name
        );
    }
    // And the record the connector carries says the same thing the register
    // does: both wells came up, and both requests are still held.
    assert_eq!(connector.wells.len(), 2);
    for (well, observation) in &connector.wells {
        assert!(observation.state_set, "{well} did not come up");
        assert!(!observation.already_on, "{well} was already on");
    }
}

#[test]
fn the_candidate_table_agrees_with_the_pin_mapping_and_the_register_table() {
    for (pin, well) in CANDIDATES {
        let gmbus = pin.aux_well();
        assert_eq!(well.register, ICL_PWR_WELL_CTL_AUX2, "{pin}");
        assert_eq!(well.name, gmbus.name(), "{pin}");
        assert_eq!(well.index, gmbus.index(), "{pin}");
        assert_eq!(well.request_mask(), gmbus.request_bit(), "{pin}");
        assert_eq!(well.state_mask(), gmbus.state_bit(), "{pin}");
        // For these two wells the index is the DDI's number: AUX_A is index 0
        // of the register and DDI A is 0 of `SHOTPLUG_CTL_DDI`, and the same
        // for B.  A pin whose DDI and well disagreed would be a mapping bug
        // that only shows up on hardware.
        let ddi = pin.ddi().expect("a DDI pin");
        assert_eq!(ddi.index(), well.index, "{pin}");
    }
}

#[test]
fn the_aux_well_register_and_its_bits_are_the_ones_the_reference_names() {
    // Reference line 478 and section 4.2: AUX_A is index 0 of
    // ICL_PWR_WELL_CTL_AUX2 (0x45444) with request 0x2 and state 0x1, and AUX_B
    // is index 1, 0x8 and 0x4.  The register cross-check against i915 -- which
    // found the same offsets, indices and bits -- is written up in
    // docs/design/intel-connector.md; this pins the numbers so that a wrong
    // well cannot reach the machine as a silent edit.
    assert_eq!(ICL_PWR_WELL_CTL_AUX2.name(), "ICL_PWR_WELL_CTL_AUX2");
    assert_eq!(ICL_PWR_WELL_CTL_AUX2.offset(), 0x4_5444);
    assert_eq!(
        (
            power::AUX_A.index,
            power::AUX_A.request_mask(),
            power::AUX_A.state_mask()
        ),
        (0, 0x2, 0x1)
    );
    assert_eq!(
        (
            power::AUX_B.index,
            power::AUX_B.request_mask(),
            power::AUX_B.state_mask()
        ),
        (1, 0x8, 0x4)
    );
}

#[test]
fn a_well_that_never_comes_up_is_recorded_and_the_probe_runs_anyway() {
    let _guard = scheduler_test_context();
    let mut bench = Bench::with_monitor(Pin::DdiB, &edid_1080p60());
    bench.well_never_comes_up(Pin::DdiA);

    let connector = resolve(bdf(), &bench, &FakeClock::new()).expect(
        "a dead AUX_A is not a reason to stop: section 11 phase 2.1's symptom is a bus that NAKs, \
         and DDI B answered",
    );

    assert_eq!(connector.pin, Pin::DdiB);
    assert_eq!(connector.ddi, Ddi::B);
    assert_eq!(
        connector.wells.len(),
        2,
        "both candidate wells are recorded, up or not"
    );
    assert_eq!(connector.wells[0].0.name(), "AUX_A");
    assert!(
        !connector.wells[0].1.state_set,
        "the well that never came up is recorded as such"
    );
    assert!(connector.wells[1].1.state_set, "AUX_B came up");

    // The report a person reads off the screen or out of the debug file names
    // the well that is off *and* the connector that was still found.
    let report = ConnectReport {
        connectors: vec![connector],
        failures: Vec::new(),
        hotplug: Vec::new(),
    };
    let text = report.render();
    assert!(text.contains("AUX_A"), "{text}");
    assert!(text.contains("NEVER CAME UP"), "{text}");
    assert!(text.contains("1920x1080"), "{text}");

    // And the bus says the same thing in the reference's own words: pin 1 NAKs
    // every address *because its well is off*, which is the symptom section 11
    // phase 2.1 names.
    let probe = gmbus::probe_sink_with(&bench, &FakeClock::new());
    let pin_a = probe.outcomes[0];
    assert_eq!(pin_a.pin, Pin::DdiA);
    assert!(
        matches!(pin_a.result, Err(GmbusError::AuxWellDown { well, .. }) if well.name() == "AUX_A"),
        "{:?}",
        pin_a.result
    );
}

#[test]
fn a_monitor_on_ddi_a_is_a_connector_for_ddi_a() {
    let _guard = scheduler_test_context();
    let block = edid_1080p60();
    let bench = Bench::with_monitor(Pin::DdiA, &block);

    let connector = resolve(bdf(), &bench, &FakeClock::new()).expect("a monitor on DDI A");
    assert_eq!(connector.bdf, bdf());
    assert_eq!(connector.pin, Pin::DdiA);
    assert_eq!(connector.pin.index(), 1, "DDI A is pin index 1, one-based");
    assert_eq!(connector.ddi, Ddi::A);
    assert_eq!(connector.edid.bytes(), &block);
    assert_eq!(connector.extension, None, "the sink declared none");
    assert!(connector.plan.strict);
    assert_eq!(connector.plan.selection.mode.hdisplay, 1920);
    assert_eq!(connector.plan.selection.mode.vdisplay, 1080);
    assert_eq!(connector.plan.selection.mode.clock_khz, 148_500);
    assert_eq!(connector.hotplug.ddi, Ddi::A);
    assert!(connector.hotplug.enabled);
    assert!(
        connector.describe().contains("DDI A"),
        "{}",
        connector.describe()
    );
}

#[test]
fn a_monitor_on_ddi_b_is_a_connector_for_ddi_b() {
    let _guard = scheduler_test_context();
    let block = edid_1080p60();
    let bench = Bench::with_monitor(Pin::DdiB, &block);

    let connector = resolve(bdf(), &bench, &FakeClock::new()).expect("a monitor on DDI B");
    assert_eq!(connector.pin, Pin::DdiB);
    assert_eq!(connector.pin.index(), 2, "DDI B is pin index 2, one-based");
    assert_eq!(connector.ddi, Ddi::B);
    assert_eq!(connector.edid.bytes(), &block);
    assert_eq!(connector.hotplug.ddi, Ddi::B);
    assert!(connector.hotplug.enabled);
    // The two connectors differ in exactly the two places the pin decides.
    let on_a = resolve(
        bdf(),
        &Bench::with_monitor(Pin::DdiA, &block),
        &FakeClock::new(),
    )
    .expect("a monitor on DDI A");
    assert_ne!(on_a.pin, connector.pin);
    assert_ne!(on_a.ddi, connector.ddi);
    assert_eq!(on_a.edid, connector.edid);
    assert_eq!(on_a.plan.selection.mode, connector.plan.selection.mode);
}

#[test]
fn no_monitor_on_any_pin_is_reported_with_what_every_pin_answered() {
    let _guard = scheduler_test_context();
    let bench = Bench::bare();
    // A port with nothing on it: the modelled controller NAKs every address,
    // which is what a machine with no monitor looks like over DDC.
    bench.controller.detach();

    let error = resolve(bdf(), &bench, &FakeClock::new()).expect_err("nothing is attached");
    let ConnectError::NoMonitor { pins } = &error else {
        panic!("a bare bus is no monitor, not {error:?}");
    };
    assert_eq!(pins.outcomes.len(), 3, "DDI A, B and C are all asked");
    let text = error.describe();
    assert!(text.contains("no monitor answered"), "{text}");
    for pin in ["pin 1", "pin 2", "pin 3"] {
        assert!(text.contains(pin), "{pin} is missing from: {text}");
    }
    assert!(text.contains("NAK"), "{text}");
    // Pin 3 has no well requested -- section 11 phase 2.3 names pins 1 and 2 --
    // so its answer names the well rather than the cable, which is the
    // difference between this driver's own step and the sink's.
    assert!(text.contains("AUX_C"), "{text}");

    let report = ConnectReport {
        connectors: Vec::new(),
        failures: vec![(bdf(), error)],
        hotplug: Vec::new(),
    };
    let rendered = report.render();
    assert!(rendered.contains("display 0000:00:02.0"), "{rendered}");
    assert!(rendered.contains("no monitor answered"), "{rendered}");
}

#[test]
fn a_block_that_does_not_validate_is_reported_as_such_and_not_as_a_missing_monitor() {
    let _guard = scheduler_test_context();

    // A sink that answers with a block whose checksum is wrong: the read
    // happened and produced bytes this kernel will not believe.
    let mut checksum_broken = edid_1080p60();
    checksum_broken[gmbus::EDID_BLOCK_LEN - 1] ^= 0xff;
    let error = resolve(
        bdf(),
        &Bench::with_monitor(Pin::DdiA, &checksum_broken),
        &FakeClock::new(),
    )
    .expect_err("a block that does not validate is not an EDID");
    let ConnectError::EdidRejected { pin, error: cause } = &error else {
        panic!("a bad checksum is a rejected block, not {error:?}");
    };
    assert_eq!(*pin, Pin::DdiA);
    assert!(
        matches!(cause, GmbusError::EdidChecksum { .. }),
        "{cause:?}"
    );
    assert!(
        error.describe().contains("did not validate"),
        "{}",
        error.describe()
    );

    // A sink whose answer does not even start with the EDID header is the other
    // half of the same check, and is kept apart from it.
    let mut header_broken = edid_1080p60();
    header_broken[..8].copy_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]);
    let error = resolve(
        bdf(),
        &Bench::with_monitor(Pin::DdiB, &header_broken),
        &FakeClock::new(),
    )
    .expect_err("a block with no EDID header is not an EDID");
    let ConnectError::EdidRejected { pin, error: cause } = &error else {
        panic!("a bad header is a rejected block, not {error:?}");
    };
    assert_eq!(*pin, Pin::DdiB);
    assert!(matches!(cause, GmbusError::EdidHeader { .. }), "{cause:?}");
}

#[test]
fn a_second_call_does_not_disturb_the_first_answer() {
    let _guard = scheduler_test_context();
    let block = edid_1080p60();
    let bench = Bench::with_monitor(Pin::DdiA, &block);
    let clock = FakeClock::new();

    let first = resolve(bdf(), &bench, &clock).expect("a monitor on DDI A");
    let first_wells: Vec<(AuxWell, WellObservation)> = first.wells.clone();
    let second = resolve(bdf(), &bench, &clock).expect("the same monitor, again");

    // The first answer is still the first answer: the second pass re-read the
    // bus and did not reach back into the value it produced.
    assert_eq!(first.pin, Pin::DdiA);
    assert_eq!(first.edid.bytes(), &block);
    assert_eq!(first.wells, first_wells);
    assert!(first.wells[1].1.state_set);
    assert!(!first.wells[1].1.already_on);

    // The two passes agree about everything the pin decides...
    assert_eq!(first.pin, second.pin);
    assert_eq!(first.ddi, second.ddi);
    assert_eq!(first.edid, second.edid);
    assert_eq!(first.plan.selection.mode, second.plan.selection.mode);
    assert_eq!(first.hotplug.ddi, second.hotplug.ddi);
    // ...and differ in exactly one observable way: the second pass found the
    // well that the first pass asked for already on, which is what the record
    // is for.
    assert!(
        second.wells[1].1.already_on,
        "the second pass found the request the first pass had left set"
    );
}

#[test]
fn a_well_the_firmware_left_on_is_recorded_as_already_on() {
    let _guard = scheduler_test_context();
    let bench = Bench::with_monitor(Pin::DdiA, &edid_1080p60());
    // §11 phase 2.1 lists this first among the reasons a state bit can be set
    // before this driver asks: someone else is already holding the well.
    bench.well_already_on(Pin::DdiA);

    let connector = resolve(bdf(), &bench, &FakeClock::new()).expect("a monitor on DDI A");
    assert!(
        connector.wells[0].1.already_on,
        "AUX_A was on before the request"
    );
    assert!(connector.wells[0].1.state_set);
    assert!(
        !connector.wells[1].1.already_on,
        "AUX_B was this driver's to bring up"
    );
    assert!(
        connector.wells[0].1.describe().contains("was already on"),
        "{}",
        connector.wells[0].1.describe()
    );
    assert!(
        connector.wells[1].1.describe().contains("came up"),
        "{}",
        connector.wells[1].1.describe()
    );
}

#[test]
fn the_sink_step_still_finds_what_it_found_before_this_module_composed_it() {
    let _guard = scheduler_test_context();
    let block = edid_1080p60();
    // The sink's own double, with no well model in front of it: this is the
    // path that ran before phase 2.1 was composed in front of it, and composing
    // must not have changed what it reads.
    let controller = FakeController::with_monitor(Pin::DdiB, &block);
    let device = sink::probe_one(bdf(), &controller, &FakeClock::new(), Narration::Boot);

    assert_eq!(device.monitor, Some(Pin::DdiB));
    assert_eq!(device.edid.map(|edid| *edid.bytes()), Some(block));
    assert_eq!(device.pins.outcomes.len(), 3);
    assert!(device.hotplug_errors.is_empty());
    let plan = device.plan.expect("a plan for a monitor that answered");
    assert!(plan.strict, "{:?}", plan.edid_error);
    assert_eq!(plan.selection.mode.clock_khz, 148_500);

    // The rendering this module replaced lived on a `SinkReport` that nothing
    // constructs any more; what the step found is now asserted directly, which
    // is the same claim without a type kept alive only to be printed once.
    let text = device.pins.render();
    assert!(text.contains("monitor on pin 2"), "{text}");
    assert!(
        format!("{}", plan.selection.mode).contains("1920x1080"),
        "{}",
        plan.selection.mode
    );
}

#[test]
fn a_report_with_no_mapped_display_produces_no_connector_and_no_failure() {
    let _guard = scheduler_test_context();
    let report = ProbeReport::unavailable(
        "hosted test build: no PCI configuration space here",
        BusFacts {
            ecam_base: None,
            bus_end: 0,
        },
    );
    let connect = resolve_at_boot(&report);
    assert!(connect.connectors.is_empty());
    assert!(connect.failures.is_empty());
    assert!(connect.render().is_empty());
}
