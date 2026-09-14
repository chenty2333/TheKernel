//! Bringing the part up: reset it, read its station address, wait for link.
//!
//! This is the phase the brief calls "where the interesting hardware behaviour
//! lives", and it is deliberately packets-free: nothing here sets up a
//! descriptor ring, so the driver cannot send or receive yet.  What it does is
//! the part of a NIC driver that is a *conversation* -- write a control bit,
//! poll a status bit, wait for the PHY to finish autonegotiating -- and a
//! conversation is exactly the kind of thing that can be tested on a machine
//! that has no such NIC, provided the device's side of it is written down.
//!
//! # The sequence, and where each step comes from
//!
//! Every step cites the vendor function it was taken from.  The order is the
//! vendor's for the reset and the link; the one step whose *position* is this
//! driver's own choice is the packet-buffer split, which Linux writes once at
//! probe time (`igc_main.c:7097-7098`) and this driver writes as part of its
//! reset sequence:
//!
//! 1. `igc_mac.c:21` `igc_disable_pcie_master` -- assert
//!    `CTRL.GIO_MASTER_DISABLE` and poll `STATUS.GIO_MASTER_ENABLE` clear, so
//!    the MAC is not mid-transaction when it is reset.
//! 2. `igc_base.c:19` `igc_reset_hw_base` -- mask every interrupt (`IGC_IMC`),
//!    stop the receive and transmit paths (`IGC_RCTL` = 0, `IGC_TCTL` =
//!    `PSP`), wait, assert `CTRL.RST`.
//! 3. `igc_mac.c:650` `igc_get_auto_rd_done` -- poll `EECD.AUTO_RD`, which is
//!    how the hardware says it has finished copying the NVM into the
//!    receive-address registers.  The station address is only readable after
//!    this.
//! 4. `igc_base.c:54-56` -- mask interrupts again and read `IGC_ICR` to clear
//!    the causes the reset raised.
//! 5. `igc_main.c:7096-7098` -- program the packet-buffer split.
//! 6. `igc_nvm.c:133` `igc_read_mac_addr` -- read `IGC_RAL(0)`/`IGC_RAH(0)`
//!    and assemble the station address.
//! 7. `igc_base.c:112-115` `igc_setup_copper_link_base` -- set `CTRL.SLU` and clear
//!    the speed/duplex force bits, so the PHY autonegotiates.
//! 8. `igc_phy.c:64` `igc_phy_has_link` and `igc_mac.c:681`
//!    `igc_get_speed_and_duplex_copper` -- poll the PHY's MII status register
//!    over `IGC_MDIC` for link, then read speed and duplex out of
//!    `IGC_STATUS`.
//!
//! # Where this driver departs from the vendor driver, and why
//!
//! * **The PHY's advertisement registers are not programmed.**  Linux's
//!   `igc_phy_setup_autoneg` (`igc_phy.c:216`) clears and rewrites MII
//!   registers 4 and 9 and the 2.5 Gb/s bit in the MMD register 7.32, so the
//!   PHY advertises exactly the speeds Linux wants.  This driver writes no PHY
//!   register at all: it reads what the PHY is advertising and reports it, and
//!   relies on the advertisement the NVM/firmware left behind.  The
//!   consequence is stated in the design note and is the most likely reason a
//!   link would come up slower than 2.5 Gb/s on the target machine.
//! * **The link wait is longer than the vendor's.**  `igc_setup_copper_link`
//!   polls ten times ten microseconds (`igc_phy.c:492`, `igc_defines.h:91`) --
//!   a check, not a wait, because Linux's link watch runs later from a work
//!   queue.  A driver with no interrupt and no work queue has to wait inline,
//!   so [`LINK_POLL_BUDGET`] is 3 seconds and is a choice, not a vendor fact.
//! * **Nothing about flow control is programmed.**  `igc_setup_link`
//!   initialises the pause-frame registers and `CTRL.RFCE`/`CTRL.TFCE`; this
//!   driver sets neither, so a link that negotiates pause frames will not have
//!   them honoured by the MAC.
//! * **The receive filter is not rewritten.**  `igc_init_rx_addrs` writes the
//!   station address into `RAL(0)`/`RAH(0)` and clears the other fifteen
//!   entries.  This driver only reads entry 0, so it depends on the hardware's
//!   own auto-read having armed it -- which is why [`StationAddress`] carries
//!   the `RAH.AV` bit and the report prints it.

use alloc::{format, string::String, vec::Vec};

use super::{
    IgcBus,
    probe::is_valid_unicast,
    regs::{
        self, DeviceControl, DeviceStatus, MdicCommand, MdicResult, NvmControl, ReceiveAddressHigh,
        Register, Speed, assemble_receive_address, bits, mii,
    },
};

/// How long to wait between `STATUS.GIO_MASTER_ENABLE` polls.
///
/// The vendor's `usleep_range(2000, 3000)` (`igc_mac.c:35`) is a range; this
/// driver waits the lower bound, which is the one the poll count is a bound on.
pub const MASTER_DISABLE_POLL_US: u32 = 2_000;

/// How long the MAC is given to settle after its queues are stopped, before
/// the reset is asserted.  `usleep_range(10000, 20000)` at `igc_base.c:38`;
/// again the lower bound.
pub const RESET_SETTLE_US: u32 = 10_000;

/// How long to wait between `EECD.AUTO_RD` polls.
/// `usleep_range(1000, 2000)` at `igc_mac.c:658`, lower bound.
pub const AUTO_READ_POLL_US: u32 = 1_000;

/// How long to wait before each `IGC_MDIC` poll.
///
/// `udelay(50)` at `igc_phy.c:571`, and note the order in the vendor loop: the
/// delay comes *before* the read, so the first read is 50 microseconds after
/// the command was written.
pub const MDIC_POLL_US: u32 = 50;

