//! The GMBUS protocol, driven through a controller that can be made to fail.
//!
//! Every test here runs the real state machine -- the same `transfer`,
//! `wait_for`, `failure` and `read_block` the target machine would run --
//! against [`FakeController`], which is a device model layered on a real
//! [`RegisterWindow`] over an ordinary buffer.  Two things follow from that,
//! and both are the point:
//!
//! * The register addresses, the access rules and the bounds checks under test
//!   are the real ones.  A write to a register the table declares read-only
//!   comes back as a refusal here exactly as it would on hardware.
//! * The failure paths -- a timeout, a NAK, a stuck bus, a floating bus, a
//!   corrupt block -- are reachable.  A working machine will not produce any of
//!   them on request, and they are the paths this workstream exists to get
//!   right.
//!
//! Nothing here says anything about an Alder Lake-N part.  It says the state
//! machine does what `gmbus.rs` says it does.

use alloc::{vec, vec::Vec};

use super::*;
use crate::{
    drm::intel::regs::{PROBE_WINDOW, RegisterWindow},
    test_support::scheduler_test_context,
};

/// A register file standing in for the aperture, sized like the window the
/// probe maps.
struct RegisterFile {
    words: Vec<u32>,
    window_len: usize,
}

impl RegisterFile {
    fn new() -> Self {
        Self {
            words: vec![0; PROBE_WINDOW / 4],
            window_len: PROBE_WINDOW,
        }
    }

    /// The window this file presents, which is the only way it is reached.
    fn window(&mut self) -> RegisterWindow {
        // SAFETY: `words` is a live, 4-byte aligned buffer of exactly
        // `PROBE_WINDOW` bytes, and `window_len` never exceeds it, so the
        // window is inside the allocation for as long as it is used.  The
        // buffer is owned by the test and is not aliased while a window over it
        // exists.
        unsafe { RegisterWindow::from_mapped(self.words.as_mut_ptr() as usize, self.window_len) }
    }
}

/// The controller, and a monitor behind it, with every knob a bring-up needs to
/// turn but cannot turn on real hardware.
struct FakeController {
    file: RegisterFile,
    /// The bytes the monitor answers with, from EEPROM address 0.
    eeprom: Vec<u8>,
    /// The same, for a transfer the model sees running at 50 kHz.  A sink that
    /// answers correctly only when the clock is slowed down is the case
    /// reference §11.1's rate-change recovery exists for.
    eeprom_at_50khz: Vec<u8>,
    /// Which pin the monitor is wired to; `None` for a port with nothing on it.
    answering_pin: Option<Pin>,
    /// Whether the device acknowledges at all.  A false here is what the
    /// reference calls "GMBUS returns NAK on every address".
    acknowledges: bool,
    /// How many four-byte words this device produces before it stops raising
    /// HW_RDY.  A number smaller than the transfer is a partial read.
    words_available: usize,
    /// Status bits the device holds.
    in_use: bool,
    active: bool,
    satoer: bool,
    stall: bool,
    /// Leave ACTIVE set after the stop cycle: a bus that never goes idle.
    stuck_after_stop: bool,
    /// NAK the first transaction only, as a passive adapter that needs a second
    /// try does.
    nak_first_transaction: bool,
    /// Writes, in order, as `(offset, value)`.
    writes: Vec<(u32, u32)>,
    /// Every command word written to `GMBUS1`.
    commands: Vec<u32>,
    /// Transactions started: a command with `SW_RDY` that is not a stop cycle
    /// and not an interrupt clear.
    transactions: u32,
    /// Words served in the current transaction.
    served: usize,
    /// The EEPROM address of the current transaction's index cycle.
    index_offset: usize,
    /// The rate field of the last pin selection.
    rate_field: u32,
    /// The pin field of the last pin selection.
    pin_field: u32,
    /// Whether a transaction is in progress.
    running: bool,
    /// How many times `GMBUS3` was read without `HW_RDY` being set first.  A
    /// protocol that reads data before the controller offers it would be
    /// building an EDID out of whatever the register happened to hold.
    reads_without_ready: u32,
}

impl FakeController {
    /// A controller with a monitor on `pin` answering with `block`.
    fn with_monitor(pin: Pin, block: &[u8; EDID_BLOCK_LEN]) -> Self {
        Self {
            eeprom: block.to_vec(),
            eeprom_at_50khz: block.to_vec(),
            answering_pin: Some(pin),
            words_available: usize::MAX,
            ..Self::bare()
        }
    }

    /// A controller with nothing behind it.
    fn bare() -> Self {
        Self {
            file: RegisterFile::new(),
            eeprom: Vec::new(),
            eeprom_at_50khz: Vec::new(),
            answering_pin: None,
            acknowledges: true,
            words_available: usize::MAX,
            in_use: false,
            active: false,
            satoer: false,
            stall: false,
            stuck_after_stop: false,
            nak_first_transaction: false,
            writes: Vec::new(),
            commands: Vec::new(),
            transactions: 0,
            served: 0,
            index_offset: 0,
            rate_field: 0,
            pin_field: 0,
            running: false,
            reads_without_ready: 0,
        }
    }

