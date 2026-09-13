//! Synthetic hardware for the host tests.
//!
//! There is no i225/i226 device model in QEMU, so no host or guest run can put
//! this driver in front of the real thing.  What *can* be tested is everything
//! between the driver's logic and the bus: given a device that answers the way
//! the part is documented to answer, does the driver reach the right
//! conclusion, refuse the right operation, and terminate?  That is what these
//! types are for.
//!
//! [`NicModel`] is that device, and it is deliberately not a simulator.  It
//! models *only* the behaviours the driver relies on, and each modelled
//! behaviour is a cited fact with a comment saying where it came from:
//!
//! * `CTRL.RST` is self-clearing (nothing in the vendor driver writes it back
//!   clear, and `igc_base.c:43` relies on that);
//! * `CTRL.GIO_MASTER_DISABLE` makes `STATUS.GIO_MASTER_ENABLE` clear, which is
//!   what `igc_disable_pcie_master` polls for (`igc_mac.c:21`);
//! * the NVM auto-read sets `EECD.AUTO_RD` a bounded time after a reset
//!   (`igc_mac.c:650`);
//! * an `IGC_MDIC` read command returns the addressed MII register and sets
//!   `IGC_MDIC_READY` (`igc_phy.c:544`);
//! * the PHY reports link in its MII status register once the cable is up
//!   (`igc_phy.c:64`), and `STATUS.LU`/`FD`/speed follow it
//!   (`igc_mac.c:681`).
//!
//! A behaviour that is not modelled is not silently benign either: a read of a
//! register the test did not seed returns [`FakeBus::unseeded`], which the
//! tests set to `0xffff_ffff` when they mean "nothing decoded this address".
//!
//! The types are `#[cfg(test)]` only: none of this exists in a kernel build.

use alloc::{collections::BTreeMap, vec::Vec};

use super::{
    IgcBus, IgcHal,
    regs::{self, Register, Speed, bits, mii},
};

/// A test HAL that records the waits the driver asked for instead of waiting.
///
/// The record is thread-local, so tests cannot see each other's waits even
/// when the test harness runs them in parallel.
pub struct FakeHal;