/// How long to wait between PHY link polls.
///
/// **Not from the vendor driver**: `igc_phy_has_link` is called with a 10
/// microsecond interval (`igc_phy.c:519`), because Linux re-checks link later
/// from its own work queue.  A driver that must know now has to wait, and 10 ms
/// is a compromise between autonegotiation settling in tens of milliseconds on
/// a good cable and not spending seconds on a dead one.
pub const LINK_POLL_US: u32 = 10_000;

/// How many times the PHY is asked for link before the wait is called a
/// failure: 300 polls, each followed by [`LINK_POLL_US`] except the last, so
/// just under three seconds.
pub const LINK_POLL_BUDGET: u32 = 300;

/// Why bringing the part up failed.
///
/// Every variant says what was being attempted and what came back, because on
/// the target machine a log line is the only diagnostic there is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BringUpError {
    /// A register the sequence needs was not readable through the window.
    RegisterNotReadable(&'static str),
    /// A register the sequence needs was not writable through the window.
    ///
    /// This is the guard that makes "the table is the only thing that decides
    /// what may be written" true at run time and not only in review: if a
    /// register's access is narrowed, the sequence that uses it fails here
    /// instead of silently skipping a write and producing a link that never
    /// comes up.
    RegisterNotWritable(&'static str),
    /// The PHY did not answer the command within the vendor's poll bound.
    PhyTimeout { register: u16, polls: u32 },
    /// The PHY answered with its error bit set.
    PhyError { register: u16 },
    /// The station address in `RAL(0)`/`RAH(0)` is not a usable unicast
    /// address, which is what a blank or unread NVM looks like.
    InvalidStationAddress([u8; 6]),
    /// The PHY never reported link within the budget.
    NoLink { polls: u32, last_phy_status: u16 },
}

impl BringUpError {
    /// The sentence the report and the log use.
    pub fn describe(&self) -> String {
        match self {
            Self::RegisterNotReadable(name) => {
                format!("{name} is not readable through the mapped window")
            }
            Self::RegisterNotWritable(name) => {
                format!("{name} is not writable through the mapped window")
            }
            Self::PhyTimeout { register, polls } => format!(
                "the PHY did not answer a read of register {register:#04x} in {polls} polls of \
                 {MDIC_POLL_US} us"
            ),
            Self::PhyError { register } => {
                format!("the PHY reported an error reading register {register:#04x}")
            }
            Self::InvalidStationAddress(address) => format!(
                "the receive-address registers hold {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, \
                 which is not a usable unicast address; a blank or unread NVM looks like this",
                address[0], address[1], address[2], address[3], address[4], address[5],
            ),
            Self::NoLink {
                polls,
                last_phy_status,
            } => format!(
                "the PHY reported no link in {polls} polls of {LINK_POLL_US} us; its last status \
                 word was {last_phy_status:#06x}"
            ),
        }
    }
}

/// The station address, and the evidence for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StationAddress {
    /// The six bytes, assembled the way `igc_read_mac_addr` assembles them.
    pub bytes: [u8; 6],
    /// The raw `IGC_RAL(0)` value.
    pub low: u32,
    /// The raw `IGC_RAH(0)` value.
    pub high: u32,
    /// `IGC_RAH_AV`, the bit that says the receive filter entry is armed.
    ///
    /// Linux does not require it (`igc_main.c:7090` only checks
    /// `is_valid_ether_addr`), and neither does this driver, but a receive
    /// filter entry with this bit clear will not accept the machine's own
    /// unicast frames, so the report prints it rather than assuming.
    pub address_valid: bool,
}

impl StationAddress {
    /// How the report spells the address.
    pub fn describe(&self) -> String {
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.bytes[0],
            self.bytes[1],
            self.bytes[2],
            self.bytes[3],
            self.bytes[4],
            self.bytes[5],
        )
    }
}

/// What the reset sequence observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResetOutcome {
    /// How many polls it took for `STATUS.GIO_MASTER_ENABLE` to clear.
    ///
    /// `None` means it never did, which the vendor driver treats as a
    /// diagnostic and not a failure (`igc_base.c:27-29`), so this driver does
    /// too -- and reports it, because a master that will not stop is worth
    /// seeing on the target machine's log.
    pub master_disable_polls: Option<u32>,
    /// How many polls it took for `EECD.AUTO_RD` to set.
    ///
    /// `None` means it never did.  The vendor driver also continues here
    /// (`igc_base.c:45-52`), and so does this one: the consequence is that the
    /// station address read next will be invalid, which fails the bring-up
    /// with a reason that names the real problem.
    pub auto_read_polls: Option<u32>,
    /// `CTRL.RST` read back clear after the auto-read wait.
    pub reset_bit_cleared: bool,
}

/// What the link wait observed.
///
/// Four MII registers, and the *register* each bit lives in matters: an
/// earlier version of this code read the 1000BASE-T full-duplex bit
/// (`CR_1000T_FD_CAPS`, `igc_defines.h:174`) out of the 10/100 advertisement
/// register, where `0x0100` means something else entirely
/// (`NWAY_AR_100TX_FD_CAPS`, `igc_defines.h:163-164`).  Each accessor below
/// names the register it reads, and a test pins the difference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkOutcome {
    /// How many times the PHY was asked for link, including the successful
    /// one.
    pub polls: u32,
    /// The MII status word of the successful poll (register 1).
    pub phy_status: u16,
    /// `MII_SR_AUTONEG_COMPLETE` from that word.
    pub autoneg_complete: bool,
    /// MII register 4, `PHY_AUTONEG_ADV`: what this PHY advertises for 10 and
    /// 100 Mb/s, and its pause bits.
    pub advertisement: Option<u16>,
    /// MII register 9, `PHY_1000T_CTRL`: the 1000BASE-T advertisement.
    pub gigabit_control: Option<u16>,
    /// MII register 5, `PHY_LP_ABILITY`: what the link partner advertises.
    pub partner: Option<u16>,
    /// MII register 10, `PHY_1000T_STATUS`: the partner's 1000BASE-T status.
    pub gigabit_status: Option<u16>,
    /// The MAC's own `IGC_STATUS` at the moment link was declared up.
    pub status: DeviceStatus,
}