    /// The bytes the monitor is offering right now.
    fn eeprom(&self) -> &[u8] {
        if self.rate_field == Rate::Khz50.field() {
            &self.eeprom_at_50khz
        } else {
            &self.eeprom
        }
    }

    fn window(&mut self) -> RegisterWindow {
        self.file.window()
    }

    /// `GMBUS2` as the device would drive it.
    fn status(&self) -> u32 {
        let mut status = 0;
        if self.in_use {
            status |= GMBUS2_INUSE;
        }
        if self.active || self.running {
            status |= GMBUS2_ACTIVE;
        }
        if self.satoer {
            status |= GMBUS2_SATOER;
        }
        if self.stall {
            status |= GMBUS2_STALL_TIMEOUT;
        }
        if self.running
            && self.acknowledges
            && self.words_available != 0
            && self.served < self.words_available
        {
            status |= GMBUS2_HW_RDY;
        }
        status
    }

    /// `GMBUS3` as the device would drive it: four bytes out of the monitor's
    /// EEPROM, byte 0 in bits 7:0.
    fn take_word(&mut self) -> u32 {
        if self.status() & GMBUS2_HW_RDY == 0 {
            self.reads_without_ready += 1;
        }
        let base = self.index_offset + self.served * 4;
        let eeprom = self.eeprom();
        let mut word = 0u32;
        for byte in 0..4 {
            let value = eeprom.get(base + byte).copied().unwrap_or(0xff);
            word |= u32::from(value) << (8 * byte);
        }
        self.served += 1;
        word
    }

    /// What the device does when a command word lands.
    fn command(&mut self, value: u32) {
        self.commands.push(value);
        if value & GMBUS1_SW_CLR_INT != 0 {
            // The controller reset clears a latched NAK.  It does not clear a
            // stall: a secondary holding the clock is not something the
            // controller can reset away, which is why that failure survives the
            // recovery and is reported as what it is.
            self.satoer = false;
            self.running = false;
            self.active = false;
            return;
        }
        if value & GMBUS1_CYCLE_STOP != 0 {
            self.running = false;
            if !self.stuck_after_stop {
                self.active = false;
            }
            return;
        }
        if value & GMBUS1_SW_RDY == 0
            || value & GMBUS1_CYCLE_INDEX == 0
            || value & GMBUS1_CYCLE_WAIT == 0
        {
            return;
        }
        // A transfer starts.
        self.transactions += 1;
        self.served = 0;
        self.index_offset =
            ((value & GMBUS1_SLAVE_INDEX_MASK) >> GMBUS1_SLAVE_INDEX_SHIFT) as usize;
        let pin = Pin::ALL
            .into_iter()
            .find(|pin| pin.index() == self.pin_field);
        let wrong_pin = match self.answering_pin {
            Some(answering) => pin != Some(answering),
            None => false,
        };
        let nak_now = self.nak_first_transaction && self.transactions == 1;
        if !self.acknowledges || wrong_pin || nak_now {
            self.satoer = true;
            self.running = false;
            self.active = false;
        } else {
            self.running = true;
            self.active = true;
        }
    }

    /// Every value written to `register`, in order.
    fn writes_to(&self, register: Register) -> Vec<u32> {
        self.writes
            .iter()
            .filter(|(offset, _)| *offset == register.offset())
            .map(|(_, value)| *value)
            .collect()
    }

    /// The last value written to `register`.
    fn last_write(&self, register: Register) -> Option<u32> {
        self.writes_to(register).pop()
    }

    /// What `register` holds now, read through the real window.
    fn peek(&mut self, register: Register) -> Option<u32> {
        self.window().read(register)
    }

    /// Make `pin`'s AUX/DDC power well read back as on.
    ///
    /// The well is a register, so the test writes the register: anything else
    /// would be testing the test's idea of the power well rather than the
    /// state bit the driver reads.
    fn set_well_on(&mut self, pin: Pin) {
        let word = ICL_PWR_WELL_CTL_AUX2.offset() as usize / 4;
        self.file.words[word] |= pin.aux_well().state_bit();
    }
}

impl BusRegisters for FakeController {
    fn read(&mut self, register: Register) -> Option<u32> {
        if register.offset() == GMBUS2.offset() {
            return Some(self.status());
        }
        if register.offset() == GMBUS3.offset() {
            return Some(self.take_word());
        }
        self.window().read(register)
    }

    fn write(&mut self, register: Register, value: u32) -> bool {
        // The real window enforces the register table's access rules, so a
        // write to a read-only register is refused here for the same reason it
        // would be on hardware.
        if !self.window().write(register, value) {
            return false;
        }
        self.writes.push((register.offset(), value));
        if register.offset() == GMBUS1.offset() {
            self.command(value);
        }
        if register.offset() == GMBUS0.offset() {
            self.pin_field = value & GMBUS0_PIN_MASK;
            self.rate_field = (value & GMBUS0_RATE_MASK) >> GMBUS0_RATE_SHIFT;
        }
        true
    }
}