std::thread_local! {
    static DELAYS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl FakeHal {
    /// Forget every wait recorded on this thread.
    pub fn reset() {
        DELAYS.with(|delays| delays.borrow_mut().clear());
    }

    /// Every wait, in microseconds and in order, recorded on this thread.
    pub fn delays() -> Vec<u32> {
        DELAYS.with(|delays| delays.borrow().clone())
    }

    /// How many waits were recorded on this thread.
    pub fn delay_count() -> usize {
        DELAYS.with(|delays| delays.borrow().len())
    }

    /// The total time waited on this thread, in microseconds.
    pub fn total_us() -> u64 {
        DELAYS.with(|delays| delays.borrow().iter().map(|us| u64::from(*us)).sum())
    }
}

impl IgcHal for FakeHal {
    fn busy_wait_us(micros: u32) {
        DELAYS.with(|delays| delays.borrow_mut().push(micros));
    }
}

/// The device's side of the conversation.
///
/// Every field is a knob a test turns to make the device behave a particular
/// way; the defaults are a healthy part that comes up at 2.5 Gb/s.
#[derive(Clone, Debug)]
pub struct NicModel {
    /// What `IGC_CTRL` reads as.
    pub control: u32,
    /// `STATUS.GIO_MASTER_ENABLE`, cleared when the driver disables the GIO
    /// master.
    pub master_enabled: bool,
    /// A test's way of making `STATUS.GIO_MASTER_ENABLE` stick.
    pub master_never_stops: bool,
    /// `EECD.AUTO_RD`.
    pub auto_read_done: bool,
    /// How many reads of `IGC_EECD` it takes before the auto-read reports done.
    /// A reset starts this counter over, which is what makes the reset
    /// sequence's wait meaningful.
    pub auto_read_after_reads: u32,
    /// How many times `IGC_EECD` has been read since the last reset.
    pub eecd_reads: u32,
    /// The MII registers the PHY answers with.
    pub phy: BTreeMap<u16, u16>,
    /// How many reads of the MII status register it takes before the PHY
    /// reports link.
    pub link_up_after_polls: u32,
    /// How many times the MII status register has been read.
    pub link_polls: u32,
    /// The speed bits `STATUS` reports once link is up.
    pub speed: u32,
    /// Whether `STATUS.FD` is set once link is up.  A modern PHY negotiating
    /// anything above 100 Mb/s reports full duplex; a test can turn it off to
    /// exercise the half-duplex decode.
    pub full_duplex: bool,
    /// The MAC address in `RAL(0)`/`RAH(0)`.
    pub station: [u8; 6],
    /// A test's way of making the address read back as zero: what an NVM that
    /// was never read, or a blank one, leaves behind.
    pub address_invalid: bool,
    /// A test's way of clearing `RAH.AV` while leaving the address alone.
    pub clear_rah_av: bool,
    /// Make every `IGC_MDIC` read answer with the error bit.
    pub mdic_error: bool,
    /// Make `IGC_MDIC_READY` never set.
    pub mdic_never_ready: bool,
    /// How many reads of `IGC_MDIC` it takes before the ready bit appears.
    pub mdic_ready_after_reads: u32,
    /// How many times `IGC_MDIC` has been read since the last command.
    pub mdic_polls: u32,
    /// The last read command's register number, set by the write that started
    /// the transaction.
    pub mdic_register: u16,
    /// The word the PHY will answer the pending read with.
    pub mdic_data: u16,
    /// Set while a command is outstanding.
    pub mdic_pending: bool,
    /// Whether a write of `CTRL.RST` clears the bit, which is what the
    /// hardware does.
    pub reset_self_clears: bool,
}

impl NicModel {
    /// A healthy part with this station address, coming up at `speed`.
    ///
    /// The advertisement the model reports is the one a part that has never
    /// been reprogrammed would carry: every 10/100 mode, 1000BASE-T full
    /// duplex in register 9, and pause in register 4.
    pub fn healthy(station: [u8; 6], speed: Speed) -> Self {
        let mut phy = BTreeMap::new();
        phy.insert(mii::ID1, 0x0000);
        phy.insert(mii::ID2, 0x0000);
        phy.insert(
            mii::AUTONEG_ADV,
            0x01e0 | bits::NWAY_AR_PAUSE | bits::NWAY_AR_ASM_DIR,
        );
        phy.insert(mii::LP_ABILITY, 0x01e0 | bits::NWAY_LPAR_PAUSE);
        phy.insert(mii::CTRL_1000T, bits::CR_1000T_FD_CAPS);
        phy.insert(mii::STATUS_1000T, bits::SR_1000T_REMOTE_RX_STATUS);
        let speed_bits = match speed {
            Speed::Mbit10 => 0,
            Speed::Mbit100 => bits::STATUS_SPEED_100,
            Speed::Mbit1000 => bits::STATUS_SPEED_1000,
            Speed::Mbit2500 => bits::STATUS_SPEED_1000 | bits::STATUS_SPEED_2500,
        };
        Self {
            control: 0,
            master_enabled: true,
            master_never_stops: false,
            auto_read_done: false,
            auto_read_after_reads: 1,
            eecd_reads: 0,
            phy,
            link_up_after_polls: 1,
            link_polls: 0,
            speed: speed_bits,
            full_duplex: true,
            station,
            address_invalid: false,
            clear_rah_av: false,
            mdic_error: false,
            mdic_never_ready: false,
            mdic_ready_after_reads: 0,
            mdic_polls: 0,
            mdic_register: 0,
            mdic_data: 0,
            mdic_pending: false,
            reset_self_clears: true,
        }
    }

    /// A part with a blank NVM: no station address at all.
    pub fn blank_nvm() -> Self {
        let mut model = Self::healthy([0; 6], Speed::Mbit10);
        model.address_invalid = true;
        model
    }

    /// The MII status word the PHY answers with right now.
    ///
    /// Link appears once the model has been asked `link_up_after_polls` times.
    fn mii_status(&self) -> u16 {
        let mut word = bits::MII_SR_AUTONEG_COMPLETE;
        if self.link_polls >= self.link_up_after_polls {
            word |= bits::MII_SR_LINK_STATUS;
        }
        word
    }

    /// What `IGC_STATUS` reads as right now.
    fn status(&self) -> u32 {
        let mut status = self.speed;
        if self.link_polls >= self.link_up_after_polls {
            status |= bits::STATUS_LU;
            if self.full_duplex {
                status |= bits::STATUS_FD;
            }
        }
        if self.master_enabled {
            status |= bits::STATUS_GIO_MASTER_ENABLE;
        }
        status
    }

    /// Apply a write the driver performed, the way the hardware would.
    fn on_write(&mut self, offset: u32, value: u32) {
        let control = regs::named("IGC_CTRL").expect("named").offset();
        if offset == control {
            self.control = value;
            if value & bits::CTRL_GIO_MASTER_DISABLE != 0 && !self.master_never_stops {
                self.master_enabled = false;
            }
            if value & bits::CTRL_RST != 0 && self.reset_self_clears {
                // The hardware clears the reset bit itself and starts the NVM
                // auto-read over.
                self.control &= !bits::CTRL_RST;
                self.auto_read_done = false;
                self.eecd_reads = 0;
                self.auto_read_after_reads = self.auto_read_after_reads.max(1);
            }
        }
        if offset == regs::named("IGC_MDIC").expect("named").offset() {
            // A read command starts a transaction the PHY completes.  A write
            // command is something this driver must never issue, and the model
            // makes it visible by answering with the error bit.
            self.mdic_polls = 0;
            self.mdic_pending = true;
            let is_read = value & bits::MDIC_OP_READ != 0;
            self.mdic_register = ((value & bits::MDIC_REG_MASK) >> bits::MDIC_REG_SHIFT) as u16;
            if !is_read {
                self.mdic_error = true;
            }
            self.mdic_data = self.phy.get(&self.mdic_register).copied().unwrap_or(0);
        }
    }

    /// Answer a read, the way the hardware would.
    fn on_read(&mut self, offset: u32, stored: u32) -> u32 {
        if offset == regs::named("IGC_CTRL").expect("named").offset() {
            // The control register is state the device holds, so a read sees
            // the model's copy -- including a reset bit the hardware has
            // already cleared itself.
            return self.control;
        }
        if offset == regs::named("IGC_STATUS").expect("named").offset() {
            return self.status();
        }
        if offset == regs::named("IGC_EECD").expect("named").offset() {
            // The auto-read takes time: `auto_read_after_reads` counts polls of
            // this register, and the value is formed after the poll that makes
            // it true.
            self.eecd_reads = self.eecd_reads.saturating_add(1);
            if self.eecd_reads >= self.auto_read_after_reads {
                self.auto_read_done = true;
            }
            return if self.auto_read_done {
                bits::EECD_AUTO_RD | bits::EECD_FLASH_DETECTED_I225 | (5 << 11)
            } else {
                bits::EECD_FLASH_DETECTED_I225
            };
        }
        if offset == regs::named("IGC_MDIC").expect("named").offset() {
            if self.mdic_never_ready {
                return 0;
            }
            if self.mdic_error {
                // An error *response*: the transaction completed and failed,
                // which is what the vendor driver distinguishes from a
                // transaction that never completed.
                return bits::MDIC_READY | bits::MDIC_ERROR | u32::from(self.mdic_data);
            }
            self.mdic_polls = self.mdic_polls.saturating_add(1);
            if self.mdic_polls <= self.mdic_ready_after_reads {
                // Still working: ready is clear and the data field is stale.
                return u32::from(self.mdic_data);
            }
            if self.mdic_pending && self.mdic_register == mii::STATUS {
                // The PHY status register is generated, not stored: it is the
                // one register whose contents change as the cable comes up.
                // The poll is counted first, so `link_up_after_polls` is the
                // number of *reads* at which link appears.
                self.link_polls = self.link_polls.saturating_add(1);
                let status = self.mii_status();
                return bits::MDIC_READY | u32::from(status);
            }
            return bits::MDIC_READY | u32::from(self.mdic_data);
        }
        if offset == regs::named("IGC_RAL(0)").expect("named").offset() {
            if self.address_invalid {
                return 0;
            }
            return u32::from_le_bytes([
                self.station[0],
                self.station[1],
                self.station[2],
                self.station[3],
            ]);
        }
        if offset == regs::named("IGC_RAH(0)").expect("named").offset() {
            if self.address_invalid {
                return 0;
            }
            let high = u32::from(self.station[4]) | (u32::from(self.station[5]) << 8);
            return if self.clear_rah_av {
                high
            } else {
                high | bits::RAH_AV
            };
        }
        stored
    }
}

/// A register file standing in for the device aperture.
pub struct FakeBus {
    words: BTreeMap<u32, u32>,
    /// What an unseeded register reads as.  `0xffff_ffff` is what an address
    /// no device decodes returns on x86; tests use it to mean that.
    unseeded: u32,
    /// Every write the bus was asked to perform, in order, as
    /// `(offset, value, accepted)`.
    ///
    /// The identify-only phase must produce none of these, which is a property
    /// a test can assert instead of trusting the register table alone.
    writes: Vec<(u32, u32, bool)>,
    /// Every wait, in order.
    delays: Vec<u32>,
    /// The device's other side, when a test wants one.
    model: Option<NicModel>,
}

impl FakeBus {
    /// A register file whose unseeded registers read as zero, with no device
    /// behind it.
    pub fn new() -> Self {
        Self {
            words: BTreeMap::new(),
            unseeded: 0,
            writes: Vec::new(),
            delays: Vec::new(),
            model: None,
        }
    }

    /// A register file with a device behind it.
    pub fn with_model(model: NicModel) -> Self {
        Self {
            model: Some(model),
            ..Self::new()
        }
    }

    /// A register file whose unseeded registers read as `0xffff_ffff`: the
    /// value a read of an address no device decodes returns on x86.
    pub fn undecoded() -> Self {
        Self::new().with_unseeded(0xffff_ffff)
    }

    /// Set what an unseeded register reads as.
    pub fn with_unseeded(mut self, value: u32) -> Self {
        self.unseeded = value;
        self
    }

    /// Seed registers by their Linux names.
    ///
    /// # Panics
    ///
    /// If a name is not in the register table: a seed for a register the
    /// driver cannot name would be a test that proves nothing.
    pub fn with(mut self, values: &[(&str, u32)]) -> Self {
        for (name, value) in values {
            let register = regs::named(name).unwrap_or_else(|| panic!("{name} is not a register"));
            self.words.insert(register.offset(), *value);
        }
        self
    }

    /// Read a register's current contents, for a test's own inspection.
    pub fn peek(&self, name: &str) -> u32 {
        let register = regs::named(name).unwrap_or_else(|| panic!("{name} is not a register"));
        self.words
            .get(&register.offset())
            .copied()
            .unwrap_or(self.unseeded)
    }

    /// Poke a register's contents behind the driver's back.
    pub fn poke(&mut self, name: &str, value: u32) {
        let register = regs::named(name).unwrap_or_else(|| panic!("{name} is not a register"));
        self.words.insert(register.offset(), value);
    }

    /// The device behind this bus, for a test that wants to inspect it.
    pub fn model(&self) -> Option<&NicModel> {
        self.model.as_ref()
    }

    /// Every write the bus was asked to perform, accepted or refused.
    pub fn writes(&self) -> &[(u32, u32, bool)] {
        &self.writes
    }

    /// The writes that were accepted, as `(offset, value)`.
    pub fn accepted_writes(&self) -> Vec<(u32, u32)> {
        self.writes
            .iter()
            .filter(|(_, _, accepted)| *accepted)
            .map(|(offset, value, _)| (*offset, *value))
            .collect()
    }

    /// Every wait, in order.
    pub fn delays(&self) -> &[u32] {
        &self.delays
    }
}

impl IgcBus for FakeBus {
    fn read(&mut self, register: Register) -> Option<u32> {
        let offset = register.offset();
        let stored = self.words.get(&offset).copied().unwrap_or(self.unseeded);
        match self.model.as_mut() {
            Some(model) => Some(model.on_read(offset, stored)),
            None => Some(stored),
        }
    }

    fn write(&mut self, register: Register, value: u32) -> bool {
        let accepted = register.is_writable();
        self.writes.push((register.offset(), value, accepted));
        if accepted {
            self.words.insert(register.offset(), value);
            if let Some(model) = self.model.as_mut() {
                model.on_write(register.offset(), value);
            }
        }
        accepted
    }

    fn delay_us(&mut self, micros: u32) {
        self.delays.push(micros);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_is_by_linux_name_and_unseeded_reads_use_the_chosen_value() {
        let bus = FakeBus::new().with(&[("IGC_STATUS", 0x83)]);
        assert_eq!(bus.peek("IGC_STATUS"), 0x83);
        assert_eq!(bus.peek("IGC_CTRL"), 0);

        let undecoded = FakeBus::undecoded();
        assert_eq!(undecoded.peek("IGC_STATUS"), 0xffff_ffff);

        // `poke` changes the register file behind the driver's back, which is
        // how a test models the device changing state on its own -- a reset
        // completing, or a link coming up between two polls.
        let mut bus = FakeBus::new();
        bus.poke("IGC_STATUS", 0x0000_0002);
        assert_eq!(bus.peek("IGC_STATUS"), 0x0000_0002);
        assert!(bus.writes().is_empty(), "a poke is not a driver write");
    }

    #[test]
    fn a_write_to_a_read_only_register_is_recorded_as_refused() {
        let mut bus = FakeBus::new();
        assert!(!bus.write(regs::named("IGC_STATUS").unwrap(), 7));
        assert_eq!(bus.writes(), &[(0x00008, 7, false)]);
        assert!(bus.accepted_writes().is_empty());
        assert_eq!(bus.peek("IGC_STATUS"), 0);
        // A writable register is accepted and recorded as accepted, so a test
        // can tell the two apart.
        assert!(bus.write(regs::named("IGC_CTRL").unwrap(), 7));
        assert_eq!(bus.accepted_writes(), alloc::vec![(0x00000, 7)]);
    }

    #[test]
    fn the_hal_records_waits_per_thread() {
        FakeHal::reset();
        FakeHal::busy_wait_us(10);
        FakeHal::busy_wait_us(25);
        assert_eq!(FakeHal::delays(), alloc::vec![10, 25]);
        assert_eq!(FakeHal::delay_count(), 2);
        assert_eq!(FakeHal::total_us(), 35);
    }

    #[test]
    fn the_model_starts_healthy_and_unknown_registers_fall_through() {
        let mut bus =
            FakeBus::with_model(NicModel::healthy([0, 1, 2, 3, 4, 5], Speed::Mbit2500));
        // Status is generated: no link until the PHY has been asked, and the
        // GIO master starts enabled.
        assert_eq!(
            bus.read(regs::named("IGC_STATUS").unwrap()).unwrap() & bits::STATUS_LU,
            0
        );
        assert_ne!(
            bus.read(regs::named("IGC_STATUS").unwrap()).unwrap()
                & bits::STATUS_GIO_MASTER_ENABLE,
            0,
        );
        // A register the model does not model reads as the file holds it.
        assert_eq!(bus.read(regs::named("IGC_RCTL").unwrap()), Some(0));
        assert!(bus.model().is_some());
    }

    #[test]
    fn the_model_clears_the_reset_bit_and_restarts_the_auto_read() {
        let mut model = NicModel::healthy([0x02; 6], Speed::Mbit1000);
        // Give the auto-read room to be visible: it completes on the third
        // poll of `IGC_EECD`.
        model.auto_read_after_reads = 3;
        let mut bus = FakeBus::with_model(model);
        for expected in [0, 0, bits::EECD_AUTO_RD] {
            let eecd = bus.read(regs::named("IGC_EECD").unwrap()).unwrap();
            assert_eq!(eecd & bits::EECD_AUTO_RD, expected);
        }
        bus.write(regs::named("IGC_CTRL").unwrap(), bits::CTRL_RST);
        assert_eq!(
            bus.read(regs::named("IGC_CTRL").unwrap()).unwrap() & bits::CTRL_RST,
            0,
            "the hardware clears the reset bit itself",
        );
        // A reset starts the auto-read over, which is why the reset sequence
        // waits for it before it reads the station address.
        let eecd = bus.read(regs::named("IGC_EECD").unwrap()).unwrap();
        assert_eq!(eecd & bits::EECD_AUTO_RD, 0);
    }

    #[test]
    fn the_model_answers_a_phy_read_from_its_register_file() {
        let mut bus = FakeBus::with_model(NicModel::healthy([0x02; 6], Speed::Mbit1000));
        let command =
            regs::MdicCommand::read(regs::MDIC_PHY_ADDRESS, mii::AUTONEG_ADV)
                .unwrap()
                .raw();
        assert!(bus.write(regs::named("IGC_MDIC").unwrap(), command));
        let result = regs::MdicResult::new(bus.read(regs::named("IGC_MDIC").unwrap()).unwrap());
        assert!(result.ready());
        assert!(!result.error());
        assert_eq!(result.data(), bus.model().unwrap().phy[&mii::AUTONEG_ADV]);
    }

    #[test]
    fn the_model_treats_an_mdic_write_command_as_an_error() {
        // This driver never writes a PHY register; the model makes such a
        // write visible as a failure rather than silently accepting it.
        let mut bus = FakeBus::with_model(NicModel::healthy([0x02; 6], Speed::Mbit1000));
        let command =
            regs::MdicCommand::write(regs::MDIC_PHY_ADDRESS, mii::AUTONEG_ADV, 1)
                .unwrap()
                .raw();
        assert!(bus.write(regs::named("IGC_MDIC").unwrap(), command));
        let result = regs::MdicResult::new(bus.read(regs::named("IGC_MDIC").unwrap()).unwrap());
        assert!(result.error());
    }

    #[test]
    fn the_model_reports_link_only_after_the_configured_number_of_polls() {
        let mut model = NicModel::healthy([0x02; 6], Speed::Mbit2500);
        model.link_up_after_polls = 2;
        let mut bus = FakeBus::with_model(model);
        let command = regs::MdicCommand::read(regs::MDIC_PHY_ADDRESS, mii::STATUS)
            .unwrap()
            .raw();
        for expected in [0, bits::MII_SR_LINK_STATUS] {
            bus.write(regs::named("IGC_MDIC").unwrap(), command);
            let result = regs::MdicResult::new(bus.read(regs::named("IGC_MDIC").unwrap()).unwrap());
            assert_eq!(result.data() & bits::MII_SR_LINK_STATUS, expected);
        }
        assert_ne!(
            bus.read(regs::named("IGC_STATUS").unwrap()).unwrap() & bits::STATUS_LU,
            0,
        );
    }
}