impl LinkOutcome {
    /// The speed the MAC reports.
    pub fn speed(&self) -> Speed {
        self.status.speed()
    }

    /// Whether the MAC reports full duplex.
    pub fn full_duplex(&self) -> bool {
        self.status.full_duplex()
    }

    /// Whether the PHY advertises 100 Mb/s full duplex, from MII register 4
    /// (`NWAY_AR_100TX_FD_CAPS`, `igc_defines.h:163`).
    pub fn advertises_100_full(&self) -> Option<bool> {
        self.advertisement
            .map(|word| word & bits::NWAY_AR_100TX_FD_CAPS != 0)
    }

    /// Whether the PHY advertises 1000BASE-T full duplex, from MII register 9
    /// (`CR_1000T_FD_CAPS`, `igc_defines.h:174` — *not* the register the 10/100
    /// bits live in).
    ///
    /// This is a read of the advertisement, not a statement about the
    /// negotiated link, and it says nothing about 2.5 Gb/s: that lives in MMD
    /// register 7.32, which this driver does not reach
    /// (`igc_phy.c:240-244` reads it and `:379-383` writes it).  So a link at
    /// 1000 Mb/s and an advertisement that does not mention 2.5 Gb/s are both
    /// consistent with what is printed here, and a link at 2500 Mb/s is
    /// perfectly possible with `advertises_gigabit_full()` false.
    pub fn advertises_gigabit_full(&self) -> Option<bool> {
        self.gigabit_control
            .map(|word| word & bits::CR_1000T_FD_CAPS != 0)
    }

    /// Whether the link partner reports its receiver is ready, from MII
    /// register 10 (`SR_1000T_REMOTE_RX_STATUS`, `igc_defines.h:177`).
    ///
    /// An earlier version of this answered "true" whenever the *register 5*
    /// read had succeeded, which is not the same claim at all.
    pub fn partner_ready(&self) -> Option<bool> {
        self.gigabit_status
            .map(|word| word & bits::SR_1000T_REMOTE_RX_STATUS != 0)
    }

    /// Whether the link partner advertises pause frames, from MII register 5
    /// (`NWAY_LPAR_PAUSE`, `igc_defines.h:169`).
    pub fn partner_advertises_pause(&self) -> Option<bool> {
        self.partner.map(|word| word & bits::NWAY_LPAR_PAUSE != 0)
    }

    /// How the report spells the negotiated link.
    pub fn describe(&self) -> String {
        format!(
            "link up, {}, {}, PHY status {:#06x}{}, after {} poll{}",
            self.status.speed().describe(),
            if self.status.full_duplex() {
                "full duplex"
            } else {
                "half duplex"
            },
            self.phy_status,
            if self.autoneg_complete {
                ", autonegotiation complete"
            } else {
                ", autonegotiation NOT reported complete"
            },
            self.polls,
            if self.polls == 1 { "" } else { "s" },
        )
    }

    /// Decode MII register 9 (`PHY_1000T_CTRL`) into the words a reader wants.
    ///
    /// `CR_1000T_HD_CAPS` and `CR_1000T_FD_CAPS` (`igc_defines.h:173-174`);
    /// 1000 Mb/s half duplex is a mode the vendor driver refuses to advertise
    /// (`igc_phy.c`, "Advertise 1000mb Half duplex request denied"), so a set
    /// half-duplex bit is reported rather than hidden.
    pub fn describe_gigabit_control(word: u16) -> String {
        let mut modes = Vec::new();
        if word & bits::CR_1000T_FD_CAPS != 0 {
            modes.push("1000FD");
        }
        if word & bits::CR_1000T_HD_CAPS != 0 {
            modes.push("1000HD");
        }
        if modes.is_empty() {
            String::from("no 1000BASE-T mode advertised")
        } else {
            modes.join(" ")
        }
    }

    /// Decode MII register 10 (`PHY_1000T_STATUS`).
    pub fn describe_gigabit_status(word: u16) -> String {
        if word & bits::SR_1000T_REMOTE_RX_STATUS != 0 {
            String::from("the partner's receiver is ready")
        } else {
            String::from("the partner does not report its receiver ready")
        }
    }

    /// Decode MII register 4 (`PHY_AUTONEG_ADV`) into the words a reader
    /// wants: which 10/100 modes are advertised, and the pause bits.
    pub fn describe_advertisement(word: u16) -> String {
        let mut parts = Vec::new();
        parts.push(if word & 0x0020 != 0 { "10HD" } else { "" });
        parts.push(if word & 0x0040 != 0 { "10FD" } else { "" });
        parts.push(if word & 0x0080 != 0 { "100HD" } else { "" });
        parts.push(if word & 0x0100 != 0 { "100FD" } else { "" });
        let speeds: Vec<&str> = parts.into_iter().filter(|part| !part.is_empty()).collect();
        // The 1000BASE-T advertisement lives in MII register 9, not here, so
        // this decode says nothing about it and the report prints the raw
        // words for a reader who has the register map open.
        format!(
            "{}; pause {}, asymmetric pause {}",
            if speeds.is_empty() {
                String::from("no 10/100 mode advertised")
            } else {
                speeds.join(" ")
            },
            if word & bits::NWAY_AR_PAUSE != 0 {
                "advertised"
            } else {
                "not advertised"
            },
            if word & bits::NWAY_AR_ASM_DIR != 0 {
                "advertised"
            } else {
                "not advertised"
            },
        )
    }
}

/// Everything the bring-up did, in the order it did it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BringUp {
    /// The registers written, in order.
    pub written: Vec<&'static str>,
    /// What the reset observed.
    pub reset: ResetOutcome,
    /// The station address.
    pub station: StationAddress,
    /// What the link wait observed.
    pub link: LinkOutcome,
}