/// A clock the test owns, so that a 50 ms timeout costs microseconds.
struct FakeClock {
    micros: core::cell::Cell<u64>,
}

/// How much time one poll costs.  Coarse on purpose: it is many times the
/// real poll interval so that a timeout test finishes in a few thousand
/// iterations instead of tens of thousands, and every deadline assertion is
/// still exact.
const CLOCK_STEP_MICROS: u64 = 25;

impl FakeClock {
    fn new() -> Self {
        Self {
            micros: core::cell::Cell::new(0),
        }
    }
}

impl PollTimer for FakeClock {
    fn now_micros(&self) -> u64 {
        self.micros.get()
    }

    fn pause(&self) {
        self.micros.set(self.micros.get() + CLOCK_STEP_MICROS);
    }
}

/// A structurally valid EDID base block: the real header, version 1.3, a
/// declared extension count, and a checksum that makes the 128 bytes sum to
/// zero.  The descriptors are left as zeroes, which is not a monitor anybody
/// would ship, but every check the transport makes passes.
fn valid_edid(extension_count: u8) -> [u8; EDID_BLOCK_LEN] {
    let mut block = [0u8; EDID_BLOCK_LEN];
    block[..8].copy_from_slice(&EDID_HEADER);
    block[8] = 0x04;
    block[9] = 0x21;
    block[0x12] = 0x01;
    block[0x13] = 0x03;
    block[0x7e] = extension_count;
    let sum = block[..EDID_BLOCK_LEN - 1]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    block[EDID_BLOCK_LEN - 1] = 0u8.wrapping_sub(sum);
    block
}

/// Read one block from a fake, with the notes, the way the driver does.
fn read(controller: &mut FakeController, pin: Pin) -> (Result<EdidBytes, GmbusError>, BusNotes) {
    let clock = FakeClock::new();
    let mut notes = BusNotes::default();
    let result = read_edid_with(controller, &clock, pin, &mut notes);
    (result, notes)
}

#[test]
fn the_pin_index_is_one_based_and_ddi_a_is_pin_one() {
    // Reference §9.2 flags this in bold, and `[I915]`'s constants agree:
    // `GMBUS_PIN_1_BXT = 1` is DDI A.  A zero-based table would write 0 for
    // DDI A, and 0 in `GMBUS0[4:0]` means the controller is disconnected -- so
    // the mistake presents as "no monitor on any pin".
    assert_eq!(Pin::DdiA.index(), 1);
    assert_eq!(Pin::DdiB.index(), 2);
    assert_eq!(Pin::DdiC.index(), 3);
    assert_eq!(Pin::Tc1.index(), 9);
    assert_eq!(Pin::Tc4.index(), 12);
    for pin in Pin::ALL {
        assert_ne!(pin.index(), 0, "{pin} must not select the disconnected pin");
        assert!(
            pin.index() <= GMBUS0_PIN_MASK,
            "{pin} does not fit GMBUS0[4:0]"
        );
    }
    // Every pin has its own index and its own GPIO pair; a duplicated index
    // would mean two connectors fighting over one DDC channel.
    for (position, pin) in Pin::ALL.iter().enumerate() {
        for other in &Pin::ALL[position + 1..] {
            assert_ne!(pin.index(), other.index());
            assert_ne!(pin.name(), other.name());
            assert_ne!(pin.gpio(), other.gpio());
        }
    }
    // The DDI pins are the ones with a hotplug index and a GMBUS pin both.
    assert_eq!(Pin::DdiA.ddi(), Some(Ddi::A));
    assert_eq!(Pin::DdiB.ddi(), Some(Ddi::B));
    assert_eq!(Pin::DdiC.ddi(), Some(Ddi::C));
    assert_eq!(Pin::Tc1.ddi(), None);
    // DDI D has hotplug but no pin in the ICP table ([I915]
    // `display/intel_gmbus.c:113-124`), so the two vocabularies do not map onto
    // each other one-for-one.
    assert_eq!(Ddi::D.pin(), None);
    assert_eq!(Ddi::A.pin(), Some(Pin::DdiA));
    // And the scan order is DDI A first, as §11 phase 2.3 recommends.
    assert_eq!(Pin::DDC, [Pin::DdiA, Pin::DdiB, Pin::DdiC]);
}

#[test]
fn the_aux_well_bits_match_the_reference_table() {
    // Reference §4.2's table of XE_LPD wells, checked against the arithmetic
    // this module uses to decide whether a NAK means "powered down".  The
    // request bits are never written by this driver, which is exactly why they
    // are checked here: if the index or the shift were wrong, both halves would
    // be wrong together and the error message would name the wrong well.
    let table: [(Pin, &str, u32, u32, u32); 4] = [
        (Pin::DdiA, "AUX_A", 0, 0x2, 0x1),
        (Pin::DdiB, "AUX_B", 1, 0x8, 0x4),
        (Pin::DdiC, "AUX_C", 2, 0x20, 0x10),
        (Pin::Tc1, "AUX_USBC1", 3, 0x80, 0x40),
    ];
    for (pin, name, index, request, state) in table {
        let well = pin.aux_well();
        assert_eq!(well.name(), name, "{pin}");
        assert_eq!(well.index(), index, "{pin}");
        assert_eq!(well.request_bit(), request, "{pin}");
        assert_eq!(well.state_bit(), state, "{pin}");
    }
    // The Type-C wells continue the same ladder from `TGL_PW_CTL_IDX_AUX_TC2`
    // upward ([I915] `i915_reg.h:3675-3685`).
    assert_eq!(Pin::Tc2.aux_well().state_bit(), 0x100);
    assert_eq!(Pin::Tc3.aux_well().state_bit(), 0x400);
    assert_eq!(Pin::Tc4.aux_well().state_bit(), 0x1000);
    // Every pin's well is distinct, or two ports would share a diagnosis.
    for (position, pin) in Pin::ALL.iter().enumerate() {
        for other in &Pin::ALL[position + 1..] {
            assert_ne!(pin.aux_well().index(), other.aux_well().index());
        }
    }
}

#[test]
fn the_command_word_is_the_one_the_sources_say() {
    let _guard = scheduler_test_context();
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    let (result, notes) = read(&mut controller, Pin::DdiA);
    assert_eq!(result, Ok(EdidBytes { bytes: block }), "{notes:?}");

    // Pin select: rate 100 kHz (field 0) and pin 1.  GMBUS0 is written more
    // than once (the reset writes 0), so the *last* value before the data is
    // the one that matters.
    let selects = controller.writes_to(GMBUS0);
    assert_eq!(
        selects.first(),
        Some(&0),
        "the reset releases the pin first"
    );
    assert_eq!(
        selects.last(),
        Some(&0),
        "and the transaction releases it after"
    );
    assert!(selects.contains(&0x0000_0001), "{selects:x?}");

    // The command word, decoded back through the masks rather than compared to
    // a remembered constant: index cycle, wait, 128 bytes, slave 0x50, read,
    // software ready.  `[I915]` builds exactly this at
    // `display/intel_gmbus.c:596-611` and writes it at `:451-452`.
    let command = *controller
        .commands
        .first()
        .expect("a command word was written");
    assert_eq!(
        command & GMBUS1_SW_CLR_INT,
        0,
        "this is a transfer, not a clear"
    );
    assert_eq!(
        command & GMBUS1_CYCLE_MASK,
        GMBUS1_CYCLE_INDEX | GMBUS1_CYCLE_WAIT,
        "the index cycle and the read are one command"
    );
    assert_eq!(
        (command & GMBUS1_BYTE_COUNT_MASK) >> GMBUS1_BYTE_COUNT_SHIFT,
        128
    );
    assert_eq!(
        (command & GMBUS1_SLAVE_INDEX_MASK) >> GMBUS1_SLAVE_INDEX_SHIFT,
        u32::from(EDID_BASE_OFFSET)
    );
    assert_eq!(
        (command & GMBUS1_SLAVE_ADDR_MASK) >> GMBUS1_SLAVE_ADDR_SHIFT,
        u32::from(DDC_ADDRESS)
    );
    assert_ne!(command & GMBUS1_SLAVE_READ, 0, "a read, not a write");
    assert_ne!(command & GMBUS1_SW_RDY, 0, "software ready starts it");
    assert_eq!(command & GMBUS0_BYTE_CNT_OVERRIDE, 0, "no burst override");
    // The whole word, for a reader comparing it against a bus trace:
    // 0x4680_00a1 = SW_RDY | CYCLE_INDEX | CYCLE_WAIT | 128 bytes | 0x50 << 1 |
    // read.
    assert_eq!(command, 0x4680_00a1);

    // The stop cycle is issued after the data, unconditionally ([I915]
    // `intel_gmbus.c:660-664`).
    let stop = *controller.commands.last().expect("a stop cycle");
    assert_eq!(stop & GMBUS1_CYCLE_MASK, GMBUS1_CYCLE_STOP);
    // The interrupt mask is left cleared: this driver polls.
    assert_eq!(controller.writes_to(GMBUS4), vec![0]);
    // And the protocol never took data the controller had not offered.
    assert_eq!(controller.reads_without_ready, 0);
}

#[test]
fn the_command_selects_the_pin_the_caller_asked_for() {
    let _guard = scheduler_test_context();
    for pin in Pin::DDC {
        let block = valid_edid(0);
        let mut controller = FakeController::with_monitor(pin, &block);
        assert_eq!(read(&mut controller, pin).0, Ok(EdidBytes { bytes: block }));
        assert!(
            controller.writes_to(GMBUS0).contains(&pin.index()),
            "{} should have selected GMBUS0[4:0] = {}",
            pin,
            pin.index()
        );
    }
}