impl BringUp {
    /// The report as text, every line prefixed so the whole thing is one grep.
    pub fn render(&self, bdf: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{}: bring-up {bdf}: reset done ({}, {}, CTRL.RST {})\n",
            super::probe::PREFIX,
            match self.reset.master_disable_polls {
                Some(polls) => format!("GIO master stopped after {polls} polls"),
                None => String::from(
                    "GIO master never stopped (reported, not fatal, as the vendor driver treats \
                     it)"
                ),
            },
            match self.reset.auto_read_polls {
                Some(polls) => format!("NVM auto-read done after {polls} polls"),
                None => String::from("NVM auto-read never reported done"),
            },
            if self.reset.reset_bit_cleared {
                "cleared"
            } else {
                "STILL SET"
            },
        ));
        out.push_str(&format!(
            "{}:   station address {} (RAH.AV {}{})\n",
            super::probe::PREFIX,
            self.station.describe(),
            u8::from(self.station.address_valid),
            if self.station.address_valid {
                ""
            } else {
                ", so the receive filter entry is NOT armed: unicast frames addressed to this \
                 address will not be accepted"
            },
        ));
        out.push_str(&format!(
            "{}:   {}\n",
            super::probe::PREFIX,
            self.link.describe(),
        ));
        if let Some(word) = self.link.advertisement {
            out.push_str(&format!(
                "{}:   PHY advertisement {word:#06x} (10/100 and pause: {}){}\n",
                super::probe::PREFIX,
                LinkOutcome::describe_advertisement(word),
                match self.link.gigabit_control {
                    Some(control) => format!(
                        "; 1000BASE-T control {control:#06x} ({})",
                        LinkOutcome::describe_gigabit_control(control),
                    ),
                    None => String::from("; 1000BASE-T control register not read"),
                },
            ));
            out.push_str(&format!(
                "{}:   link partner {:#06x}{}; 2.5 Gb/s is advertised in MMD register 7.32, which \
                 this driver does not read, so the advertisement cannot confirm or deny it\n",
                super::probe::PREFIX,
                self.link.partner.unwrap_or(0),
                match self.link.gigabit_status {
                    Some(status) => format!(
                        " (1000BASE-T status {status:#06x}: {})",
                        LinkOutcome::describe_gigabit_status(status),
                    ),
                    None => String::from(", 1000BASE-T status register not read"),
                },
            ));
        }
        out.push_str(&format!(
            "{}:   wrote {} registers in order: {}\n",
            super::probe::PREFIX,
            self.written.len(),
            self.written.join(", "),
        ));
        out.push_str(&format!(
            "{}: this phase programs the MAC and reads the PHY; it sets up no descriptor ring, so \
             no frame can be sent or received yet, and no PHY register was written\n",
            super::probe::PREFIX,
        ));
        out
    }
}

/// The sequence's own progress record: which registers it has written.
///
/// It exists so the report can print them and so a test can assert the
/// sequence writes exactly what it claims to.
struct Journal {
    written: Vec<&'static str>,
}

impl Journal {
    fn new() -> Self {
        Self {
            written: Vec::new(),
        }
    }

    /// Write a register, or fail with the reason it could not be written.
    ///
    /// Every write in this module goes through here, which is what makes the
    /// journalled list and the device's actual writes the same thing.
    fn write<B: IgcBus>(
        &mut self,
        bus: &mut B,
        register: Register,
        value: u32,
    ) -> Result<(), BringUpError> {
        if !register.is_writable() {
            return Err(BringUpError::RegisterNotWritable(register.name()));
        }
        if !bus.write(register, value) {
            return Err(BringUpError::RegisterNotWritable(register.name()));
        }
        self.written.push(register.name());
        Ok(())
    }
}

/// Read a register through the bus, or fail naming it.
fn read<B: IgcBus>(bus: &mut B, register: Register) -> Result<u32, BringUpError> {
    bus.read(register)
        .ok_or(BringUpError::RegisterNotReadable(register.name()))
}

/// Stop the MAC's DMA engine before resetting it (`igc_mac.c:21`).
///
/// Returns how many polls it took, or `None` if the bit never cleared.  The
/// vendor driver treats that as a diagnostic and continues, and so does this
/// one; the caller reports it.
pub(super) fn disable_pcie_master<B: IgcBus>(bus: &mut B) -> Result<Option<u32>, BringUpError> {
    let control = DeviceControl::new(read(bus, regs_ctl())?);
    if !bus.write(regs_ctl(), control.with_master_disabled().raw()) {
        return Err(BringUpError::RegisterNotWritable(regs_ctl().name()));
    }
    for poll in 1..=bits::MASTER_DISABLE_TIMEOUT {
        let status = DeviceStatus::new(read(bus, regs_status())?);
        if !status.master_enabled() {
            return Ok(Some(poll));
        }
        bus.delay_us(MASTER_DISABLE_POLL_US);
    }
    Ok(None)
}

/// `IGC_CTRL`, by name, resolved once.
fn regs_ctl() -> Register {
    regs::named("IGC_CTRL").expect("IGC_CTRL is in the register table")
}

/// `IGC_STATUS`, by name.
fn regs_status() -> Register {
    regs::named("IGC_STATUS").expect("IGC_STATUS is in the register table")
}

/// Reset the MAC (`igc_base.c:19` and `igc_mac.c:650`).
pub fn reset<B: IgcBus>(bus: &mut B) -> Result<(ResetOutcome, Vec<&'static str>), BringUpError> {
    let mut journal = Journal::new();

    let master_disable_polls = disable_pcie_master(bus)?;
    journal.written.push(regs_ctl().name());

    // Mask every interrupt, then stop both queues.
    journal.write(
        bus,
        regs::named("IGC_IMC").expect("named"),
        bits::INTERRUPT_MASK_ALL,
    )?;
    journal.write(bus, regs::named("IGC_RCTL").expect("named"), 0)?;
    journal.write(bus, regs::named("IGC_TCTL").expect("named"), bits::TCTL_PSP)?;
    // The vendor driver's flush: a read of a register that is always there.
    let _ = read(bus, regs_status())?;
    bus.delay_us(RESET_SETTLE_US);

    // Assert the global reset.  The hardware clears the bit itself.
    let control = DeviceControl::new(read(bus, regs_ctl())?);
    journal.write(bus, regs_ctl(), control.with_reset().raw())?;

    // Wait for the NVM auto-read, which is what makes the receive-address
    // registers readable and is the only completion signal the vendor driver
    // uses after this reset.
    let mut auto_read_polls = None;
    for poll in 1..=bits::AUTO_READ_DONE_TIMEOUT {
        let eecd = NvmControl::new(read(bus, regs::named("IGC_EECD").expect("named"))?);
        if eecd.auto_read_done() {
            auto_read_polls = Some(poll);
            break;
        }
        bus.delay_us(AUTO_READ_POLL_US);
    }

    // Clear the causes the reset raised.  Reading IGC_ICR *is* the clear, which
    // is why the table marks it read-to-clear.
    journal.write(
        bus,
        regs::named("IGC_IMC").expect("named"),
        bits::INTERRUPT_MASK_ALL,
    )?;
    let _ = read(bus, regs::named("IGC_ICR").expect("named"))?;

    let reset_bit_cleared = !DeviceControl::new(read(bus, regs_ctl())?).reset_asserted();

    // The packet-buffer split the vendor driver programs at probe.
    journal.write(
        bus,
        regs::named("IGC_RXPBS").expect("named"),
        bits::RXPBSIZE_DEFAULT,
    )?;
    journal.write(
        bus,
        regs::named("IGC_TXPBS").expect("named"),
        bits::TXPBSIZE_DEFAULT,
    )?;

    Ok((
        ResetOutcome {
            master_disable_polls,
            auto_read_polls,
            reset_bit_cleared,
        },
        journal.written,
    ))
}

/// Read the station address out of the receive-address registers
/// (`igc_nvm.c:133`).
pub fn read_station_address<B: IgcBus>(bus: &mut B) -> Result<StationAddress, BringUpError> {
    let low = read(bus, regs::named("IGC_RAL(0)").expect("named"))?;
    let high = read(bus, regs::named("IGC_RAH(0)").expect("named"))?;
    let high = ReceiveAddressHigh::new(high);
    Ok(StationAddress {
        bytes: assemble_receive_address(low, high),
        low,
        high: high.raw(),
        address_valid: high.address_valid(),
    })
}

/// Read one of the PHY's MII registers over `IGC_MDIC` (`igc_phy.c:544`).
///
/// The vendor loop delays *before* each read, so the first poll happens 50
/// microseconds after the command word was written; this keeps that order and
/// the vendor's poll bound of `IGC_GEN_POLL_TIMEOUT` iterations
/// (`igc_defines.h:620`).
pub fn read_phy<B: IgcBus>(bus: &mut B, register: u16) -> Result<u16, BringUpError> {
    let command = MdicCommand::read(regs::MDIC_PHY_ADDRESS, register)
        .ok_or(BringUpError::PhyError { register })?;
    let mdic = regs::named("IGC_MDIC").expect("named");
    if !bus.write(mdic, command.raw()) {
        return Err(BringUpError::RegisterNotWritable(mdic.name()));
    }
    let mut result = MdicResult::new(0);
    for _ in 0..bits::GEN_POLL_TIMEOUT {
        bus.delay_us(MDIC_POLL_US);
        result = MdicResult::new(
            bus.read(mdic)
                .ok_or(BringUpError::RegisterNotReadable(mdic.name()))?,
        );
        if result.ready() {
            break;
        }
    }
    if !result.ready() {
        return Err(BringUpError::PhyTimeout {
            register,
            polls: bits::GEN_POLL_TIMEOUT,
        });
    }
    if result.error() {
        return Err(BringUpError::PhyError { register });
    }
    Ok(result.data())
}

/// Ask the MAC to bring the link up and let the PHY autonegotiate
/// (`igc_base.c:112-115`).
pub fn start_link<B: IgcBus>(
    bus: &mut B,
    journal: &mut Vec<&'static str>,
) -> Result<(), BringUpError> {
    let control = DeviceControl::new(read(bus, regs_ctl())?);
    let register = regs_ctl();
    if !bus.write(register, control.with_link_up_autonegotiated().raw()) {
        return Err(BringUpError::RegisterNotWritable(register.name()));
    }
    journal.push(register.name());
    Ok(())
}

/// Poll the PHY for link, then read speed and duplex out of `IGC_STATUS`.
///
/// This is `igc_phy_has_link` (`igc_phy.c:64`) followed by
/// `igc_get_speed_and_duplex_copper` (`igc_mac.c:681`), with the bounded wait
/// described in the module documentation.
pub fn wait_for_link<B: IgcBus>(bus: &mut B) -> Result<LinkOutcome, BringUpError> {
    let mut last_phy_status = 0;
    for poll in 1..=LINK_POLL_BUDGET {
        // The vendor driver reads the status register twice per poll, because
        // on some PHYs the link bit is sticky (`igc_phy.c:71-86`).  Keeping
        // the second read is cheap and keeps this loop the vendor's shape.
        let _first = read_phy(bus, mii::STATUS)?;
        last_phy_status = read_phy(bus, mii::STATUS)?;
        if last_phy_status & bits::MII_SR_LINK_STATUS != 0 {
            let status = DeviceStatus::new(read(bus, regs_status())?);
            // The advertisement and partner words are read *after* link, as
            // diagnostics: this driver never writes them.
            let advertisement = read_phy(bus, mii::AUTONEG_ADV).ok();
            let gigabit_control = read_phy(bus, mii::CTRL_1000T).ok();
            let partner = read_phy(bus, mii::LP_ABILITY).ok();
            let gigabit_status = read_phy(bus, mii::STATUS_1000T).ok();
            return Ok(LinkOutcome {
                polls: poll,
                phy_status: last_phy_status,
                autoneg_complete: last_phy_status & bits::MII_SR_AUTONEG_COMPLETE != 0,
                advertisement,
                gigabit_control,
                partner,
                gigabit_status,
                status,
            });
        }
        if poll < LINK_POLL_BUDGET {
            bus.delay_us(LINK_POLL_US);
        }
    }
    Err(BringUpError::NoLink {
        polls: LINK_POLL_BUDGET,
        last_phy_status,
    })
}