#[test]
fn a_valid_block_is_returned_byte_for_byte() {
    let _guard = scheduler_test_context();
    let mut block = valid_edid(1);
    // Make the block distinctive so that a shuffled or truncated read cannot
    // pass by accident.
    for (index, byte) in block.iter_mut().enumerate().take(0x7e).skip(0x14) {
        *byte = index as u8;
    }
    let sum = block[..EDID_BLOCK_LEN - 1]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    block[EDID_BLOCK_LEN - 1] = 0u8.wrapping_sub(sum);

    let mut controller = FakeController::with_monitor(Pin::DdiB, &block);
    let (result, notes) = read(&mut controller, Pin::DdiB);
    let bytes = result.expect("a valid block must read");
    assert_eq!(bytes.as_slice(), &block[..]);
    assert_eq!(bytes.bytes(), &block);
    assert_eq!(bytes.extension_count(), 1);
    assert!(notes.is_quiet(), "{notes:?}");
    assert_eq!(notes.attempts, 1);
    assert_eq!(controller.reads_without_ready, 0);
}

#[test]
fn a_bad_checksum_is_named_and_retried_at_a_lower_rate() {
    let _guard = scheduler_test_context();
    // The monitor answers at 100 kHz with a block whose last byte is wrong.
    let good = valid_edid(0);
    let mut corrupt = good;
    corrupt[EDID_BLOCK_LEN - 1] ^= 0x5a;
    let mut controller = FakeController::with_monitor(Pin::DdiA, &corrupt);
    // A second controller whose sink only answers correctly at 50 kHz: the
    // reference's recovery (§11.1, "Re-read; if persistent, lower the rate").
    let mut recovered = FakeController::with_monitor(Pin::DdiA, &corrupt);
    recovered.eeprom_at_50khz = good.to_vec();

    let (result, notes) = read(&mut controller, Pin::DdiA);
    let error = result.expect_err("a corrupt block must not be returned");
    assert!(matches!(error, GmbusError::EdidChecksum { sum, .. } if sum != 0));
    assert_eq!(notes.attempts, 2, "the read is retried once");
    assert_eq!(notes.last_rate, Rate::Khz50, "and the retry is slower");
    // Two transactions, and the second one selected the 50 kHz field.
    let selects = controller.writes_to(GMBUS0);
    assert!(selects.contains(&((Rate::Khz50.field() << GMBUS0_RATE_SHIFT) | 1)));
    assert!(
        error.describe().contains("modulo 256"),
        "{}",
        error.describe()
    );

    let (result, notes) = read(&mut recovered, Pin::DdiA);
    assert_eq!(result, Ok(EdidBytes { bytes: good }), "{notes:?}");
    assert_eq!(notes.attempts, 2);
}

#[test]
fn a_bad_header_is_named_and_not_reported_as_a_checksum_problem() {
    let _guard = scheduler_test_context();
    // All zeroes: the checksum is right (it is zero) and the header is not, so
    // this isolates the header check from the checksum check.
    let mut block = [0u8; EDID_BLOCK_LEN];
    block[0] = 0x00;
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    let (result, _) = read(&mut controller, Pin::DdiA);
    match result {
        Err(GmbusError::EdidHeader { pin, header }) => {
            assert_eq!(pin, Pin::DdiA);
            assert_eq!(header, [0u8; 8]);
        }
        other => panic!("expected a header failure, got {other:?}"),
    }
}

#[test]
fn a_floating_bus_is_named_rather_than_called_a_bad_header() {
    let _guard = scheduler_test_context();
    // A bus with no device and working pull-ups reads as all ones; the
    // reference lists that separately from a wrong header because the causes
    // differ (§11.1).
    let block = [0xffu8; EDID_BLOCK_LEN];
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    controller.answering_pin = None;
    let (result, _) = read(&mut controller, Pin::DdiA);
    assert_eq!(result, Err(GmbusError::BusFloating { pin: Pin::DdiA }));
}

#[test]
fn a_bus_that_never_offers_data_times_out_and_is_left_released() {
    let _guard = scheduler_test_context();
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    // The controller accepts the command and then says nothing.
    controller.words_available = 0;
    let (result, _) = read(&mut controller, Pin::DdiA);
    match result {
        Err(GmbusError::ReadyTimeout {
            pin,
            rate,
            waited_micros,
            ..
        }) => {
            assert_eq!(pin, Pin::DdiA);
            assert_eq!(rate, Rate::Khz100);
            assert!(
                waited_micros >= READY_TIMEOUT_MICROS,
                "a timeout must wait its budget, not less: {waited_micros}"
            );
        }
        other => panic!("expected a ready timeout, got {other:?}"),
    }
    // The recovery ran: the pin was released, the interrupt mask is clear and
    // the controller was reset through SW_CLR_INT.
    assert_eq!(controller.peek(GMBUS0), Some(0));
    assert_eq!(controller.peek(GMBUS4), Some(0));
    let commands = &controller.commands;
    assert!(
        commands
            .iter()
            .any(|command| command & GMBUS1_SW_CLR_INT != 0),
        "the latched error must be cleared"
    );
    assert_eq!(controller.transactions, 2, "the timeout is retried once");
}

#[test]
fn a_partial_read_never_becomes_a_short_block() {
    let _guard = scheduler_test_context();
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    // Two words out of thirty-two, then silence.
    controller.words_available = 2;
    let (result, _) = read(&mut controller, Pin::DdiA);
    assert!(
        matches!(result, Err(GmbusError::ReadyTimeout { .. })),
        "a partial transfer is a failure, not a short block: {result:?}"
    );
    // The bytes the device did send are not available to anybody: there is no
    // partial `EdidBytes` to be mistaken for a block.
    assert_eq!(controller.reads_without_ready, 0);
}

#[test]
fn a_nak_with_the_power_well_off_names_the_well() {
    let _guard = scheduler_test_context();
    // The case the brief and the reference both single out: GMBUS NAKs on
    // every address because the AUX/DDC power well for that pin pair is not
    // enabled.  It must be that, and not a timeout.
    let mut controller = FakeController::bare();
    controller.acknowledges = false;
    controller.answering_pin = Some(Pin::DdiB);
    let (result, _) = read(&mut controller, Pin::DdiB);
    match result {
        Err(GmbusError::AuxWellDown {
            pin, well, address, ..
        }) => {
            assert_eq!(pin, Pin::DdiB);
            assert_eq!(well.name(), "AUX_B");
            assert_eq!(address, DDC_ADDRESS);
        }
        other => panic!("expected the power-well diagnosis, got {other:?}"),
    }
    let error = result.unwrap_err();
    assert!(error.aux_well_is_down());
    let text = error.describe();
    assert!(text.contains("AUX_B"), "{text}");
    assert!(text.contains("not enabled"), "{text}");
    assert!(text.contains("NAKed"), "{text}");
}

#[test]
fn a_nak_with_the_power_well_on_is_only_a_nak() {
    let _guard = scheduler_test_context();
    let mut controller = FakeController::bare();
    controller.acknowledges = false;
    controller.answering_pin = Some(Pin::DdiA);
    controller.set_well_on(Pin::DdiA);
    let (result, _) = read(&mut controller, Pin::DdiA);
    match result {
        Err(GmbusError::NoAck { well, address, .. }) => {
            assert_eq!(well, AuxWellReading::On);
            assert_eq!(address, DDC_ADDRESS);
        }
        other => panic!("expected a plain NAK, got {other:?}"),
    }
    assert!(!result.unwrap_err().aux_well_is_down());
}

#[test]
fn a_nak_is_retried_once_and_a_late_answer_is_used() {
    let _guard = scheduler_test_context();
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    // "Passive adapters sometimes NAK the first probe" ([I915]
    // `display/intel_gmbus.c:714-725`).
    controller.nak_first_transaction = true;
    let (result, notes) = read(&mut controller, Pin::DdiA);
    assert_eq!(result, Ok(EdidBytes { bytes: block }));
    assert_eq!(notes.attempts, 2);
    assert_eq!(controller.transactions, 2);
}

#[test]
fn a_stuck_bus_is_named_and_the_recovery_releases_the_pin() {
    let _guard = scheduler_test_context();
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    // The transaction completes and the bus never goes idle afterwards.
    controller.stuck_after_stop = true;
    let (result, _) = read(&mut controller, Pin::DdiA);
    match result {
        Err(GmbusError::BusStuck {
            pin,
            status,
            waited_micros,
        }) => {
            assert_eq!(pin, Pin::DdiA);
            assert_ne!(status & GMBUS2_ACTIVE, 0, "ACTIVE is what did not clear");
            assert!(waited_micros >= IDLE_TIMEOUT_MICROS);
        }
        other => panic!("expected a stuck bus, got {other:?}"),
    }
    assert_eq!(controller.peek(GMBUS0), Some(0));
}

#[test]
fn a_stall_is_reported_with_the_status_bit_that_says_so() {
    let _guard = scheduler_test_context();
    let mut controller = FakeController::bare();
    controller.stall = true;
    let (result, _) = read(&mut controller, Pin::DdiA);
    match result {
        Err(GmbusError::BusStuck { status, .. }) => {
            assert_ne!(status & GMBUS2_STALL_TIMEOUT, 0);
        }
        other => panic!("expected a stalled bus, got {other:?}"),
    }
    assert!(
        GmbusError::BusStuck {
            pin: Pin::DdiA,
            status: GMBUS2_STALL_TIMEOUT,
            waited_micros: 0,
        }
        .describe()
        .contains("STALL_TIMEOUT")
    );
}

#[test]
fn the_firmware_leaving_the_index_register_in_two_byte_mode_is_recorded() {
    let _guard = scheduler_test_context();
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    controller.file.words[GMBUS5.offset() as usize / 4] = GMBUS5_2BYTE_INDEX_EN | 0x1234;
    let (result, notes) = read(&mut controller, Pin::DdiA);
    assert_eq!(result, Ok(EdidBytes { bytes: block }));
    assert!(notes.stale_two_byte_index);
    assert!(!notes.is_quiet());
    assert!(
        notes
            .describe()
            .unwrap_or_default()
            .contains("two-byte index mode")
    );
    // And it was cleared before the transaction, so the index phase is the
    // one-byte phase this driver programs.
    assert_eq!(controller.last_write(GMBUS5), Some(0));
}