/// The whole phase: reset, station address, link.
///
/// The station address is checked before the link is started, because a part
/// whose address is not usable is a part this driver will not drive: an
/// interface with no address cannot be reached, and claiming one would be
/// claiming more than is known.
pub fn bring_up<B: IgcBus>(bus: &mut B) -> Result<BringUp, BringUpError> {
    let (reset, mut written) = reset(bus)?;
    let station = read_station_address(bus)?;
    if !is_valid_unicast(station.bytes) {
        return Err(BringUpError::InvalidStationAddress(station.bytes));
    }
    start_link(bus, &mut written)?;
    let link = wait_for_link(bus)?;
    Ok(BringUp {
        written,
        reset,
        station,
        link,
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::igc::fake::{FakeBus, NicModel};

    const MAC: [u8; 6] = [0x00, 0x1b, 0x21, 0x9a, 0xbc, 0xde];

    fn seeded(model: NicModel) -> FakeBus {
        FakeBus::with_model(model)
    }

    #[test]
    fn a_healthy_part_resets_reads_its_address_and_finds_link() {
        let mut bus = seeded(NicModel::healthy(MAC, Speed::Mbit2500));
        let up = bring_up(&mut bus).expect("a healthy part comes up");

        // The reset observed what it should.
        assert_eq!(up.reset.master_disable_polls, Some(1));
        assert_eq!(up.reset.auto_read_polls, Some(1));
        assert!(up.reset.reset_bit_cleared);
        // The address is the one the "NVM" held, and its valid bit is set.
        assert_eq!(up.station.bytes, MAC);
        assert!(up.station.address_valid);
        assert_eq!(up.station.low, 0x9a211b00);
        assert_eq!(up.station.high, 0x8000_debc);
        // The link came up at the speed the model reports.
        assert_eq!(up.link.speed(), Speed::Mbit2500);
        assert!(up.link.full_duplex());
        assert!(up.link.autoneg_complete);
        assert_eq!(up.link.polls, 1);
        assert_eq!(up.link.status.raw() & bits::STATUS_LU, bits::STATUS_LU);
    }

    #[test]
    fn the_sequence_writes_exactly_the_registers_it_journals() {
        let mut bus = seeded(NicModel::healthy(MAC, Speed::Mbit1000));
        let up = bring_up(&mut bus).unwrap();
        assert_eq!(
            up.written,
            alloc::vec![
                "IGC_CTRL", // GIO master disable
                "IGC_IMC",  // mask interrupts
                "IGC_RCTL", // stop receive
                "IGC_TCTL", // stop transmit, pad short packets
                "IGC_CTRL", // global reset
                "IGC_IMC",  // mask interrupts again
                "IGC_RXPBS",
                "IGC_TXPBS",
                "IGC_CTRL", // link up, autonegotiate
            ]
        );
        // And the bus saw exactly those writes, in that order: the journal is
        // not a claim about the sequence, it is the sequence's own record.
        // `IGC_MDIC` is excluded because a PHY read is a command/response
        // transaction rather than one of the sequence's setup writes; the loop
        // below checks that every one of those was a read command.
        let mdic = regs::named("IGC_MDIC").unwrap().offset();
        let accepted: Vec<&str> = bus
            .accepted_writes()
            .iter()
            .filter(|(offset, _)| *offset != mdic)
            .map(|(offset, _)| regs::at_offset(*offset).expect("a named register").name())
            .collect();
        assert_eq!(accepted, up.written);
        // Not one PHY register was written: MDIC transactions here are reads.
        for (offset, value, accepted) in bus.writes() {
            if *offset == regs::named("IGC_MDIC").unwrap().offset() {
                assert!(*accepted);
                assert_eq!(
                    value & bits::MDIC_OP_WRITE,
                    0,
                    "the driver must never issue an MDIC write"
                );
            }
        }
    }

    #[test]
    fn the_reset_waits_for_the_nvm_auto_read_before_reading_the_address() {
        // A part whose auto-read takes three polls: the address must still be
        // the one the model then reports, and the report must say how long it
        // took.
        let mut model = NicModel::healthy(MAC, Speed::Mbit1000);
        model.auto_read_after_reads = 3;
        let mut bus = seeded(model);
        let up = bring_up(&mut bus).unwrap();
        assert_eq!(up.reset.auto_read_polls, Some(3));
        assert_eq!(up.station.bytes, MAC);
        assert!(
            bus.delays().contains(&AUTO_READ_POLL_US),
            "the wait between polls is the vendor's"
        );
    }

    #[test]
    fn an_auto_read_that_never_completes_is_reported_and_then_fails_on_the_address() {
        // The vendor driver continues after a failed auto-read, and so does
        // this one -- but the consequence, an address that was never loaded,
        // stops the bring-up with a reason that names the real problem.
        let mut model = NicModel::blank_nvm();
        model.auto_read_after_reads = u32::MAX;
        let mut bus = seeded(model);
        let error = bring_up(&mut bus).expect_err("no address, no driver");
        assert_eq!(
            error,
            BringUpError::InvalidStationAddress([0; 6]),
            "{error:?}"
        );
        assert!(error.describe().contains("blank or unread NVM"));
    }

    #[test]
    fn a_part_with_no_link_fails_after_the_budget_and_says_what_it_saw() {
        let mut model = NicModel::healthy(MAC, Speed::Mbit1000);
        model.link_up_after_polls = u32::MAX;
        let mut bus = seeded(model);
        // Only the link phase is driven here, so the delay log holds the link
        // waits and the MDIC transactions and nothing else -- `RESET_SETTLE_US`
        // happens to have the same value as `LINK_POLL_US`, and a whole
        // `bring_up` would make this count ambiguous.
        start_link(&mut bus, &mut Vec::new()).unwrap();
        let error = wait_for_link(&mut bus).expect_err("no link");
        assert_eq!(
            error,
            BringUpError::NoLink {
                polls: LINK_POLL_BUDGET,
                // Autonegotiation completed; the cable is what is missing, and
                // the model's status word says exactly that.
                last_phy_status: bits::MII_SR_AUTONEG_COMPLETE,
            }
        );
        assert!(error.describe().contains("no link in 300 polls"));
        // The wait was bounded: one delay between polls, and none after the
        // last one, so the whole thing is 2.99 seconds rather than 3.
        let link_delays: Vec<u32> = bus
            .delays()
            .iter()
            .copied()
            .filter(|delay| *delay == LINK_POLL_US)
            .collect();
        assert_eq!(link_delays.len() as u32, LINK_POLL_BUDGET - 1);
        assert_eq!(
            link_delays.iter().map(|us| u64::from(*us)).sum::<u64>(),
            u64::from(LINK_POLL_US) * u64::from(LINK_POLL_BUDGET - 1),
        );
        // And the PHY was asked exactly twice per poll.
        let mdic_delays = bus
            .delays()
            .iter()
            .filter(|delay| **delay == MDIC_POLL_US)
            .count();
        assert_eq!(mdic_delays as u32, LINK_POLL_BUDGET * 2);
    }

    #[test]
    fn link_that_comes_up_late_is_waited_for_and_counted() {
        let mut model = NicModel::healthy(MAC, Speed::Mbit100);
        // The model counts *reads* of the MII status register; the driver
        // reads it twice per poll (the vendor driver does too, because the link
        // bit is sticky on some PHYs) and checks the second read.  Link
        // appearing on read 13 is therefore declared on poll 7.
        model.link_up_after_polls = 13;
        model.full_duplex = false;
        let mut bus = seeded(model);
        let up = bring_up(&mut bus).unwrap();
        assert_eq!(up.link.polls, 7);
        assert_eq!(
            bus.model().unwrap().link_polls,
            14,
            "seven polls of two reads each, with link on the first read of the seventh",
        );
        assert_eq!(up.link.speed(), Speed::Mbit100);
        assert!(
            !up.link.full_duplex(),
            "100 Mb/s is reported as half duplex only if the model says so"
        );
    }

    #[test]
    fn a_phy_that_does_not_answer_times_out_with_the_vendor_bound() {
        let mut model = NicModel::healthy(MAC, Speed::Mbit1000);
        model.mdic_never_ready = true;
        let mut bus = seeded(model);
        let error = bring_up(&mut bus).expect_err("a silent PHY is not a link");
        assert_eq!(
            error,
            BringUpError::PhyTimeout {
                register: mii::STATUS,
                polls: bits::GEN_POLL_TIMEOUT,
            }
        );
        // The vendor bound is what the loop used, and the delay is the
        // vendor's 50 microseconds per poll.
        let mdic_polls = bus
            .delays()
            .iter()
            .filter(|delay| **delay == MDIC_POLL_US)
            .count();
        assert!(mdic_polls >= bits::GEN_POLL_TIMEOUT as usize);
    }

    #[test]
    fn a_phy_that_reports_an_error_fails_with_the_register_named() {
        let mut model = NicModel::healthy(MAC, Speed::Mbit1000);
        model.mdic_error = true;
        let mut bus = seeded(model);
        let error = bring_up(&mut bus).expect_err("an error bit is an error");
        assert_eq!(
            error,
            BringUpError::PhyError {
                register: mii::STATUS
            }
        );
    }

    #[test]
    fn a_gio_master_that_will_not_stop_is_reported_and_not_fatal() {
        // igc_base.c:27-29 treats this as a diagnostic; so does this driver,
        // and the report has to say so rather than looking like a success.
        let mut model = NicModel::healthy(MAC, Speed::Mbit1000);
        model.master_never_stops = true;
        let mut bus = seeded(model);
        let up = bring_up(&mut bus).unwrap();
        assert_eq!(up.reset.master_disable_polls, None);
        let text = up.render("0000:01:00.0");
        assert!(text.contains("GIO master never stopped"), "{text}");
        assert!(text.contains("reported, not fatal"), "{text}");
        // The poll bound is the vendor's.
        let polls = bus
            .delays()
            .iter()
            .filter(|delay| **delay == MASTER_DISABLE_POLL_US)
            .count();
        assert_eq!(polls as u32, bits::MASTER_DISABLE_TIMEOUT);
    }

    #[test]
    fn the_rendered_report_carries_the_facts_a_person_on_the_machine_needs() {
        let mut bus = seeded(NicModel::healthy(MAC, Speed::Mbit2500));
        let up = bring_up(&mut bus).unwrap();
        let text = up.render("0000:01:00.0");
        for line in text.lines() {
            assert!(line.starts_with(super::super::probe::PREFIX), "{line}");
        }
        assert!(text.contains("0000:01:00.0"), "{text}");
        assert!(text.contains("00:1b:21:9a:bc:de"), "{text}");
        assert!(text.contains("RAH.AV 1"), "{text}");
        assert!(text.contains("2500 Mb/s"), "{text}");
        assert!(text.contains("full duplex"), "{text}");
        assert!(text.contains("autonegotiation complete"), "{text}");
        assert!(text.contains("wrote 9 registers"), "{text}");
        assert!(text.contains("no PHY register was written"), "{text}");
        assert!(
            text.contains("no frame can be sent or received yet"),
            "the report must not imply the interface works: {text}",
        );
    }

    #[test]
    fn a_clear_rah_av_bit_is_printed_as_a_warning_and_not_as_success() {
        let mut model = NicModel::healthy(MAC, Speed::Mbit2500);
        model.clear_rah_av = true;
        let mut bus = seeded(model);
        let up = bring_up(&mut bus).unwrap();
        assert!(!up.station.address_valid);
        let text = up.render("0000:01:00.0");
        assert!(text.contains("RAH.AV 0"), "{text}");
        assert!(text.contains("NOT armed"), "{text}");
    }

    #[test]
    fn the_advertisement_decode_names_the_modes_and_the_pause_bits() {
        // A typical 10/100/1000 advertisement: every 10/100 mode plus pause.
        let word = 0x01e0 | bits::NWAY_AR_PAUSE | bits::NWAY_AR_ASM_DIR;
        let text = LinkOutcome::describe_advertisement(word);
        assert!(text.contains("10HD"), "{text}");
        assert!(text.contains("10FD"), "{text}");
        assert!(text.contains("100HD"), "{text}");
        assert!(text.contains("100FD"), "{text}");
        assert!(text.contains("pause advertised"), "{text}");
        assert!(text.contains("asymmetric pause advertised"), "{text}");
        // A word with no 10/100 bits says so rather than printing nothing.
        let empty = LinkOutcome::describe_advertisement(0);
        assert!(empty.contains("no 10/100 mode advertised"), "{empty}");
        assert!(empty.contains("pause not advertised"), "{empty}");
    }

    #[test]
    fn the_link_outcome_reports_what_the_advertisement_does_and_does_not_say() {
        let mut bus = seeded(NicModel::healthy(MAC, Speed::Mbit1000));
        let up = bring_up(&mut bus).unwrap();
        // The model advertises every 10/100 mode and pause in register 4,
        // 1000BASE-T full duplex in register 9, and the partner's receiver
        // ready in register 10.
        assert!(up.link.advertisement.is_some());
        assert!(up.link.gigabit_control.is_some());
        assert!(up.link.partner.is_some());
        assert!(up.link.gigabit_status.is_some());
        let described = up.link.describe();
        assert!(described.starts_with("link up, "), "{described}");
        assert!(described.contains("after 1 poll"), "{described}");
        // The report says plainly that it cannot see the 2.5 Gb/s
        // advertisement.
        let text = up.render("0000:01:00.0");
        assert!(
            text.contains("MMD register 7.32, which this driver does not read"),
            "{text}"
        );
    }

    #[test]
    fn the_gigabit_advertisement_bit_is_read_from_register_9_not_register_4() {
        // This is the bug an adversarial audit found: `CR_1000T_FD_CAPS` is
        // 0x0200 in MII register 9, while 0x0100 in register 4 is
        // `NWAY_AR_100TX_FD_CAPS` -- 100 Mb/s full duplex, not 1000.
        let mut bus = seeded(NicModel::healthy(MAC, Speed::Mbit1000));
        let up = bring_up(&mut bus).unwrap();
        assert_eq!(up.link.advertises_gigabit_full(), Some(true));
        assert_eq!(up.link.advertises_100_full(), Some(true));
        assert_eq!(up.link.partner_ready(), Some(true));
        assert_eq!(up.link.partner_advertises_pause(), Some(true));

        // A PHY that advertises 100 Mb/s full duplex but not 1000BASE-T: the
        // old predicate would have said "gigabit" here.
        let mut model = NicModel::healthy(MAC, Speed::Mbit100);
        model.phy.insert(mii::CTRL_1000T, 0);
        model.phy.insert(mii::STATUS_1000T, 0);
        let mut bus = seeded(model);
        let up = bring_up(&mut bus).unwrap();
        assert_eq!(
            up.link
                .advertisement
                .map(|word| word & bits::NWAY_AR_100TX_FD_CAPS != 0),
            Some(true)
        );
        assert_eq!(up.link.advertises_100_full(), Some(true));
        assert_eq!(up.link.advertises_gigabit_full(), Some(false));
        assert_eq!(up.link.partner_ready(), Some(false));

        // And a register that was never read says so instead of guessing.
        let outcome = LinkOutcome {
            polls: 1,
            phy_status: 0x0024,
            autoneg_complete: true,
            advertisement: None,
            gigabit_control: None,
            partner: None,
            gigabit_status: None,
            status: DeviceStatus::new(0),
        };
        assert_eq!(outcome.advertises_gigabit_full(), None);
        assert_eq!(outcome.partner_ready(), None);
    }

    #[test]
    fn the_gigabit_decodes_name_the_register_they_come_from() {
        let control = LinkOutcome::describe_gigabit_control(bits::CR_1000T_FD_CAPS);
        assert!(control.contains("1000FD"), "{control}");
        assert!(!control.contains("1000HD"), "{control}");
        let both =
            LinkOutcome::describe_gigabit_control(bits::CR_1000T_FD_CAPS | bits::CR_1000T_HD_CAPS);
        assert!(both.contains("1000FD") && both.contains("1000HD"), "{both}");
        let none = LinkOutcome::describe_gigabit_control(0);
        assert!(none.contains("no 1000BASE-T mode"), "{none}");

        assert!(
            LinkOutcome::describe_gigabit_status(bits::SR_1000T_REMOTE_RX_STATUS)
                .contains("receiver is ready")
        );
        assert!(
            LinkOutcome::describe_gigabit_status(0).contains("does not report its receiver ready")
        );
    }

    #[test]
    fn the_error_sentences_name_the_register_or_the_bound() {
        assert!(
            BringUpError::RegisterNotWritable("IGC_CTRL")
                .describe()
                .contains("IGC_CTRL is not writable")
        );
        assert!(
            BringUpError::RegisterNotReadable("IGC_STATUS")
                .describe()
                .contains("IGC_STATUS is not readable")
        );
        let invalid = BringUpError::InvalidStationAddress([0x01, 0, 0, 0, 0, 0]);
        assert!(invalid.describe().contains("01:00:00:00:00:00"));
        assert!(invalid.describe().contains("not a usable unicast address"));
        let no_link = BringUpError::NoLink {
            polls: 5,
            last_phy_status: 0x1234,
        };
        assert!(no_link.describe().contains("0x1234"));
    }
}