#[test]
fn a_bus_already_in_use_is_recorded() {
    let _guard = scheduler_test_context();
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    controller.in_use = true;
    let (result, notes) = read(&mut controller, Pin::DdiA);
    assert_eq!(result, Ok(EdidBytes { bytes: block }));
    assert!(notes.was_in_use);
}

#[test]
fn a_window_that_stops_before_the_index_register_is_named() {
    let _guard = scheduler_test_context();
    // A window that reaches GMBUS4 but not GMBUS5: the first two writes and
    // the status read succeed, and the register that says how the index phase
    // is interpreted is the one that is missing.
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    controller.file.window_len = GMBUS5.offset() as usize;
    let (result, _) = read(&mut controller, Pin::DdiA);
    assert_eq!(
        result,
        Err(GmbusError::WindowTooSmall { register: "GMBUS5" })
    );
}

#[test]
fn a_window_that_does_not_reach_gmbus_at_all_refuses_the_write() {
    let _guard = scheduler_test_context();
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiA, &block);
    controller.file.window_len = 0x100;
    let (result, _) = read(&mut controller, Pin::DdiA);
    assert_eq!(
        result,
        Err(GmbusError::RegisterRefused { register: "GMBUS0" })
    );
}

#[test]
fn reading_a_later_block_asks_the_eeprom_for_that_offset() {
    let _guard = scheduler_test_context();
    let base = valid_edid(1);
    let extension = {
        let mut block = valid_edid(0);
        block[0x14] = 0x5a;
        let sum = block[..EDID_BLOCK_LEN - 1]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        block[EDID_BLOCK_LEN - 1] = 0u8.wrapping_sub(sum);
        block
    };
    let mut controller = FakeController::with_monitor(Pin::DdiA, &base);
    // The monitor's EEPROM is one image: the base block at 0 and the extension
    // at 0x80.
    controller.eeprom.resize(2 * EDID_BLOCK_LEN, 0);
    controller.eeprom[..EDID_BLOCK_LEN].copy_from_slice(&base);
    controller.eeprom[EDID_BLOCK_LEN..].copy_from_slice(&extension);
    controller.eeprom_at_50khz = controller.eeprom.clone();

    let clock = FakeClock::new();
    let mut notes = BusNotes::default();
    let block = read_block(
        &mut controller,
        &clock,
        Pin::DdiA,
        DDC_ADDRESS,
        EDID_EXTENSION_OFFSET,
        &mut notes,
    )
    .expect("the extension block must read");
    assert_eq!(block.bytes(), &extension);
    // The second transaction's index cycle carried 0x80.  The *last* command
    // is the stop cycle, so this looks for the last transfer command.
    let command = *controller
        .commands
        .iter()
        .filter(|command| *command & GMBUS1_CYCLE_INDEX != 0)
        .next_back()
        .expect("a transfer command");
    assert_eq!(
        (command & GMBUS1_SLAVE_INDEX_MASK) >> GMBUS1_SLAVE_INDEX_SHIFT,
        0x80
    );
}

#[test]
fn the_extension_read_follows_the_count_in_the_base_block() {
    let _guard = scheduler_test_context();
    let base = valid_edid(1);
    let extension = {
        let mut block = valid_edid(0);
        block[0x14] = 0xc7;
        let sum = block[..EDID_BLOCK_LEN - 1]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        block[EDID_BLOCK_LEN - 1] = 0u8.wrapping_sub(sum);
        block
    };
    let mut controller = FakeController::with_monitor(Pin::DdiA, &base);
    controller.eeprom.resize(2 * EDID_BLOCK_LEN, 0);
    controller.eeprom[..EDID_BLOCK_LEN].copy_from_slice(&base);
    controller.eeprom[EDID_BLOCK_LEN..].copy_from_slice(&extension);
    controller.eeprom_at_50khz = controller.eeprom.clone();

    let block = read_edid_extension_with(&mut controller, &FakeClock::new(), Pin::DdiA)
        .expect("the base and its declared extension must read");
    assert_eq!(block.map(|block| *block.bytes()), Some(extension));
    // The extension is a second transaction at EEPROM address 0x80.
    assert_eq!(controller.transactions, 2);

    // A sink that declares no extension costs one transaction and no guess.
    let mut plain = FakeController::with_monitor(Pin::DdiA, &valid_edid(0));
    let none = read_edid_extension_with(&mut plain, &FakeClock::new(), Pin::DdiA)
        .expect("a base block with no extensions is a valid answer");
    assert_eq!(none, None);
    assert_eq!(plain.transactions, 1);
}

#[test]
fn the_sink_probe_tries_every_ddc_pin_and_says_which_one_answered() {
    let _guard = scheduler_test_context();
    let block = valid_edid(0);
    let mut controller = FakeController::with_monitor(Pin::DdiB, &block);
    controller.set_well_on(Pin::DdiA);
    let probe = probe_sink_with(&mut controller, &FakeClock::new());
    assert_eq!(probe.found(), Some(Pin::DdiB));
    assert_eq!(probe.edid(), Some(EdidBytes { bytes: block }));
    assert_eq!(probe.outcomes.len(), Pin::DDC.len());
    assert_eq!(probe.outcomes[0].pin, Pin::DdiA);
    assert!(probe.outcomes[0].result.is_err());
    assert!(probe.outcomes[1].result.is_ok());
    assert!(probe.outcomes[2].result.is_err());
    let text = probe.render();
    assert!(text.contains("monitor on pin 2"), "{text}");
    assert!(text.contains("pin 1"), "{text}");
    assert!(text.contains("pin 3"), "{text}");
    // The log path is exercised too: on host it writes to the test output, and
    // the value of the test is that nothing in it panics or allocates wildly.
    probe.log();
}

#[test]
fn a_probe_with_no_monitor_says_so_on_every_pin() {
    let _guard = scheduler_test_context();
    let mut controller = FakeController::bare();
    controller.acknowledges = false;
    let probe = probe_sink_with(&mut controller, &FakeClock::new());
    assert_eq!(probe.found(), None);
    assert_eq!(probe.edid(), None);
    let text = probe.render();
    assert!(text.contains("no monitor"), "{text}");
    for pin in Pin::DDC {
        assert!(text.contains(&format!("pin {}", pin.index())), "{text}");
    }
}

#[test]
fn rates_carry_their_field_and_step_down_once() {
    assert_eq!(Rate::Khz100.field(), 0);
    assert_eq!(Rate::Khz50.field(), 1);
    assert_eq!(Rate::Khz400.field(), 2);
    assert_eq!(Rate::Mhz1.field(), 3);
    assert_eq!(Rate::Khz100.khz(), 100);
    assert_eq!(Rate::DEFAULT, Rate::Khz100);
    assert_eq!(Rate::Khz100.slower(), Some(Rate::Khz50));
    assert_eq!(Rate::Khz400.slower(), Some(Rate::Khz50));
    assert_eq!(Rate::Khz50.slower(), None, "the ladder has a bottom");
    // The retry ladder in `read_block` is exactly one step, so a sink that
    // needs a third rate reports rather than loops.
    assert_eq!(Rate::DEFAULT.slower().and_then(Rate::slower), None);
}

#[test]
fn a_status_word_is_decoded_for_the_log() {
    assert_eq!(describe_status(0), "no status bit set");
    assert_eq!(describe_status(GMBUS2_SATOER), "SATOER");
    assert_eq!(describe_status(GMBUS2_HW_RDY), "HW_RDY");
    assert_eq!(
        describe_status(GMBUS2_ACTIVE | GMBUS2_STALL_TIMEOUT),
        "STALL_TIMEOUT|ACTIVE"
    );
    assert_eq!(
        describe_status(GMBUS2_INUSE | GMBUS2_HW_WAIT_PHASE | GMBUS2_INT),
        "INUSE|HW_WAIT_PHASE|INT"
    );
}

#[test]
fn a_transfer_length_outside_one_block_is_refused() {
    let _guard = scheduler_test_context();
    let mut controller = FakeController::bare();
    let clock = FakeClock::new();
    let mut notes = BusNotes::default();
    let mut bus = Bus {
        registers: &mut controller,
        timer: &clock,
        pin: Pin::DdiA,
        rate: Rate::DEFAULT,
        notes: &mut notes,
    };
    assert!(
        matches!(
            bus.transfer(DDC_ADDRESS, EDID_BASE_OFFSET, &mut []),
            Err(GmbusError::WindowTooSmall { .. })
        ),
        "an empty transfer is refused"
    );
    let mut too_long = [0u8; MAX_TRANSFER + 4];
    assert!(matches!(
        bus.transfer(DDC_ADDRESS, EDID_BASE_OFFSET, &mut too_long),
        Err(GmbusError::WindowTooSmall { .. })
    ));
    // Nothing was written: the refusal happens before the bus is touched.
    assert!(controller.commands.is_empty());
}

#[test]
fn edid_validation_rejects_exactly_the_two_things_it_checks() {
    let block = valid_edid(0);
    assert_eq!(validate_edid(&block), Ok(()));
    assert!(!all_ones(&block));

    let mut bad_header = block;
    bad_header[0] = 0x01;
    // Fix the checksum so that the header is the only thing wrong.
    bad_header[EDID_BLOCK_LEN - 1] = 0;
    let sum = bad_header[..EDID_BLOCK_LEN - 1]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    bad_header[EDID_BLOCK_LEN - 1] = 0u8.wrapping_sub(sum);
    assert!(matches!(
        validate_edid(&bad_header),
        Err(EdidFault::Header(_))
    ));

    let mut bad_checksum = block;
    bad_checksum[0x20] ^= 0xff;
    assert!(matches!(
        validate_edid(&bad_checksum),
        Err(EdidFault::Checksum(_))
    ));
    assert!(all_ones(&[0xffu8; EDID_BLOCK_LEN]));
}
