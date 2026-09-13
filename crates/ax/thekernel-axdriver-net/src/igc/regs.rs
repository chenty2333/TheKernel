//! The register map: named registers, their access rules, and bounded access.
//!
//! A NIC is a block of 32-bit registers behind a PCI memory BAR.  Everything
//! about this part that is not in configuration space is a load or a store at
//! a fixed offset, so the job of this module is to make those loads and stores
//! named, typed, bounded, and honest about what the hardware is allowed to be
//! asked.
//!
//! Four properties are deliberate, and the first three are the same three the
//! Intel display probe's register module states for its own aperture, because
//! this project has one way of doing this and not two:
//!
//! * **A register is a named value, not an integer.**  A caller cannot form an
//!   offset by adding to a base, and it cannot write a register the table did
//!   not declare writable, nor read one the table declares write-only.  The
//!   window refuses the operation and returns `None`/`false`; it does not
//!   guess.
//! * **Every register cites where its fact came from.**  Each entry carries
//!   the Linux `igc` symbol and line the offset and access came from, and the
//!   report prints it, so a reader with the source open can check the value
//!   against the code that uses it instead of guessing.
//! * **Access is bounded by the window that was actually mapped.**  A register
//!   outside the mapped window is refused rather than followed, so a table
//!   that disagrees with the aperture produces a report line instead of a
//!   fault.
//! * **A read with a side effect is not a read.**  `IGC_ICR` clears the
//!   interrupt causes it reports, and the identify-only probe must not clear
//!   anything.  The table says so as an access mode, and the probe's own
//!   register list is a subset that excludes it -- a property a test checks
//!   rather than a rule a reader has to trust.
//!
//! # Where the facts come from
//!
//! Linux v6.12 `drivers/net/ethernet/intel/igc/` (tag `v6.12`, commit
//! `adc218676eef25575469234709c2d87185ca223a`): `igc_regs.h` for offsets,
//! `igc_defines.h` for bit values and field masks, `igc_base.h` for the
//! descriptor-control bits, `igc.h` for the ring thresholds, and `igc_main.c`
//! / `igc_base.c` / `igc_mac.c` / `igc_phy.c` / `igc_nvm.c` for the sequences
//! that read and write them.  Facts only; no code is copied.  The vendor id
//! lives in [`super::ids`].
//!
//! # What is deliberately not in the table
//!
//! The rule is the brief's: the registers this part needs for reset, link-up
//! and a descriptor ring, not the whole datasheet.  A reader who wonders why a
//! register is missing should find the answer here rather than assume an
//! oversight:
//!
//! * **RSS** (`IGC_MRQC`, `IGC_RETA`, `IGC_RSSRK`, `IGC_RXCSUM`).  Linux's
//!   `igc_setup_mrqc` programs all of them, for many queues and a random hash
//!   key.  This driver uses one queue and does not hash, so it programs none
//!   of them, and the RX descriptor's RSS and checksum words are carried
//!   through as raw values rather than interpreted.
//! * **The multicast table array and the unicast/multicast hash tables**
//!   (`IGC_MTA`, `IGC_UTA`).  Linux zeroes 128 dwords of each in
//!   `igc_init_hw_base`.  This driver does not: they hold whatever reset left
//!   in them.  The consequence is in the design note's list of what is not
//!   implemented.
//! * **The receive address filter beyond entry 0** (`IGC_RAL(1..15)`,
//!   `IGC_RAH(1..15)`).  `igc_init_rx_addrs` writes entry 0 and clears the
//!   other fifteen; this driver writes none of them and relies on the
//!   hardware's NVM auto-read having enabled entry 0.
//! * **Flow control** (`IGC_FCT`, `IGC_FCAH`, `IGC_FCAL`, `IGC_FCTTV`,
//!   `IGC_FCRTL`, `IGC_FCRTH`, `IGC_FCRTV`).  `igc_setup_link` initialises
//!   them and `igc_config_fc_after_link_up` reconciles them with what the PHY
//!   negotiated.  This driver programs none of them and does not set
//!   `CTRL.RFCE`/`CTRL.TFCE`, so pause frames the PHY may negotiate are not
//!   honoured -- a stated limitation, not an oversight.
//! * **Interrupt programming** (`IGC_EIMS`, `IGC_EIMC`, `IGC_IVAR0`,
//!   `IGC_EITR`, `IGC_GPIE`).  This driver polls.  It masks every interrupt
//!   source once, during reset, and never enables one, so it declares only
//!   `IGC_IMC` (write) and `IGC_ICR` (read-to-clear).
//! * **The NVM/flash access path** (`IGC_EERD`, `IGC_EEWR`, `IGC_SRWR`) and
//!   the flash-update registers.  This driver reads the MAC address out of the
//!   receive-address registers after reset, which is what `igc_read_mac_addr`
//!   does, and never talks to the NVM directly.
//! * **Timestamps, TSN, LEDs, power management and PTM** (`IGC_SYSTIML`,
//!   `IGC_TQAVCTRL`, `IGC_LEDCTL`, `IGC_I225_PHPM`, `IGC_PTM_*`).  Named by
//!   the vendor driver; not needed to move a frame.
//!
//! # Two facts that changed this table
//!
//! Both are recorded because a reviewer will otherwise think they are
//! mistakes:
//!
//! * **`IGC_GPHY_VERSION` (`igc_regs.h:17`, offset `0x0001e`) is not an
//!   aperture register.**  `igc_regs.h` lists it in the same block as
//!   `IGC_CTRL` with the comment "I225 gPHY Firmware Version", but its offset
//!   is not dword aligned and the only code that reads it, `igc_phy.c`
//!   `igc_read_phy_fw_version`, goes through `igc_read_phy_reg_gpy` and then
//!   `igc_read_phy_reg_mdic`: it is a register *inside the PHY*, reached over
//!   `IGC_MDIC`.  Reading it needs a write to start the transaction.  The
//!   dword-alignment assertion in [`Register::declare`] is what caught this.
//! * **`hw->phy.addr` is never assigned.**  The vendor driver only ever reads
//!   it, in the two MDIC helpers, and the `igc_hw` structure is zeroed when
//!   the adapter is allocated -- so the vendor driver addresses the integrated
//!   PHY at 0.  [`MDIC_PHY_ADDRESS`] is that zero, with the reasoning attached
//!   rather than the number alone.

use core::sync::atomic::{Ordering, compiler_fence};

/// How many bytes of BAR0 this driver maps.
///
/// The window is not the BAR: it is the part of the BAR this driver maps, and
/// every access is checked against it.  It is sized to contain every register
/// the driver names -- the highest is `IGC_TXDCTL(0)` at `0x0e028` -- with
/// room to spare, and the probe refuses to run at all when the
/// firmware-assigned BAR is smaller than this.
///
/// The BAR's true size is not a fact any source available here establishes:
/// Linux's `igc` maps `pci_resource_len(pdev, 0)` (`igc_main.c:6990`) and
/// never states a size, and the register offsets it names reach `0x12594`
/// (`IGC_PTM_TDELAY`, `igc_regs.h:287`), so the only lower bound this project
/// can derive is "bigger than the registers someone uses".  A window of 64 KiB
/// is a chosen bound, not a measured one, and the probe reports the BAR size
/// beside it.
pub const WINDOW_BYTES: usize = 0x1_0000;

/// Which block of the device a register belongs to.
///
/// This exists for the report and for a reader checking the table against the
/// datasheet's chapters; nothing in the driver switches on it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Group {
    /// Identity and always-present control and status.
    Identity,
    /// Interrupt masking and cause registers.
    Interrupt,
    /// Receive control and the receive descriptor ring.
    Receive,
    /// Transmit control and the transmit descriptor ring.
    Transmit,
    /// The integrated PHY, reached through `IGC_MDIC`.
    Phy,
}

impl Group {
    /// The word the report uses for this group.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Interrupt => "interrupt",
            Self::Receive => "receive",
            Self::Transmit => "transmit",
            Self::Phy => "phy",
        }
    }
}

/// Whether a register may be read, written, or both -- and whether reading it
/// changes anything.
///
/// This is a statement about *this kernel*, not only about the hardware: a
/// register can be writable in the architecture and still be declared
/// read-only here, because writing it before the driver understands it would
/// change what a device the firmware left running is doing.  It is also a
/// statement about *this phase*: an entry's access changes only in the change
/// that first needs the write, and each phase's tests pin the set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    /// This kernel reads it and never writes it.
    ReadOnly,
    /// This kernel reads it and writes it.
    ReadWrite,
    /// This kernel writes it and never reads it.
    ///
    /// `IGC_IMC` is the case: it is a mask-clear register, and the Linux
    /// header calls it write-only (`igc_regs.h:54`).
    WriteOnly,
    /// Reading it has a side effect: it clears what it reports.
    ///
    /// `IGC_ICR` is the case (`igc_regs.h:51`, "Intr Cause Read - RC/W1C").
    /// The identify-only phase must not read one of these, so the probe's
    /// register list is a subset the table marks as side-effect free and a
    /// test checks the subset relation.
    ReadToClear,
}

impl Access {
    /// Whether a plain read is allowed.
    pub const fn is_readable(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite | Self::ReadToClear)
    }

    /// Whether a write is allowed.
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::ReadWrite | Self::WriteOnly)
    }

    /// Whether reading it changes the device's state.
    pub const fn read_has_side_effect(self) -> bool {
        matches!(self, Self::ReadToClear)
    }

    /// The word the report uses for this access.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
            Self::WriteOnly => "wo",
            Self::ReadToClear => "rc",
        }
    }
}

/// What a register's value means, so a report can say something about it
/// without matching on a register's name.
///
/// Only the registers a report *interprets* carry a meaning other than
/// [`Meaning::None`].  A register whose value the driver uses but the report
/// does not describe -- a ring base address, say -- is named, cited and
/// access-checked like any other and simply has nothing to say to a reader of
/// the log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Meaning {
    /// The report has nothing to say about this register's value.
    None,
    /// `IGC_CTRL`: device control.
    DeviceControl,
    /// `IGC_STATUS`: device status, including link state and speed.
    DeviceStatus,
    /// `IGC_EECD`: NVM/flash control and status.
    NvmControl,
    /// `IGC_RAL(0)`: the low half of receive address 0.
    ReceiveAddressLow,
    /// `IGC_RAH(0)`: the high half of receive address 0, plus its valid bit.
    ReceiveAddressHigh,
}

impl Meaning {
    /// The word the report uses for this meaning.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::None => "not interpreted",
            Self::DeviceControl => "device control",
            Self::DeviceStatus => "device status",
            Self::NvmControl => "NVM/flash control",
            Self::ReceiveAddressLow => "receive address 0, low half",
            Self::ReceiveAddressHigh => "receive address 0, high half",
        }
    }
}

/// One 32-bit register in the device aperture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Register {
    name: &'static str,
    offset: u32,
    access: Access,
    group: Group,
    meaning: Meaning,
    /// Why this kernel names it, in one sentence.
    purpose: &'static str,
    /// The Linux symbol and line this offset and access came from.
    source: &'static str,
}

impl Register {
    /// Declare a register this kernel reads and never writes.
    pub const fn read_only(
        name: &'static str,
        offset: u32,
        group: Group,
        meaning: Meaning,
        purpose: &'static str,
        source: &'static str,
    ) -> Self {
        Self::declare(name, offset, Access::ReadOnly, group, meaning, purpose, source)
    }

    /// Declare a register this kernel reads and writes.
    pub const fn read_write(
        name: &'static str,
        offset: u32,
        group: Group,
        meaning: Meaning,
        purpose: &'static str,
        source: &'static str,
    ) -> Self {
        Self::declare(name, offset, Access::ReadWrite, group, meaning, purpose, source)
    }

    /// Declare a register this kernel writes and never reads.
    pub const fn write_only(
        name: &'static str,
        offset: u32,
        group: Group,
        purpose: &'static str,
        source: &'static str,
    ) -> Self {
        Self::declare(
            name,
            offset,
            Access::WriteOnly,
            group,
            Meaning::None,
            purpose,
            source,
        )
    }

    /// Declare a register whose read clears what it reports.
    pub const fn read_to_clear(
        name: &'static str,
        offset: u32,
        group: Group,
        purpose: &'static str,
        source: &'static str,
    ) -> Self {
        Self::declare(
            name,
            offset,
            Access::ReadToClear,
            group,
            Meaning::None,
            purpose,
            source,
        )
    }

    const fn declare(
        name: &'static str,
        offset: u32,
        access: Access,
        group: Group,
        meaning: Meaning,
        purpose: &'static str,
        source: &'static str,
    ) -> Self {
        assert!(
            offset.is_multiple_of(4),
            "device register offsets are dword aligned"
        );
        assert!(
            (offset as usize) + 4 <= WINDOW_BYTES,
            "a named register must lie inside the window this driver maps"
        );
        Self {
            name,
            offset,
            access,
            group,
            meaning,
            purpose,
            source,
        }
    }

    /// The register's name, as the Linux source spells it.
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// The register's offset from the start of the aperture.
    pub const fn offset(self) -> u32 {
        self.offset
    }

    /// What this kernel may do with it.
    pub const fn access(self) -> Access {
        self.access
    }

    /// Which block of the device it belongs to.
    pub const fn group(self) -> Group {
        self.group
    }

    /// What its value means to the report.
    pub const fn meaning(self) -> Meaning {
        self.meaning
    }

    /// Why this kernel names the register.
    pub const fn purpose(self) -> &'static str {
        self.purpose
    }

    /// Where the fact came from.
    pub const fn source(self) -> &'static str {
        self.source
    }

    /// Whether this kernel may read it.
    pub const fn is_readable(self) -> bool {
        self.access.is_readable()
    }

    /// Whether this kernel may write it.
    pub const fn is_writable(self) -> bool {
        self.access.is_writable()
    }

    /// Whether reading it changes the device.
    pub const fn read_has_side_effect(self) -> bool {
        self.access.read_has_side_effect()
    }

    /// Whether a window of `len` bytes contains this register whole.
    pub const fn fits_in(self, len: usize) -> bool {
        (self.offset as usize) + 4 <= len
    }
}

/// Every register this driver names, in offset order.
///
/// The set is the brief's: reset, link-up, and one receive and one transmit
/// descriptor ring, plus the identify registers the probe reads.  The module
/// documentation lists what is deliberately absent and why.
pub const NAMED: &[Register] = &[
    // -- identity ---------------------------------------------------------
    Register::read_write(
        "IGC_CTRL",
        0x00000,
        Group::Identity,
        Meaning::DeviceControl,
        "global reset (CTRL.RST), the link-up request (CTRL.SLU), speed and duplex forcing, and \
         the GIO master disable the reset sequence asserts first",
        "igc_regs.h:8 (IGC_CTRL, \"Device Control - RW\")",
    ),
    Register::read_only(
        "IGC_STATUS",
        0x00008,
        Group::Identity,
        Meaning::DeviceStatus,
        "link state, negotiated speed and duplex, the LAN function number, and the GIO master \
         enable bit the reset sequence polls clear",
        "igc_regs.h:9 (IGC_STATUS, \"Device Status - RO\")",
    ),
    Register::read_only(
        "IGC_EECD",
        0x00010,
        Group::Identity,
        Meaning::NvmControl,
        "the NVM auto-read-done bit that says the receive-address registers hold the address the \
         NVM was programmed with",
        "igc_regs.h:10 (IGC_EECD, \"EEPROM/Flash Control - RW\"); read by igc_mac.c:650 \
         igc_get_auto_rd_done",
    ),
    Register::read_write(
        "IGC_MDIC",
        0x00020,
        Group::Phy,
        Meaning::None,
        "the command and result register for every access to the integrated PHY's registers, \
         including the link-status poll",
        "igc_regs.h:12 (IGC_MDIC, \"MDI Control - RW\"); used by igc_phy.c:544 \
         igc_read_phy_reg_mdic and igc_phy.c:600 igc_write_phy_reg_mdic",
    ),
    Register::read_write(
        "IGC_RCTL",
        0x00100,
        Group::Receive,
        Meaning::None,
        "receive enable, broadcast accept, CRC strip and the long-packet bit",
        "igc_regs.h:98 (IGC_RCTL, \"Rx Control - RW\"); programmed by igc_main.c:835 \
         igc_setup_rctl",
    ),
    Register::read_write(
        "IGC_TCTL",
        0x00400,
        Group::Transmit,
        Meaning::None,
        "transmit enable, pad-short-packets and retransmit-on-late-collision",
        "igc_regs.h:119 (IGC_TCTL, \"Tx Control - RW\"); programmed by igc_main.c:882 \
         igc_setup_tctl",
    ),
    Register::read_to_clear(
        "IGC_ICR",
        0x01500,
        Group::Interrupt,
        "clear pending interrupt causes during reset; reading it is the clear",
        "igc_regs.h:51 (IGC_ICR, \"Intr Cause Read - RC/W1C\"); read by igc_base.c:56 \
         igc_reset_hw_base",
    ),
    Register::write_only(
        "IGC_IMC",
        0x0150c,
        Group::Interrupt,
        "mask every interrupt source during reset; this driver never unmasks one",
        "igc_regs.h:54 (IGC_IMC, \"Intr Mask Clear - WO\"); written by igc_base.c:32 \
         igc_reset_hw_base",
    ),
    Register::read_write(
        "IGC_RXPBS",
        0x02404,
        Group::Identity,
        Meaning::None,
        "the receive packet buffer split; the vendor driver programs the part's documented \
         default at probe",
        "igc_regs.h:20 (IGC_RXPBS, \"Rx Packet Buffer Size - RW\"); written by igc_main.c:7097 \
         with I225_RXPBSIZE_DEFAULT (igc_defines.h:399)",
    ),
    Register::read_write(
        "IGC_TXPBS",
        0x03404,
        Group::Identity,
        Meaning::None,
        "the transmit packet buffer split; the vendor driver programs the part's documented \
         default at probe",
        "igc_regs.h:21 (IGC_TXPBS, \"Tx Packet Buffer Size - RW\"); written by igc_main.c:7098 \
         with I225_TXPBSIZE_DEFAULT (igc_defines.h:400)",
    ),
    Register::read_write(
        "IGC_RLPML",
        0x05004,
        Group::Receive,
        Meaning::None,
        "the longest frame the receive path will accept; the vendor driver sets it to the jumbo \
         bound, and this driver sets it to the size of the buffers it gives the hardware",
        "igc_regs.h:109 (IGC_RLPML, \"Rx Long Packet Max Length\"); written by igc_main.c:4013 \
         igc_set_rx_mode, and cleared by igc_base.c igc_rx_fifo_flush_base",
    ),
    Register::read_only(
        "IGC_RAL(0)",
        0x05400,
        Group::Identity,
        Meaning::ReceiveAddressLow,
        "bytes 0..3 of the station address, little-endian, as the NVM auto-read left them",
        "igc_regs.h:114 (IGC_RAL(_n)); read by igc_nvm.c:139 igc_read_mac_addr",
    ),
    Register::read_only(
        "IGC_RAH(0)",
        0x05404,
        Group::Identity,
        Meaning::ReceiveAddressHigh,
        "bytes 4..5 of the station address and the RAH.AV bit that says the filter entry is armed",
        "igc_regs.h:115 (IGC_RAH(_n)); read by igc_nvm.c:138 igc_read_mac_addr",
    ),
    // -- receive ----------------------------------------------------------
    Register::read_write(
        "IGC_RDBAL(0)",
        0x0c000,
        Group::Receive,
        Meaning::None,
        "the low 32 bits of the receive descriptor ring's bus address",
        "igc_regs.h:101 (IGC_RDBAL(_n)); written by igc_main.c:625 igc_configure_rx_ring",
    ),
    Register::read_write(
        "IGC_RDBAH(0)",
        0x0c004,
        Group::Receive,
        Meaning::None,
        "the high 32 bits of the receive descriptor ring's bus address",
        "igc_regs.h:102 (IGC_RDBAH(_n)); written by igc_main.c:625 igc_configure_rx_ring",
    ),
    Register::read_write(
        "IGC_RDLEN(0)",
        0x0c008,
        Group::Receive,
        Meaning::None,
        "the length of the receive descriptor ring in bytes",
        "igc_regs.h:103 (IGC_RDLEN(_n)); written by igc_main.c:625 igc_configure_rx_ring",
    ),
    Register::read_write(
        "IGC_SRRCTL(0)",
        0x0c00c,
        Group::Receive,
        Meaning::None,
        "the split-receive control for queue 0: buffer size and descriptor type",
        "igc_regs.h:99 (IGC_SRRCTL(_n)); programmed by igc_main.c:625 igc_configure_rx_ring",
    ),
    Register::read_write(
        "IGC_RDH(0)",
        0x0c010,
        Group::Receive,
        Meaning::None,
        "the receive descriptor head; software resets it to zero when it configures the ring",
        "igc_regs.h:104 (IGC_RDH(_n)); written by igc_main.c:625 igc_configure_rx_ring",
    ),
    Register::read_write(
        "IGC_RDT(0)",
        0x0c018,
        Group::Receive,
        Meaning::None,
        "the receive descriptor tail: the boundary between descriptors the hardware owns and \
         descriptors the driver owns",
        "igc_regs.h:105 (IGC_RDT(_n)); written by igc_main.c:2229 igc_alloc_rx_buffers",
    ),
    Register::read_write(
        "IGC_RXDCTL(0)",
        0x0c028,
        Group::Receive,
        Meaning::None,
        "the receive queue's prefetch and write-back thresholds and its queue-enable bit",
        "igc_regs.h:106 (IGC_RXDCTL(_n)); written by igc_main.c:625 igc_configure_rx_ring",
    ),
    // -- transmit ---------------------------------------------------------
    Register::read_write(
        "IGC_TDBAL(0)",
        0x0e000,
        Group::Transmit,
        Meaning::None,
        "the low 32 bits of the transmit descriptor ring's bus address",
        "igc_regs.h:121 (IGC_TDBAL(_n)); written by igc_main.c:728 igc_configure_tx_ring",
    ),
    Register::read_write(
        "IGC_TDBAH(0)",
        0x0e004,
        Group::Transmit,
        Meaning::None,
        "the high 32 bits of the transmit descriptor ring's bus address",
        "igc_regs.h:122 (IGC_TDBAH(_n)); written by igc_main.c:728 igc_configure_tx_ring",
    ),
    Register::read_write(
        "IGC_TDLEN(0)",
        0x0e008,
        Group::Transmit,
        Meaning::None,
        "the length of the transmit descriptor ring in bytes",
        "igc_regs.h:123 (IGC_TDLEN(_n)); written by igc_main.c:728 igc_configure_tx_ring",
    ),
    Register::read_write(
        "IGC_TDH(0)",
        0x0e010,
        Group::Transmit,
        Meaning::None,
        "the transmit descriptor head; software resets it to zero when it configures the ring",
        "igc_regs.h:124 (IGC_TDH(_n)); written by igc_main.c:728 igc_configure_tx_ring",
    ),
    Register::read_write(
        "IGC_TDT(0)",
        0x0e018,
        Group::Transmit,
        Meaning::None,
        "the transmit descriptor tail: writing it is what tells the hardware a frame is ready",
        "igc_regs.h:125 (IGC_TDT(_n)); written by igc_main.c:1316 igc_tx_map",
    ),
    Register::read_write(
        "IGC_TXDCTL(0)",
        0x0e028,
        Group::Transmit,
        Meaning::None,
        "the transmit queue's prefetch and write-back thresholds and its queue-enable bit",
        "igc_regs.h:126 (IGC_TXDCTL(_n)); written by igc_main.c:728 igc_configure_tx_ring",
    ),
];

/// The registers the identify-only probe reads, in report order.
///
/// A subset of [`NAMED`], and a subset a test checks: every entry must be
/// readable and have no read side effect, so the probe cannot clear an
/// interrupt cause or read a write-only register by accident.  The ring and
/// interrupt registers are named in the table but are not read here: nothing
/// has configured them yet, so their values would be facts about the firmware
/// rather than about the part.
pub const IDENTIFY: &[Register] = &[
    Register::read_only(
        "IGC_CTRL",
        0x00000,
        Group::Identity,
        Meaning::DeviceControl,
        "whether the firmware left link forced up, or a reset pending",
        "igc_regs.h:8 (IGC_CTRL, \"Device Control - RW\")",
    ),
    Register::read_only(
        "IGC_STATUS",
        0x00008,
        Group::Identity,
        Meaning::DeviceStatus,
        "link state, speed and duplex as the firmware left them",
        "igc_regs.h:9 (IGC_STATUS, \"Device Status - RO\")",
    ),
    Register::read_only(
        "IGC_EECD",
        0x00010,
        Group::Identity,
        Meaning::NvmControl,
        "whether the NVM auto-read has completed",
        "igc_regs.h:10 (IGC_EECD, \"EEPROM/Flash Control - RW\")",
    ),
    Register::read_only(
        "IGC_RAL(0)",
        0x05400,
        Group::Identity,
        Meaning::ReceiveAddressLow,
        "the station address the NVM auto-read left in the receive filter",
        "igc_regs.h:114 (IGC_RAL(_n)); read by igc_nvm.c:139 igc_read_mac_addr",
    ),
    Register::read_only(
        "IGC_RAH(0)",
        0x05404,
        Group::Identity,
        Meaning::ReceiveAddressHigh,
        "the upper half of that address and its valid bit",
        "igc_regs.h:115 (IGC_RAH(_n)); read by igc_nvm.c:138 igc_read_mac_addr",
    ),
];

/// The highest offset any named register occupies, exclusive.
///
/// The probe compares this against the BAR the firmware assigned: a BAR too
/// small to contain the registers the driver names is a device this driver
/// must not touch.
pub const NAMED_SPAN: usize = {
    let mut highest = 0;
    let mut index = 0;
    while index < NAMED.len() {
        let end = NAMED[index].offset() as usize + 4;
        if end > highest {
            highest = end;
        }
        index += 1;
    }
    highest
};

/// Every named register lies inside the mapped window, checked at compile
/// time: a register that does not cannot be compiled.
const _: () = assert!(NAMED_SPAN <= WINDOW_BYTES);

/// The named register called `name`, or `None`.
pub fn named(name: &str) -> Option<Register> {
    NAMED.iter().copied().find(|register| register.name == name)
}

/// The register at `offset`, or `None` when the table does not name one there.
///
/// This exists for tests and for the report's own bookkeeping; driver code
/// refers to registers by name, never by offset.
pub fn at_offset(offset: u32) -> Option<Register> {
    NAMED
        .iter()
        .copied()
        .find(|register| register.offset == offset)
}

/// Bit values, named as the Linux source names them.
///
/// Each carries the symbol and line it came from; a test checks the values
/// against the comments, so a transcription slip is a test failure rather than
/// a device that behaves oddly.
pub mod bits {
    /// `IGC_CTRL_GIO_MASTER_DISABLE` (`igc_defines.h:97`).
    pub const CTRL_GIO_MASTER_DISABLE: u32 = 0x0000_0004;
    /// `IGC_CTRL_RST` — global reset (`igc_defines.h:132`).
    pub const CTRL_RST: u32 = 0x0400_0000;
    /// `IGC_CTRL_PHY_RST` — PHY reset (`igc_defines.h:134`).
    pub const CTRL_PHY_RST: u32 = 0x8000_0000;
    /// `IGC_CTRL_SLU` — set link up (`igc_defines.h:135`).
    pub const CTRL_SLU: u32 = 0x0000_0040;
    /// `IGC_CTRL_FRCSPD` — force speed (`igc_defines.h:136`).
    pub const CTRL_FRCSPD: u32 = 0x0000_0800;
    /// `IGC_CTRL_FRCDPX` — force duplex (`igc_defines.h:137`).
    pub const CTRL_FRCDPX: u32 = 0x0000_1000;
    /// `IGC_CTRL_RFCE` — receive flow control enable (`igc_defines.h:140`).
    pub const CTRL_RFCE: u32 = 0x0800_0000;
    /// `IGC_CTRL_TFCE` — transmit flow control enable (`igc_defines.h:141`).
    pub const CTRL_TFCE: u32 = 0x1000_0000;

    /// `IGC_STATUS_FD` — full duplex (`igc_defines.h:223`).
    pub const STATUS_FD: u32 = 0x0000_0001;
    /// `IGC_STATUS_LU` — link up (`igc_defines.h:224`).
    pub const STATUS_LU: u32 = 0x0000_0002;
    /// `IGC_STATUS_FUNC_MASK` — PCI function (`igc_defines.h:225`).
    pub const STATUS_FUNC_MASK: u32 = 0x0000_000c;
    /// `IGC_STATUS_FUNC_SHIFT` (`igc_defines.h:226`).
    pub const STATUS_FUNC_SHIFT: u32 = 2;
    /// `IGC_STATUS_TXOFF` — transmission paused (`igc_defines.h:227`).
    pub const STATUS_TXOFF: u32 = 0x0000_0010;
    /// `IGC_STATUS_SPEED_100` — 100 Mb/s (`igc_defines.h:228`).
    pub const STATUS_SPEED_100: u32 = 0x0000_0040;
    /// `IGC_STATUS_SPEED_1000` — 1000 Mb/s (`igc_defines.h:229`).
    pub const STATUS_SPEED_1000: u32 = 0x0000_0080;
    /// `IGC_STATUS_SPEED_2500` — the extra bit that makes it 2.5 Gb/s
    /// (`igc_defines.h:230`).
    pub const STATUS_SPEED_2500: u32 = 0x0040_0000;
    /// `IGC_STATUS_GIO_MASTER_ENABLE` (`igc_defines.h:99`).
    pub const STATUS_GIO_MASTER_ENABLE: u32 = 0x0008_0000;

    /// `IGC_EECD_AUTO_RD` — NVM auto-read done (`igc_defines.h:187`).
    pub const EECD_AUTO_RD: u32 = 0x0000_0200;
    /// `IGC_EECD_SIZE_EX_MASK` — NVM size, extended encoding
    /// (`igc_defines.h:193`).
    pub const EECD_SIZE_EX_MASK: u32 = 0x0000_7800;
    /// `IGC_EECD_SIZE_EX_SHIFT` (`igc_defines.h:194`).
    pub const EECD_SIZE_EX_SHIFT: u32 = 11;
    /// `IGC_EECD_FLASH_DETECTED_I225` (`igc_defines.h:197`).
    pub const EECD_FLASH_DETECTED_I225: u32 = 0x0008_0000;

    /// `IGC_RAH_AV` — receive address valid (`igc_defines.h:114`).
    pub const RAH_AV: u32 = 0x8000_0000;
    /// `IGC_RAH_RAH_MASK` — the upper two bytes of the address
    /// (`igc_defines.h:108`).
    pub const RAH_ADDR_MASK: u32 = 0x0000_ffff;

    /// The value `igc_reset_hw_base` writes to `IGC_IMC`: every source masked
    /// (`igc_base.c:32`).
    pub const INTERRUPT_MASK_ALL: u32 = 0xffff_ffff;

    /// `I225_RXPBSIZE_DEFAULT` (`igc_defines.h:399`).
    pub const RXPBSIZE_DEFAULT: u32 = 0x0000_00a2;
    /// `I225_TXPBSIZE_DEFAULT` (`igc_defines.h:400`).
    pub const TXPBSIZE_DEFAULT: u32 = 0x0400_0014;

    /// `IGC_TCTL_EN` — enable transmit (`igc_defines.h:330`).
    pub const TCTL_EN: u32 = 0x0000_0002;
    /// `IGC_TCTL_PSP` — pad short packets (`igc_defines.h:331`).
    pub const TCTL_PSP: u32 = 0x0000_0008;
    /// `IGC_TCTL_CT` — collision threshold field (`igc_defines.h:332`).
    pub const TCTL_CT: u32 = 0x0000_0ff0;
    /// `IGC_TCTL_COLD` — collision distance field (`igc_defines.h:333`).
    pub const TCTL_COLD: u32 = 0x003f_f000;
    /// `IGC_TCTL_RTLC` — retransmit on late collision (`igc_defines.h:334`).
    pub const TCTL_RTLC: u32 = 0x0100_0000;
    /// `IGC_COLLISION_THRESHOLD`, the value `igc_setup_tctl` puts in `CT`
    /// (`igc_defines.h:217`, `igc_main.c:882`).
    pub const COLLISION_THRESHOLD: u32 = 15;
    /// `IGC_CT_SHIFT` (`igc_defines.h:218`).
    pub const CT_SHIFT: u32 = 4;

    /// `IGC_RCTL_RST` — receive software reset (`igc_defines.h:348`).
    pub const RCTL_RST: u32 = 0x0000_0001;
    /// `IGC_RCTL_EN` — enable receive (`igc_defines.h:349`).
    pub const RCTL_EN: u32 = 0x0000_0002;
    /// `IGC_RCTL_SBP` — store bad packets (`igc_defines.h:350`).
    pub const RCTL_SBP: u32 = 0x0000_0004;
    /// `IGC_RCTL_UPE` — unicast promiscuous (`igc_defines.h:351`).
    pub const RCTL_UPE: u32 = 0x0000_0008;
    /// `IGC_RCTL_MPE` — multicast promiscuous (`igc_defines.h:352`).
    pub const RCTL_MPE: u32 = 0x0000_0010;
    /// `IGC_RCTL_LPE` — long packet enable (`igc_defines.h:353`).
    pub const RCTL_LPE: u32 = 0x0000_0020;
    /// `IGC_RCTL_LBM_MAC`/`IGC_RCTL_LBM_TCVR` — loopback mode field
    /// (`igc_defines.h:354-355`).
    pub const RCTL_LBM: u32 = 0x0000_00c0;
    /// `IGC_RCTL_RDMTS_HALF` (`igc_defines.h:357`).
    pub const RCTL_RDMTS_HALF: u32 = 0x0000_0000;
    /// `IGC_RCTL_BAM` — broadcast accept (`igc_defines.h:358`).
    pub const RCTL_BAM: u32 = 0x0000_8000;
    /// `IGC_RCTL_SZ_256` — the buffer-size field `igc_setup_rctl` clears
    /// (`igc_defines.h:391`, `igc_main.c:835`).
    pub const RCTL_SZ_256: u32 = 0x0003_0000;
    /// `IGC_RCTL_MO_SHIFT` (`igc_defines.h:393`).
    pub const RCTL_MO_SHIFT: u32 = 12;
    /// `IGC_RCTL_SECRC` — strip the Ethernet CRC (`igc_defines.h:397`).
    pub const RCTL_SECRC: u32 = 0x0400_0000;

    /// `MAX_JUMBO_FRAME_SIZE`, the value `igc_set_rx_mode` puts in `IGC_RLPML`
    /// (`igc_defines.h:147`, `igc_main.c:3978`).
    ///
    /// This driver does not use it: its receive buffers are
    /// [`super::desc::RX_BUFFER_BYTES`] bytes, and a receive bound larger than
    /// the buffer a frame is written into is a bound the driver cannot honour.
    pub const MAX_JUMBO_FRAME_SIZE: u32 = 0x2600;

    /// `IGC_RXD_STAT_DD` — descriptor done, in the receive descriptor's
    /// `status_error` word (`igc_defines.h:304`).
    pub const RXD_STAT_DD: u32 = 0x0000_0001;
    /// `IGC_RXD_STAT_EOP` — end of packet, in the same word
    /// (`igc_defines.h:366`).
    pub const RXD_STAT_EOP: u32 = 0x0000_0002;

    /// `IGC_ADVTXD_DTYP_DATA` — an advanced data descriptor (`igc_base.h:45`).
    pub const ADVTXD_DTYP_DATA: u32 = 0x0030_0000;
    /// `IGC_ADVTXD_DCMD_EOP` — end of packet (`igc_base.h:46`).
    pub const ADVTXD_DCMD_EOP: u32 = 0x0100_0000;
    /// `IGC_ADVTXD_DCMD_IFCS` — insert the frame check sequence
    /// (`igc_base.h:47`).
    pub const ADVTXD_DCMD_IFCS: u32 = 0x0200_0000;
    /// `IGC_ADVTXD_DCMD_RS` — report status (`igc_base.h:48`).
    pub const ADVTXD_DCMD_RS: u32 = 0x0800_0000;
    /// `IGC_ADVTXD_DCMD_DEXT` — the descriptor is an advanced one
    /// (`igc_base.h:49`).
    pub const ADVTXD_DCMD_DEXT: u32 = 0x2000_0000;
    /// `IGC_ADVTXD_DCMD_VLE` — insert a VLAN tag (`igc_base.h:50`).
    pub const ADVTXD_DCMD_VLE: u32 = 0x4000_0000;
    /// `IGC_ADVTXD_DCMD_TSE` — TCP segmentation offload (`igc_base.h:51`).
    pub const ADVTXD_DCMD_TSE: u32 = 0x8000_0000;
    /// `IGC_ADVTXD_PAYLEN_SHIFT` (`igc_base.h:52`).
    pub const ADVTXD_PAYLEN_SHIFT: u32 = 14;
    /// `IGC_ADVTXD_MAC_TSTAMP` — take a timestamp for this packet
    /// (`igc_base.h:36`).
    pub const ADVTXD_MAC_TSTAMP: u32 = 0x0008_0000;

    /// `IGC_TXD_STAT_DD` — descriptor done, in the transmit descriptor's
    /// write-back `status` word (`igc_defines.h:315`).
    pub const TXD_STAT_DD: u32 = 0x0000_0001;
    /// `IGC_TXD_POPTS_IXSM` — insert IPv4 checksum (`igc_defines.h:309`).
    pub const TXD_POPTS_IXSM: u32 = 0x0000_0001;
    /// `IGC_TXD_POPTS_TXSM` — insert TCP/UDP checksum (`igc_defines.h:310`).
    pub const TXD_POPTS_TXSM: u32 = 0x0000_0002;

    /// `IGC_TXDCTL_QUEUE_ENABLE` (`igc_base.h:89`).
    pub const TXDCTL_QUEUE_ENABLE: u32 = 0x0200_0000;
    /// `IGC_TXDCTL_SWFLUSH` — transmit software flush (`igc_base.h:90`).
    pub const TXDCTL_SWFLUSH: u32 = 0x0400_0000;
    /// `IGC_RXDCTL_QUEUE_ENABLE` (`igc_base.h:93`).
    pub const RXDCTL_QUEUE_ENABLE: u32 = 0x0200_0000;
    /// `IGC_RXDCTL_SWFLUSH` — receive software flush (`igc_base.h:94`).
    pub const RXDCTL_SWFLUSH: u32 = 0x0400_0000;

    /// `IGC_SRRCTL_BSIZEPKT_MASK` — the packet buffer size field, in units of
    /// 1 KiB (`igc_base.h:97-99`).
    pub const SRRCTL_BSIZEPKT_MASK: u32 = 0x0000_007f;
    /// `IGC_SRRCTL_BSIZEHDR_MASK` — the header buffer size field, in units of
    /// 64 bytes (`igc_base.h:100-102`).
    pub const SRRCTL_BSIZEHDR_MASK: u32 = 0x0000_3f00;
    /// `IGC_SRRCTL_DESCTYPE_MASK` (`igc_base.h:103`).
    pub const SRRCTL_DESCTYPE_MASK: u32 = 0x0e00_0000;
    /// `IGC_SRRCTL_DESCTYPE_ADV_ONEBUF` — one advanced descriptor per packet,
    /// which is what `igc_configure_rx_ring` selects (`igc_base.h:104`,
    /// `igc_main.c:625`).
    pub const SRRCTL_DESCTYPE_ADV_ONEBUF: u32 = 1 << 25;

    /// `IGC_MDIC_DATA_MASK` (`igc_defines.h:644`).
    pub const MDIC_DATA_MASK: u32 = 0x0000_ffff;
    /// `IGC_MDIC_REG_MASK` (`igc_defines.h:645`).
    pub const MDIC_REG_MASK: u32 = 0x001f_0000;
    /// `IGC_MDIC_REG_SHIFT` (`igc_defines.h:646`).
    pub const MDIC_REG_SHIFT: u32 = 16;
    /// `IGC_MDIC_PHY_MASK` (`igc_defines.h:647`).
    pub const MDIC_PHY_MASK: u32 = 0x03e0_0000;
    /// `IGC_MDIC_PHY_SHIFT` (`igc_defines.h:648`).
    pub const MDIC_PHY_SHIFT: u32 = 21;
    /// `IGC_MDIC_OP_WRITE` (`igc_defines.h:649`).
    pub const MDIC_OP_WRITE: u32 = 0x0400_0000;
    /// `IGC_MDIC_OP_READ` (`igc_defines.h:650`).
    pub const MDIC_OP_READ: u32 = 0x0800_0000;
    /// `IGC_MDIC_READY` (`igc_defines.h:651`).
    pub const MDIC_READY: u32 = 0x1000_0000;
    /// `IGC_MDIC_ERROR` (`igc_defines.h:652`).
    pub const MDIC_ERROR: u32 = 0x4000_0000;
    /// `MAX_PHY_REG_ADDRESS` — the PHY register field is 5 bits
    /// (`igc_defines.h:619`).
    pub const MAX_PHY_REG_ADDRESS: u32 = 0x1f;
    /// `IGC_GEN_POLL_TIMEOUT` — the MDIC poll bound, in 50 microsecond
    /// iterations (`igc_defines.h:620`, `igc_phy.c:544`).
    pub const GEN_POLL_TIMEOUT: u32 = 1920;

    /// `MII_SR_LINK_STATUS` — the link bit in MII status register 1
    /// (`igc_defines.h:628`).
    pub const MII_SR_LINK_STATUS: u16 = 0x0004;
    /// `MII_SR_AUTONEG_COMPLETE` (`igc_defines.h:629`).
    pub const MII_SR_AUTONEG_COMPLETE: u16 = 0x0020;
    /// `NWAY_AR_PAUSE` — we advertise pause (`igc_defines.h:165`).
    pub const NWAY_AR_PAUSE: u16 = 0x0400;
    /// `NWAY_AR_ASM_DIR` — we advertise asymmetric pause
    /// (`igc_defines.h:166`).
    pub const NWAY_AR_ASM_DIR: u16 = 0x0800;
    /// `NWAY_LPAR_PAUSE` — the link partner advertises pause
    /// (`igc_defines.h:169`).
    pub const NWAY_LPAR_PAUSE: u16 = 0x0400;
    /// `NWAY_LPAR_ASM_DIR` — the link partner advertises asymmetric pause
    /// (`igc_defines.h:170`).
    pub const NWAY_LPAR_ASM_DIR: u16 = 0x0800;
    /// `CR_1000T_FD_CAPS` — 1000BASE-T full duplex advertised
    /// (`igc_defines.h:174`).
    pub const CR_1000T_FD_CAPS: u16 = 0x0200;
    /// `SR_1000T_REMOTE_RX_STATUS` (`igc_defines.h:177`).
    pub const SR_1000T_REMOTE_RX_STATUS: u16 = 0x1000;

    /// `COPPER_LINK_UP_LIMIT` — how many times `igc_setup_copper_link` asks the
    /// PHY for link before giving up (`igc_defines.h:91`, `igc_phy.c:492`).
    pub const COPPER_LINK_UP_LIMIT: u32 = 10;
    /// `MASTER_DISABLE_TIMEOUT` — the GIO master disable poll bound, in 2-3
    /// millisecond iterations (`igc_defines.h:95`, `igc_mac.c:21`).
    pub const MASTER_DISABLE_TIMEOUT: u32 = 800;
    /// `AUTO_READ_DONE_TIMEOUT` — the NVM auto-read poll bound, in 1-2
    /// millisecond iterations (`igc_defines.h:186`, `igc_mac.c:650`).
    pub const AUTO_READ_DONE_TIMEOUT: u32 = 10;
}

/// MII register numbers in the integrated PHY, as `igc_defines.h` names them
/// (`igc_defines.h:635-641`).
pub mod mii {
    /// `PHY_STATUS` — the MII status register, whose bit 2 is link.
    pub const STATUS: u16 = 0x01;
    /// `PHY_ID1` — PHY identifier, first word.
    pub const ID1: u16 = 0x02;
    /// `PHY_ID2` — PHY identifier, second word (revision in the low bits).
    pub const ID2: u16 = 0x03;
    /// `PHY_AUTONEG_ADV` — the advertisement register this driver reads and
    /// does not write.
    pub const AUTONEG_ADV: u16 = 0x04;
    /// `PHY_LP_ABILITY` — what the link partner advertises.
    pub const LP_ABILITY: u16 = 0x05;
    /// `PHY_1000T_CTRL` — the 1000BASE-T control register.
    pub const CTRL_1000T: u16 = 0x09;
    /// `PHY_1000T_STATUS` — the 1000BASE-T status register.
    pub const STATUS_1000T: u16 = 0x0a;
}

/// The PHY address this driver puts in `IGC_MDIC`'s PHY field.
///
/// Linux never assigns `hw->phy.addr`: the field is only ever read, inside
/// `igc_read_phy_reg_mdic` and `igc_write_phy_reg_mdic`
/// (`igc_phy.c:561`/`:618`), and the `igc_hw` structure is zeroed when the
/// adapter is allocated, so the vendor driver addresses the integrated PHY at
/// **0**.  That is a fact about the vendor driver, not about the part, and it
/// is the first thing to check if a link-status poll on real hardware returns
/// `IGC_MDIC_ERROR`.
pub const MDIC_PHY_ADDRESS: u32 = 0;

/// Link speed, as the part reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Speed {
    /// 10 Mb/s.
    Mbit10,
    /// 100 Mb/s.
    Mbit100,
    /// 1000 Mb/s.
    Mbit1000,
    /// 2500 Mb/s.
    Mbit2500,
}

impl Speed {
    /// The speed in Mb/s.
    pub const fn mbps(self) -> u16 {
        match self {
            Self::Mbit10 => 10,
            Self::Mbit100 => 100,
            Self::Mbit1000 => 1000,
            Self::Mbit2500 => 2500,
        }
    }

    /// How the report spells it.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Mbit10 => "10 Mb/s",
            Self::Mbit100 => "100 Mb/s",
            Self::Mbit1000 => "1000 Mb/s",
            Self::Mbit2500 => "2500 Mb/s",
        }
    }
}

/// `IGC_STATUS`: link state, speed, duplex and function number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceStatus(u32);

impl DeviceStatus {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_STATUS_LU`: the MAC reports link.
    pub const fn link_up(self) -> bool {
        self.0 & bits::STATUS_LU != 0
    }

    /// `IGC_STATUS_FD`: full duplex when set.
    pub const fn full_duplex(self) -> bool {
        self.0 & bits::STATUS_FD != 0
    }

    /// `IGC_STATUS_TXOFF`: the transmitter is paused.
    pub const fn transmit_paused(self) -> bool {
        self.0 & bits::STATUS_TXOFF != 0
    }

    /// `IGC_STATUS_GIO_MASTER_ENABLE`: the GIO master is still enabled.
    ///
    /// `igc_disable_pcie_master` (`igc_mac.c:21`) polls this bit *clear* after
    /// setting `CTRL.GIO_MASTER_DISABLE`.
    pub const fn master_enabled(self) -> bool {
        self.0 & bits::STATUS_GIO_MASTER_ENABLE != 0
    }

    /// `IGC_STATUS_FUNC`: which PCI function this is.
    ///
    /// `igc_base.c:165` reads it the same way when it sets the LAN id.
    pub const fn function_id(self) -> u8 {
        ((self.0 & bits::STATUS_FUNC_MASK) >> bits::STATUS_FUNC_SHIFT) as u8
    }

    /// The link speed, decoded exactly as `igc_mac.c:681-712`
    /// (`igc_get_speed_and_duplex_copper`) decodes it.
    ///
    /// The encoding is not monotonic: `IGC_STATUS_SPEED_1000` means *1 Gb/s or
    /// faster*, and `IGC_STATUS_SPEED_2500` is an extra bit that distinguishes
    /// 2.5 Gb/s from 1 Gb/s.  A `SPEED_2500` without `SPEED_1000` therefore
    /// decodes as 10 Mb/s — which is what the vendor driver does, and the test
    /// below pins that behaviour rather than inventing a nicer one.
    pub const fn speed(self) -> Speed {
        if self.0 & bits::STATUS_SPEED_1000 != 0 {
            if self.0 & bits::STATUS_SPEED_2500 != 0 {
                Speed::Mbit2500
            } else {
                Speed::Mbit1000
            }
        } else if self.0 & bits::STATUS_SPEED_100 != 0 {
            Speed::Mbit100
        } else {
            Speed::Mbit10
        }
    }
}

/// `IGC_CTRL`: device control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceControl(u32);

impl DeviceControl {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_CTRL_SLU` is set, so the MAC is driving the link up.
    pub const fn set_link_up(self) -> bool {
        self.0 & bits::CTRL_SLU != 0
    }

    /// `IGC_CTRL_RST` is set, so a global reset is in progress or pending.
    pub const fn reset_asserted(self) -> bool {
        self.0 & bits::CTRL_RST != 0
    }

    /// `IGC_CTRL_GIO_MASTER_DISABLE` is set.
    pub const fn master_disabled(self) -> bool {
        self.0 & bits::CTRL_GIO_MASTER_DISABLE != 0
    }

    /// The same value with `CTRL.RST` asserted: `igc_reset_hw_base` writes
    /// exactly this (`igc_base.c:43`).
    pub const fn with_reset(self) -> Self {
        Self(self.0 | bits::CTRL_RST)
    }

    /// The same value with `CTRL.GIO_MASTER_DISABLE` asserted:
    /// `igc_disable_pcie_master` writes exactly this (`igc_mac.c:27`).
    pub const fn with_master_disabled(self) -> Self {
        Self(self.0 | bits::CTRL_GIO_MASTER_DISABLE)
    }

    /// The same value with `CTRL.RST` clear.
    ///
    /// Nothing writes this: the hardware clears the bit itself when the reset
    /// completes, which is what the reset sequence waits for.
    pub const fn without_reset(self) -> Self {
        Self(self.0 & !bits::CTRL_RST)
    }

    /// The same value with `CTRL.SLU` set and both force bits clear: the link
    /// request `igc_setup_copper_link_base` makes (`igc_base.c:322-326`).
    pub const fn with_link_up_autonegotiated(self) -> Self {
        Self((self.0 | bits::CTRL_SLU) & !(bits::CTRL_FRCSPD | bits::CTRL_FRCDPX))
    }
}

/// `IGC_EECD`: NVM/flash control and status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmControl(u32);

impl NvmControl {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_EECD_AUTO_RD`: the hardware has finished reading the NVM into the
    /// receive-address registers.
    ///
    /// `igc_mac.c:650-674` (`igc_get_auto_rd_done`) polls this bit after a
    /// reset; it is the only signal this driver has that the MAC address in
    /// `IGC_RAL(0)`/`IGC_RAH(0)` is the one the NVM holds.
    pub const fn auto_read_done(self) -> bool {
        self.0 & bits::EECD_AUTO_RD != 0
    }

    /// `IGC_EECD_FLASH_DETECTED_I225`: flash, rather than an EEPROM, is
    /// attached.  `igc_i225.c:459` (`igc_get_flash_presence_i225`) reads it.
    pub const fn flash_detected(self) -> bool {
        self.0 & bits::EECD_FLASH_DETECTED_I225 != 0
    }

    /// `IGC_EECD_SIZE_EX_MASK`: the NVM size field.
    ///
    /// What the field's value *means* is not established here: `igc_base.c`
    /// derives a word size from it inside `igc_init_nvm_params_base`, but the
    /// driver never needs the size, so this returns the raw field rather than
    /// a byte count this project cannot source.
    pub const fn size_field(self) -> u16 {
        ((self.0 & bits::EECD_SIZE_EX_MASK) >> bits::EECD_SIZE_EX_SHIFT) as u16
    }
}

/// `IGC_RAH(0)`: the upper half of receive address 0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceiveAddressHigh(u32);

impl ReceiveAddressHigh {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_RAH_AV`: the receive address in this register pair is valid.
    pub const fn address_valid(self) -> bool {
        self.0 & bits::RAH_AV != 0
    }

    /// The upper two bytes of the address.  `igc_nvm.c:144-145` takes them the
    /// same way.
    pub const fn address_high(self) -> u16 {
        (self.0 & bits::RAH_ADDR_MASK) as u16
    }
}

/// The six bytes of a receive address, assembled the way `igc_nvm.c`
/// `igc_read_mac_addr` assembles them: the low register holds bytes 0..3
/// little-endian, the high register holds bytes 4..5.
pub const fn assemble_receive_address(low: u32, high: ReceiveAddressHigh) -> [u8; 6] {
    let mut address = [0u8; 6];
    address[0] = low as u8;
    address[1] = (low >> 8) as u8;
    address[2] = (low >> 16) as u8;
    address[3] = (low >> 24) as u8;
    let upper = high.address_high();
    address[4] = upper as u8;
    address[5] = (upper >> 8) as u8;
    address
}

/// A command word for `IGC_MDIC`, built the way `igc_phy.c` builds one.
///
/// Two functions in the vendor driver build these: `igc_read_phy_reg_mdic`
/// (`igc_phy.c:544`) and `igc_write_phy_reg_mdic` (`igc_phy.c:600`).  Both then
/// poll `IGC_MDIC_READY` and check `IGC_MDIC_ERROR`, which is what
/// [`MdicResult`] decodes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MdicCommand(u32);

impl MdicCommand {
    /// A read command for `register` on the PHY at `phy`.
    ///
    /// The register field is five bits (`MAX_PHY_REG_ADDRESS`), so a larger
    /// number cannot be encoded.  The vendor driver refuses one
    /// (`igc_phy.c:549`); this constructor returns `None` instead of masking.
    pub const fn read(phy: u32, register: u16) -> Option<Self> {
        Self::build(phy, register, bits::MDIC_OP_READ, 0)
    }

    /// A write command carrying `data`.
    pub const fn write(phy: u32, register: u16, data: u16) -> Option<Self> {
        Self::build(phy, register, bits::MDIC_OP_WRITE, data)
    }

    const fn build(phy: u32, register: u16, op: u32, data: u16) -> Option<Self> {
        if register as u32 > bits::MAX_PHY_REG_ADDRESS || phy > 31 {
            return None;
        }
        Some(Self(
            (data as u32)
                | ((register as u32) << bits::MDIC_REG_SHIFT)
                | (phy << bits::MDIC_PHY_SHIFT)
                | op,
        ))
    }

    /// The word to store in `IGC_MDIC`.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// What `IGC_MDIC` says after a command: done, failed, or not finished.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MdicResult(u32);

impl MdicResult {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The PHY answered.
    pub const fn ready(self) -> bool {
        self.0 & bits::MDIC_READY != 0
    }

    /// The PHY reported an error.
    pub const fn error(self) -> bool {
        self.0 & bits::MDIC_ERROR != 0
    }

    /// The data word the PHY returned.
    pub const fn data(self) -> u16 {
        (self.0 & bits::MDIC_DATA_MASK) as u16
    }
}

/// `IGC_RCTL`: receive control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceiveControl(u32);

impl ReceiveControl {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The value `igc_setup_rctl` programs for a single queue with no `RXALL`
    /// request and the default multicast filter type (`igc_main.c:835-866`).
    ///
    /// Three of the bits it sets deserve naming, because a reader will look
    /// for them:
    ///
    /// * `RCTL.SECRC` is set, so the hardware strips the Ethernet CRC and the
    ///   length in the receive descriptor does *not* include it.  A driver
    ///   that sets this bit must not subtract four from that length, and this
    ///   one does not;
    /// * `RCTL.LPE` (long packet enable) is set, which is what makes a frame
    ///   longer than 1518 bytes receivable at all;
    /// * the multicast-offset field is left at zero, because the vendor
    ///   driver's value comes from `hw->mac.mc_filter_type` and **nothing in
    ///   the igc driver ever assigns that field** -- the structure is zeroed
    ///   at probe -- so the vendor driver programmes a zero too.
    pub const fn setup_value() -> Self {
        Self(
            bits::RCTL_EN
                | bits::RCTL_BAM
                | bits::RCTL_RDMTS_HALF
                | bits::RCTL_SECRC
                | bits::RCTL_LPE,
        )
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_RCTL_EN`: the receive path is enabled.
    pub const fn enabled(self) -> bool {
        self.0 & bits::RCTL_EN != 0
    }

    /// `IGC_RCTL_SECRC`: the hardware strips the CRC whose length it then
    /// omits from the descriptor.
    pub const fn strips_crc(self) -> bool {
        self.0 & bits::RCTL_SECRC != 0
    }

    /// The same value with the unicast-promiscuous bit set or cleared.
    pub const fn with_unicast_promiscuous(self, on: bool) -> Self {
        if on {
            Self(self.0 | bits::RCTL_UPE)
        } else {
            Self(self.0 & !bits::RCTL_UPE)
        }
    }

    /// The same value with the multicast-promiscuous bit set or cleared.
    pub const fn with_multicast_promiscuous(self, on: bool) -> Self {
        if on {
            Self(self.0 | bits::RCTL_MPE)
        } else {
            Self(self.0 & !bits::RCTL_MPE)
        }
    }
}

/// `IGC_TCTL`: transmit control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransmitControl(u32);

impl TransmitControl {
    /// Interpret a raw register value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The value `igc_setup_tctl` programs, from the value it read
    /// (`igc_main.c:882-899`): clear the collision-threshold field, set
    /// pad-short-packets, retransmit-on-late-collision, the documented
    /// collision threshold, and transmit enable.  It is a read-modify-write in
    /// the vendor driver and is one here too, so a bit the firmware set that
    /// this driver does not know about survives.
    pub const fn setup_value(current: u32) -> Self {
        let cleared = current & !bits::TCTL_CT;
        Self(
            cleared
                | bits::TCTL_PSP
                | bits::TCTL_RTLC
                | (bits::COLLISION_THRESHOLD << bits::CT_SHIFT)
                | bits::TCTL_EN,
        )
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// `IGC_TCTL_EN`: the transmit path is enabled.
    pub const fn enabled(self) -> bool {
        self.0 & bits::TCTL_EN != 0
    }

    /// The collision-threshold field, decoded.
    pub const fn collision_threshold(self) -> u32 {
        (self.0 & bits::TCTL_CT) >> bits::CT_SHIFT
    }
}

/// `IGC_SRRCTL(0)`: split-receive control for one queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SplitReceiveControl(u32);

impl SplitReceiveControl {
    /// The value `igc_configure_rx_ring` programs for one buffer per packet
    /// (`igc_main.c:625-706`): the header-size field holds `header_bytes` in
    /// 64-byte units, the packet-size field holds `packet_bytes` in 1 KiB
    /// units, and the descriptor type is `ADV_ONEBUF`.
    pub const fn one_buffer(packet_bytes: u32, header_bytes: u32) -> Self {
        Self(
            ((packet_bytes / 1024) & bits::SRRCTL_BSIZEPKT_MASK)
                | (((header_bytes / 64) << 8) & bits::SRRCTL_BSIZEHDR_MASK)
                | bits::SRRCTL_DESCTYPE_ADV_ONEBUF,
        )
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The packet buffer size in bytes, decoded from its 1 KiB field.
    pub const fn packet_bytes(self) -> u32 {
        (self.0 & bits::SRRCTL_BSIZEPKT_MASK) * 1024
    }

    /// The descriptor type field, decoded.
    pub const fn descriptor_type(self) -> u32 {
        (self.0 & bits::SRRCTL_DESCTYPE_MASK) >> 25
    }
}

/// The receive prefetch threshold (`igc.h:480`).
pub const RX_PTHRESH: u32 = 8;
/// The receive host threshold (`igc.h:481`).
pub const RX_HTHRESH: u32 = 8;
/// The transmit prefetch threshold (`igc.h:482`).
pub const TX_PTHRESH: u32 = 8;
/// The transmit host threshold (`igc.h:483`).
pub const TX_HTHRESH: u32 = 1;
/// The receive write-back threshold (`igc.h:484`).
pub const RX_WTHRESH: u32 = 4;
/// The transmit write-back threshold (`igc.h:485`).
pub const TX_WTHRESH: u32 = 16;

/// `IGC_RXDCTL(0)` / `IGC_TXDCTL(0)`: queue thresholds and the enable bit.
///
/// The two registers have the same layout and `igc_configure_rx_ring` and
/// `igc_configure_tx_ring` build both the same way: the prefetch threshold in
/// bits 2:0, the host threshold in bits 10:8, the write-back threshold in bits
/// 20:16, then the queue-enable bit (`igc_main.c:625-706` and `:728-765`, with
/// the constants from `igc.h:480-485`).
///
/// The vendor driver ORs the three constants in *without* masking them, so the
/// field widths are inferred rather than stated by a mask in the header, and
/// the inference has a sharp edge worth writing down: `IGC_RX_PTHRESH` is 8 and
/// `IGC_RX_HTHRESH` is 8, which **do not fit in three-bit fields**.  A first
/// version of this module used three-bit masks for them, truncated both values
/// to zero, and produced `0x02040000` where the vendor driver produces
/// `0x02040808` -- the tests below caught it.  The masks here are the narrowest
/// ones that pass the vendor's own constants through unchanged: four bits for
/// the prefetch and host thresholds, five for the write-back threshold.  The
/// tests pin the exact words `igc_configure_rx_ring` and
/// `igc_configure_tx_ring` produce, which is the check that matters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueControl(u32);

impl QueueControl {
    /// The prefetch threshold field: at least four bits, because the vendor
    /// value 8 does not fit in three.
    pub const PTHRESH_MASK: u32 = 0x0000_000f;
    /// The host threshold field: four bits, for the same reason.
    pub const HTHRESH_MASK: u32 = 0x0000_0f00;
    /// The write-back threshold field: five bits, which holds the vendor's
    /// largest value, 16.
    pub const WTHRESH_MASK: u32 = 0x001f_0000;

    /// The receive queue's thresholds, as `igc_configure_rx_ring` sets them.
    pub const fn receive_defaults() -> Self {
        Self(
            (RX_PTHRESH & Self::PTHRESH_MASK)
                | ((RX_HTHRESH << 8) & Self::HTHRESH_MASK)
                | ((RX_WTHRESH << 16) & Self::WTHRESH_MASK),
        )
    }

    /// The transmit queue's thresholds, as `igc_configure_tx_ring` sets them.
    pub const fn transmit_defaults() -> Self {
        Self(
            (TX_PTHRESH & Self::PTHRESH_MASK)
                | ((TX_HTHRESH << 8) & Self::HTHRESH_MASK)
                | ((TX_WTHRESH << 16) & Self::WTHRESH_MASK),
        )
    }

    /// The value both configure functions write first, to stop the queue
    /// touching a half-configured descriptor ring (`igc_main.c:625`, `:728`).
    pub const fn disabled() -> Self {
        Self(0)
    }

    /// The same value with the queue-enable bit set.
    pub const fn with_queue_enable(self) -> Self {
        Self(self.0 | bits::RXDCTL_QUEUE_ENABLE)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The queue-enable bit.
    pub const fn queue_enabled(self) -> bool {
        self.0 & bits::RXDCTL_QUEUE_ENABLE != 0
    }

    /// The prefetch threshold field.
    pub const fn prefetch_threshold(self) -> u32 {
        self.0 & Self::PTHRESH_MASK
    }

    /// The host threshold field.
    pub const fn host_threshold(self) -> u32 {
        (self.0 & Self::HTHRESH_MASK) >> 8
    }

    /// The write-back threshold field.
    pub const fn write_back_threshold(self) -> u32 {
        (self.0 & Self::WTHRESH_MASK) >> 16
    }
}

/// A 64-bit descriptor ring address, split into the two registers the hardware
/// takes it in.
///
/// `igc_configure_rx_ring` and `igc_configure_tx_ring` split it the same way:
/// the low dword into `*DBAL` and the high dword into `*DBAH`
/// (`igc_main.c:625`, `:728`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingBase(u64);

impl RingBase {
    /// Split a bus address.
    pub const fn new(address: u64) -> Self {
        Self(address)
    }

    /// The low dword, for `IGC_*DBAL`.
    pub const fn low(self) -> u32 {
        self.0 as u32
    }

    /// The high dword, for `IGC_*DBAH`.
    pub const fn high(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// The address.
    pub const fn address(self) -> u64 {
        self.0
    }
}

/// Descriptor ring geometry, in the terms the hardware registers take.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingLength(usize);

impl RingLength {
    /// The byte length of `descriptors` descriptors of `descriptor_bytes`
    /// each.
    ///
    /// `igc_configure_rx_ring` writes `ring->count * sizeof(desc)`
    /// (`igc_main.c:625`), and the vendor driver rounds the *allocation* up to
    /// 4 KiB (`igc_setup_rx_resources`, `igc_main.c:534`) but not the value it
    /// puts in the register.  This constructor refuses a length that is not a
    /// multiple of 128 bytes, which is the alignment a descriptor ring length
    /// must have for the hardware to accept it; the vendor driver does not
    /// check, and a ring of 8 descriptors of 16 bytes is exactly 128.
    pub const fn new(descriptors: usize, descriptor_bytes: usize) -> Option<Self> {
        let bytes = descriptors * descriptor_bytes;
        if descriptors == 0 || descriptor_bytes == 0 || !bytes.is_multiple_of(128) {
            return None;
        }
        Some(Self(bytes))
    }

    /// The byte length, for `IGC_RDLEN`/`IGC_TDLEN`.
    pub const fn bytes(self) -> u32 {
        self.0 as u32
    }

    /// How many descriptors the ring holds.
    pub const fn descriptors(self, descriptor_bytes: usize) -> usize {
        self.0 / descriptor_bytes
    }
}

/// A mapped window of the device aperture.
///
/// The window is not the BAR; it is the part of the BAR this driver mapped,
/// and every access is checked against it.
#[derive(Clone, Copy, Debug)]
pub struct RegisterWindow {
    base: usize,
    len: usize,
}

impl RegisterWindow {
    /// Take a window over an aperture that is already mapped.
    ///
    /// # Safety
    ///
    /// `base .. base + len` must be a live region of at least `len` bytes,
    /// mapped as device memory (or, in a test, as ordinary memory standing in
    /// for it) for as long as this value is used, and it must be aligned well
    /// enough for the 32-bit accesses the register table performs.  The host
    /// tests use this to point a window at an ordinary buffer, which is the
    /// only way to exercise volatile register access on a machine that has no
    /// such device.
    pub const unsafe fn from_mapped(base: usize, len: usize) -> Self {
        Self { base, len }
    }

    /// The length of the mapped window in bytes.
    pub const fn len(self) -> usize {
        self.len
    }

    /// The address the window starts at.
    pub const fn base(self) -> usize {
        self.base
    }

    /// Whether the window contains `register` whole.
    pub const fn contains(self, register: Register) -> bool {
        register.fits_in(self.len)
    }

    /// Read a register, or `None` when it cannot be read.
    ///
    /// `None` means one of three things, and the caller can tell them apart
    /// from the table: the register lies outside the mapped window, it is
    /// declared write-only, or it is not a register this driver names.
    /// Reading a register this part does not implement, or one whose engine is
    /// not clocked, is a normal read: it returns whatever the bus returns and
    /// changes nothing -- except for an [`Access::ReadToClear`] register, where
    /// the read *is* the state change.  That is why the register table, not the
    /// read, is where the safety argument lives.
    pub fn read(self, register: Register) -> Option<u32> {
        if !register.is_readable() {
            return None;
        }
        let address = register.offset() as usize;
        if address + 4 > self.len {
            return None;
        }
        // SAFETY: the offset was just checked to lie inside a window this
        // value promises is mapped device memory, and a 32-bit register access
        // is naturally aligned because the register table is dword aligned.
        // Nothing else in the kernel aliases it: the aperture belongs to this
        // device and is mapped uncached, so a read cannot be served from a
        // stale cache line.
        let value = unsafe { core::ptr::read_volatile((self.base + address) as *const u32) };
        // The load may not be moved past a later volatile access, and this
        // fence states that whatever the caller does next happens after the
        // device answered.  x86_64 needs no processor fence here: uncached
        // accesses are strongly ordered against each other.
        compiler_fence(Ordering::Acquire);
        Some(value)
    }

    /// Write a register, returning whether the write happened.
    ///
    /// A read-only register, or one outside the window, is refused; the caller
    /// gets `false` rather than a silent no-op.
    ///
    /// The write is posted: it may still be in flight when this returns.  A
    /// driver that needs the device to have seen it reads the register back,
    /// which is the ordering primitive this architecture actually offers --
    /// there is no completion signal for an MMIO store, and an `mfence` would
    /// not create one.  `igc_regs.h:342` spells the same primitive `wrfl()`:
    /// the vendor driver's flush is a read of `IGC_STATUS`.
    pub fn write(self, register: Register, value: u32) -> bool {
        if !register.is_writable() {
            return false;
        }
        let address = register.offset() as usize;
        if address + 4 > self.len {
            return false;
        }
        // Keep the value from being computed after the store, and the store
        // from being sunk past whatever the caller does next.
        compiler_fence(Ordering::Release);
        // SAFETY: as for `read`, and the register table is the only source of
        // offsets, so the store is to a dword-aligned register inside the
        // mapped aperture.
        unsafe { core::ptr::write_volatile((self.base + address) as *mut u32, value) };
        compiler_fence(Ordering::SeqCst);
        true
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    /// A buffer standing in for an aperture, sized exactly like the window the
    /// driver maps.  Device memory is ordinary memory with rules, so the
    /// volatile access path can be tested against it as long as the window is
    /// built to point at it.
    struct Scratch {
        words: vec::Vec<u32>,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                words: vec![0; WINDOW_BYTES / 4],
            }
        }

        fn window(&mut self) -> RegisterWindow {
            // SAFETY: `words` is a live, 4-byte aligned buffer of exactly
            // `WINDOW_BYTES` bytes that outlives both the window and this
            // borrow, and the windows built over it are the only way the
            // buffer is reached once the borrow ends.
            unsafe { RegisterWindow::from_mapped(self.words.as_mut_ptr() as usize, WINDOW_BYTES) }
        }
    }

    #[test]
    fn every_named_register_is_inside_the_mapped_window() {
        for register in NAMED {
            assert!(
                register.fits_in(WINDOW_BYTES),
                "{} does not fit in the window",
                register.name()
            );
        }
        assert_eq!(NAMED_SPAN, 0x0e02c, "the highest named register end");
    }

    #[test]
    fn every_named_register_is_dword_aligned_which_is_how_the_phy_register_was_caught() {
        // `IGC_GPHY_VERSION` is listed in igc_regs.h beside the identity
        // registers and is *not* an aperture register: its offset 0x1e is not
        // dword aligned, and it is read through MDIC by igc_phy.c
        // igc_read_phy_fw_version.  The assertion in `Register::declare` is
        // what turned that into a compile error rather than a probe that read
        // a PHY register as if it were an aperture register.
        assert!(
            !(0x0001eu32).is_multiple_of(4),
            "0x1e is the PHY register's offset, and it is not an aperture offset"
        );
        for register in NAMED {
            assert!(
                register.offset().is_multiple_of(4),
                "{} is not dword aligned",
                register.name()
            );
        }
    }

    #[test]
    fn the_table_is_ordered_and_free_of_duplicates() {
        for (index, register) in NAMED.iter().enumerate() {
            for other in &NAMED[index + 1..] {
                assert_ne!(
                    register.offset(),
                    other.offset(),
                    "{} and {} name the same offset",
                    register.name(),
                    other.name()
                );
            }
        }
        // Named in offset order, so a reader can diff the table against the
        // datasheet's address map without sorting it first.
        for pair in NAMED.windows(2) {
            assert!(
                pair[0].offset() < pair[1].offset(),
                "{} at {:#x} comes before {} at {:#x}",
                pair[0].name(),
                pair[0].offset(),
                pair[1].name(),
                pair[1].offset(),
            );
        }
    }

    #[test]
    fn the_identify_set_is_the_side_effect_free_subset_of_the_table() {
        assert!(!IDENTIFY.is_empty());
        for register in IDENTIFY {
            let named = named(register.name())
                .unwrap_or_else(|| panic!("{} is not in the table", register.name()));
            assert_eq!(named.offset(), register.offset(), "{}", register.name());
            assert!(
                register.is_readable(),
                "{} is in the probe's list but is not readable",
                register.name()
            );
            assert!(
                !register.read_has_side_effect(),
                "{} clears something when read, so the probe must not read it",
                register.name()
            );
            assert_eq!(
                register.access(),
                Access::ReadOnly,
                "the identify phase reads and never writes",
            );
        }
        // The read-to-clear register is named but not probed: this is the
        // property the display probe's register module states for its own
        // side-effecting registers, applied to a NIC.
        assert!(named("IGC_ICR").unwrap().read_has_side_effect());
        assert!(named("IGC_ICR").unwrap().is_readable());
        assert!(!IDENTIFY.iter().any(|register| register.name() == "IGC_ICR"));
        // And the write-only one cannot be read at all.
        let imc = named("IGC_IMC").unwrap();
        assert!(!imc.is_readable());
        assert!(imc.is_writable());
    }

    #[test]
    fn every_register_says_why_it_is_named_and_where_the_fact_came_from() {
        for register in NAMED {
            assert!(
                register.source().contains("igc_"),
                "{} does not cite the vendor source: {:?}",
                register.name(),
                register.source()
            );
            assert!(
                register.purpose().len() > 20,
                "{} has no usable purpose sentence: {:?}",
                register.name(),
                register.purpose(),
            );
            assert!(!register.meaning().describe().is_empty());
        }
    }

    #[test]
    fn the_table_names_exactly_the_registers_the_three_phases_need() {
        // Reset, link-up and one ring of each direction.  If a register is
        // added, this list has to be edited deliberately, which is the point.
        let expected: &[(&str, u32, Access)] = &[
            ("IGC_CTRL", 0x00000, Access::ReadWrite),
            ("IGC_STATUS", 0x00008, Access::ReadOnly),
            ("IGC_EECD", 0x00010, Access::ReadOnly),
            ("IGC_MDIC", 0x00020, Access::ReadWrite),
            ("IGC_RCTL", 0x00100, Access::ReadWrite),
            ("IGC_TCTL", 0x00400, Access::ReadWrite),
            ("IGC_ICR", 0x01500, Access::ReadToClear),
            ("IGC_IMC", 0x0150c, Access::WriteOnly),
            ("IGC_RXPBS", 0x02404, Access::ReadWrite),
            ("IGC_TXPBS", 0x03404, Access::ReadWrite),
            ("IGC_RLPML", 0x05004, Access::ReadWrite),
            ("IGC_RAL(0)", 0x05400, Access::ReadOnly),
            ("IGC_RAH(0)", 0x05404, Access::ReadOnly),
            ("IGC_RDBAL(0)", 0x0c000, Access::ReadWrite),
            ("IGC_RDBAH(0)", 0x0c004, Access::ReadWrite),
            ("IGC_RDLEN(0)", 0x0c008, Access::ReadWrite),
            ("IGC_SRRCTL(0)", 0x0c00c, Access::ReadWrite),
            ("IGC_RDH(0)", 0x0c010, Access::ReadWrite),
            ("IGC_RDT(0)", 0x0c018, Access::ReadWrite),
            ("IGC_RXDCTL(0)", 0x0c028, Access::ReadWrite),
            ("IGC_TDBAL(0)", 0x0e000, Access::ReadWrite),
            ("IGC_TDBAH(0)", 0x0e004, Access::ReadWrite),
            ("IGC_TDLEN(0)", 0x0e008, Access::ReadWrite),
            ("IGC_TDH(0)", 0x0e010, Access::ReadWrite),
            ("IGC_TDT(0)", 0x0e018, Access::ReadWrite),
            ("IGC_TXDCTL(0)", 0x0e028, Access::ReadWrite),
        ];
        assert_eq!(expected.len(), NAMED.len(), "{NAMED:#?}");
        for (name, offset, access) in expected {
            let register = named(name).unwrap_or_else(|| panic!("{name} is missing"));
            assert_eq!(register.offset(), *offset, "{name}");
            assert_eq!(register.access(), *access, "{name}");
        }
    }

    #[test]
    fn the_bit_values_match_the_linux_defines() {
        // igc_defines.h, by line.
        assert_eq!(bits::CTRL_GIO_MASTER_DISABLE, 0x0000_0004); // :97
        assert_eq!(bits::CTRL_RST, 0x0400_0000); // :132
        assert_eq!(bits::CTRL_PHY_RST, 0x8000_0000); // :134
        assert_eq!(bits::CTRL_SLU, 0x0000_0040); // :135
        assert_eq!(bits::CTRL_FRCSPD, 0x0000_0800); // :136
        assert_eq!(bits::CTRL_FRCDPX, 0x0000_1000); // :137
        assert_eq!(bits::CTRL_RFCE, 0x0800_0000); // :140
        assert_eq!(bits::CTRL_TFCE, 0x1000_0000); // :141
        assert_eq!(bits::STATUS_FD, 0x0000_0001); // :223
        assert_eq!(bits::STATUS_LU, 0x0000_0002); // :224
        assert_eq!(bits::STATUS_FUNC_MASK, 0x0000_000c); // :225
        assert_eq!(bits::STATUS_FUNC_SHIFT, 2); // :226
        assert_eq!(bits::STATUS_TXOFF, 0x0000_0010); // :227
        assert_eq!(bits::STATUS_SPEED_100, 0x0000_0040); // :228
        assert_eq!(bits::STATUS_SPEED_1000, 0x0000_0080); // :229
        assert_eq!(bits::STATUS_SPEED_2500, 0x0040_0000); // :230
        assert_eq!(bits::STATUS_GIO_MASTER_ENABLE, 0x0008_0000); // :99
        assert_eq!(bits::EECD_AUTO_RD, 0x0000_0200); // :187
        assert_eq!(bits::EECD_SIZE_EX_MASK, 0x0000_7800); // :193
        assert_eq!(bits::EECD_SIZE_EX_SHIFT, 11); // :194
        assert_eq!(bits::EECD_FLASH_DETECTED_I225, 0x0008_0000); // :197
        assert_eq!(bits::RAH_AV, 0x8000_0000); // :114
        assert_eq!(bits::RAH_ADDR_MASK, 0x0000_ffff); // :108
        assert_eq!(bits::RXPBSIZE_DEFAULT, 0x0000_00a2); // :399
        assert_eq!(bits::TXPBSIZE_DEFAULT, 0x0400_0014); // :400
        assert_eq!(bits::MAX_JUMBO_FRAME_SIZE, 0x2600); // :147
        assert_eq!(bits::TCTL_EN, 0x0000_0002); // :330
        assert_eq!(bits::TCTL_PSP, 0x0000_0008); // :331
        assert_eq!(bits::TCTL_CT, 0x0000_0ff0); // :332
        assert_eq!(bits::TCTL_RTLC, 0x0100_0000); // :334
        assert_eq!(bits::COLLISION_THRESHOLD, 15); // :217
        assert_eq!(bits::CT_SHIFT, 4); // :218
        assert_eq!(bits::RCTL_EN, 0x0000_0002); // :349
        assert_eq!(bits::RCTL_SBP, 0x0000_0004); // :350
        assert_eq!(bits::RCTL_UPE, 0x0000_0008); // :351
        assert_eq!(bits::RCTL_MPE, 0x0000_0010); // :352
        assert_eq!(bits::RCTL_LPE, 0x0000_0020); // :353
        assert_eq!(bits::RCTL_RDMTS_HALF, 0x0000_0000); // :357
        assert_eq!(bits::RCTL_BAM, 0x0000_8000); // :358
        assert_eq!(bits::RCTL_SZ_256, 0x0003_0000); // :391
        assert_eq!(bits::RCTL_MO_SHIFT, 12); // :393
        assert_eq!(bits::RCTL_SECRC, 0x0400_0000); // :397
        assert_eq!(bits::RXD_STAT_DD, 0x0000_0001); // :304
        assert_eq!(bits::RXD_STAT_EOP, 0x0000_0002); // :366
        assert_eq!(bits::TXD_STAT_DD, 0x0000_0001); // :315
        assert_eq!(bits::ADVTXD_MAC_TSTAMP, 0x0008_0000); // igc_base.h:36
        assert_eq!(bits::ADVTXD_DTYP_DATA, 0x0030_0000); // igc_base.h:45
        assert_eq!(bits::ADVTXD_DCMD_EOP, 0x0100_0000); // igc_base.h:46
        assert_eq!(bits::ADVTXD_DCMD_IFCS, 0x0200_0000); // igc_base.h:47
        assert_eq!(bits::ADVTXD_DCMD_RS, 0x0800_0000); // igc_base.h:48
        assert_eq!(bits::ADVTXD_DCMD_DEXT, 0x2000_0000); // igc_base.h:49
        assert_eq!(bits::ADVTXD_DCMD_VLE, 0x4000_0000); // igc_base.h:50
        assert_eq!(bits::ADVTXD_DCMD_TSE, 0x8000_0000); // igc_base.h:51
        assert_eq!(bits::ADVTXD_PAYLEN_SHIFT, 14); // igc_base.h:52
        assert_eq!(bits::TXD_POPTS_IXSM, 0x0000_0001); // :309
        assert_eq!(bits::TXD_POPTS_TXSM, 0x0000_0002); // :310
        assert_eq!(bits::MDIC_DATA_MASK, 0x0000_ffff); // :644
        assert_eq!(bits::MDIC_REG_MASK, 0x001f_0000); // :645
        assert_eq!(bits::MDIC_REG_SHIFT, 16); // :646
        assert_eq!(bits::MDIC_PHY_MASK, 0x03e0_0000); // :647
        assert_eq!(bits::MDIC_PHY_SHIFT, 21); // :648
        assert_eq!(bits::MDIC_OP_WRITE, 0x0400_0000); // :649
        assert_eq!(bits::MDIC_OP_READ, 0x0800_0000); // :650
        assert_eq!(bits::MDIC_READY, 0x1000_0000); // :651
        assert_eq!(bits::MDIC_ERROR, 0x4000_0000); // :652
        assert_eq!(bits::MAX_PHY_REG_ADDRESS, 0x1f); // :619
        assert_eq!(bits::GEN_POLL_TIMEOUT, 1920); // :620
        assert_eq!(bits::MII_SR_LINK_STATUS, 0x0004); // :628
        assert_eq!(bits::MII_SR_AUTONEG_COMPLETE, 0x0020); // :629
        assert_eq!(bits::NWAY_AR_PAUSE, 0x0400); // :165
        assert_eq!(bits::NWAY_AR_ASM_DIR, 0x0800); // :166
        assert_eq!(bits::NWAY_LPAR_PAUSE, 0x0400); // :169
        assert_eq!(bits::NWAY_LPAR_ASM_DIR, 0x0800); // :170
        assert_eq!(bits::CR_1000T_FD_CAPS, 0x0200); // :174
        assert_eq!(bits::SR_1000T_REMOTE_RX_STATUS, 0x1000); // :177
        assert_eq!(bits::COPPER_LINK_UP_LIMIT, 10); // :91
        assert_eq!(bits::MASTER_DISABLE_TIMEOUT, 800); // :95
        assert_eq!(bits::AUTO_READ_DONE_TIMEOUT, 10); // :186
        assert_eq!(bits::INTERRUPT_MASK_ALL, 0xffff_ffff); // igc_base.c:32
        // igc_base.h and igc_base.c, for the descriptor dials.
        assert_eq!(bits::TXDCTL_QUEUE_ENABLE, 0x0200_0000); // igc_base.h:89
        assert_eq!(bits::TXDCTL_SWFLUSH, 0x0400_0000); // igc_base.h:90
        assert_eq!(bits::RXDCTL_QUEUE_ENABLE, 0x0200_0000); // igc_base.h:93
        assert_eq!(bits::RXDCTL_SWFLUSH, 0x0400_0000); // igc_base.h:94
        assert_eq!(bits::SRRCTL_BSIZEPKT_MASK, 0x0000_007f); // igc_base.h:97
        assert_eq!(bits::SRRCTL_BSIZEHDR_MASK, 0x0000_3f00); // igc_base.h:100
        assert_eq!(bits::SRRCTL_DESCTYPE_MASK, 0x0e00_0000); // igc_base.h:103
        assert_eq!(bits::SRRCTL_DESCTYPE_ADV_ONEBUF, 0x0200_0000); // igc_base.h:104
        // igc.h, for the queue thresholds.
        assert_eq!((RX_PTHRESH, RX_HTHRESH, RX_WTHRESH), (8, 8, 4)); // igc.h:480-484
        assert_eq!((TX_PTHRESH, TX_HTHRESH, TX_WTHRESH), (8, 1, 16)); // igc.h:482-485
    }

    #[test]
    fn the_speed_field_decodes_the_way_the_vendor_driver_decodes_it() {
        // igc_mac.c igc_get_speed_and_duplex_copper.
        assert_eq!(DeviceStatus::new(0).speed(), Speed::Mbit10);
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_100).speed(),
            Speed::Mbit100
        );
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_1000).speed(),
            Speed::Mbit1000
        );
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_1000 | bits::STATUS_SPEED_2500).speed(),
            Speed::Mbit2500
        );
        // Both low bits set: 1 Gb/s wins, because the vendor driver tests
        // SPEED_1000 first.
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_100 | bits::STATUS_SPEED_1000).speed(),
            Speed::Mbit1000
        );
        // The odd one: the 2.5 Gb/s discriminator without the 1 Gb/s bit is
        // 10 Mb/s in this encoding, and this test pins that rather than
        // silently "fixing" it into something the hardware does not promise.
        assert_eq!(
            DeviceStatus::new(bits::STATUS_SPEED_2500).speed(),
            Speed::Mbit10
        );
        assert_eq!(Speed::Mbit2500.mbps(), 2500);
    }

    #[test]
    fn the_status_fields_decode() {
        let status =
            DeviceStatus::new(bits::STATUS_LU | bits::STATUS_FD | bits::STATUS_SPEED_1000 | (2 << 2));
        assert!(status.link_up());
        assert!(status.full_duplex());
        assert!(!status.transmit_paused());
        assert!(
            !status.master_enabled(),
            "GIO master enable is a separate bit and this value does not set it"
        );
        assert!(DeviceStatus::new(bits::STATUS_GIO_MASTER_ENABLE).master_enabled());
        assert_eq!(status.function_id(), 2);
        assert_eq!(status.speed(), Speed::Mbit1000);

        let down = DeviceStatus::new((3 << 2) | bits::STATUS_TXOFF);
        assert!(!down.link_up());
        assert!(!down.full_duplex());
        assert!(down.transmit_paused());
        // GIO_MASTER_ENABLE clear is what the reset sequence waits for.
        assert!(!down.master_enabled());
        assert_eq!(down.function_id(), 3);
    }

    #[test]
    fn the_control_and_nvm_fields_decode() {
        let control = DeviceControl::new(bits::CTRL_SLU | bits::CTRL_RST);
        assert!(control.set_link_up());
        assert!(control.reset_asserted());
        assert!(!control.master_disabled());
        // The three transformations the reset and link sequences use.
        assert!(DeviceControl::new(0).with_reset().reset_asserted());
        assert!(
            !DeviceControl::new(bits::CTRL_RST)
                .without_reset()
                .reset_asserted()
        );
        assert!(DeviceControl::new(0).with_master_disabled().master_disabled());
        let linked =
            DeviceControl::new(bits::CTRL_FRCSPD | bits::CTRL_FRCDPX).with_link_up_autonegotiated();
        assert!(linked.set_link_up());
        assert_eq!(
            linked.raw() & (bits::CTRL_FRCSPD | bits::CTRL_FRCDPX),
            0,
            "forcing speed and duplex is cleared so the PHY autonegotiates",
        );

        let eecd = NvmControl::new(bits::EECD_AUTO_RD | bits::EECD_FLASH_DETECTED_I225 | (5 << 11));
        assert!(eecd.auto_read_done());
        assert!(eecd.flash_detected());
        assert_eq!(eecd.size_field(), 5);

        let bare = NvmControl::new(0);
        assert!(!bare.auto_read_done());
        assert!(!bare.flash_detected());
        assert_eq!(bare.size_field(), 0);
    }

    #[test]
    fn the_receive_address_assembles_little_endian_low_then_high() {
        // igc_nvm.c igc_read_mac_addr: byte 0 is the low byte of RAL, byte 5
        // is the high byte of RAH's low half.
        let low = 0x3322_1100u32;
        let high = ReceiveAddressHigh::new(0x8000_0000 | 0x5544);
        assert_eq!(
            assemble_receive_address(low, high),
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]
        );
        assert!(high.address_valid());
        assert_eq!(high.address_high(), 0x5544);

        // The valid bit is not part of the address, and a zero address with
        // the valid bit clear must not become a valid one.
        let invalid = ReceiveAddressHigh::new(0);
        assert!(!invalid.address_valid());
        assert_eq!(assemble_receive_address(0, invalid), [0; 6]);
    }

    #[test]
    fn the_mdic_command_encodes_the_two_fields_and_refuses_what_does_not_fit() {
        // igc_phy.c igc_read_phy_reg_mdic builds `(offset << 16) | (phy << 21)
        // | OP_READ`, and the write path adds the data word in the low 16 bits.
        let read = MdicCommand::read(MDIC_PHY_ADDRESS, mii::STATUS).unwrap();
        assert_eq!(read.raw(), (0x01 << 16) | bits::MDIC_OP_READ);
        let write = MdicCommand::write(MDIC_PHY_ADDRESS, mii::AUTONEG_ADV, 0x01e0).unwrap();
        assert_eq!(write.raw(), 0x01e0 | (0x04 << 16) | bits::MDIC_OP_WRITE);
        // A five-bit register field: 0x20 does not fit, and the vendor driver
        // refuses it rather than truncating.
        assert!(MdicCommand::read(MDIC_PHY_ADDRESS, 0x20).is_none());
        assert!(MdicCommand::read(32, mii::STATUS).is_none());
        assert!(MdicCommand::read(31, mii::STATUS).is_some());
        // The PHY address field sits where the mask says it does.
        let high = MdicCommand::read(31, mii::STATUS).unwrap().raw();
        assert_eq!((high & bits::MDIC_PHY_MASK) >> bits::MDIC_PHY_SHIFT, 31);
        assert_eq!((high & bits::MDIC_REG_MASK) >> bits::MDIC_REG_SHIFT, 0x01);
    }

    #[test]
    fn the_mdic_result_decodes_ready_error_and_data() {
        let done = MdicResult::new(bits::MDIC_READY | 0x1234);
        assert!(done.ready());
        assert!(!done.error());
        assert_eq!(done.data(), 0x1234);

        let failed = MdicResult::new(bits::MDIC_ERROR);
        assert!(!failed.ready());
        assert!(failed.error());

        // The data field is the low 16 bits only: a status bit must never be
        // mistaken for part of the value.
        let noisy =
            MdicResult::new(bits::MDIC_READY | bits::MDIC_ERROR | bits::MDIC_OP_READ | 0x00ff);
        assert_eq!(noisy.data(), 0x00ff);
    }

    #[test]
    fn the_receive_control_value_is_the_one_the_vendor_driver_programs() {
        // igc_main.c igc_setup_rctl, with mc_filter_type 0 (never assigned in
        // the vendor driver) and no RXALL request:
        //   EN | BAM | RDMTS_HALF | (0 << MO_SHIFT) | SECRC | LPE
        // with SBP and SZ_256 cleared.
        let rctl = ReceiveControl::setup_value();
        assert_eq!(rctl.raw(), 0x0400_8022);
        assert!(rctl.enabled());
        assert!(rctl.strips_crc());
        assert_eq!(
            rctl.raw() & bits::RCTL_SBP,
            0,
            "store-bad-packets stays clear"
        );
        assert_eq!(
            rctl.raw() & bits::RCTL_SZ_256,
            0,
            "the size field stays clear"
        );
        assert_eq!(
            rctl.raw() & (0x3 << bits::RCTL_MO_SHIFT),
            0,
            "the multicast-offset field is zero"
        );
        // Promiscuous modes are a later decision, and they only ever add bits.
        assert_eq!(
            rctl.with_unicast_promiscuous(true).raw() & bits::RCTL_UPE,
            bits::RCTL_UPE
        );
        assert_eq!(
            rctl.with_multicast_promiscuous(true).raw() & bits::RCTL_MPE,
            bits::RCTL_MPE
        );
        assert_eq!(rctl.with_unicast_promiscuous(false).raw(), rctl.raw());
    }

    #[test]
    fn the_transmit_control_value_is_a_read_modify_write_of_what_was_there() {
        // igc_main.c igc_setup_tctl.  Starting from the value the firmware
        // left, the collision-threshold field is replaced and nothing else is
        // cleared.
        let firmware = bits::TCTL_COLD | 0x3;
        let tctl = TransmitControl::setup_value(firmware);
        assert!(tctl.enabled());
        assert_eq!(tctl.collision_threshold(), bits::COLLISION_THRESHOLD);
        assert_eq!(
            tctl.raw() & bits::TCTL_COLD,
            firmware & bits::TCTL_COLD,
            "the collision-distance field survives"
        );
        assert_eq!(tctl.raw() & bits::TCTL_PSP, bits::TCTL_PSP);
        assert_eq!(tctl.raw() & bits::TCTL_RTLC, bits::TCTL_RTLC);
        // From zero, the vendor driver's value is PSP | RTLC | 15<<4 | EN.
        assert_eq!(TransmitControl::setup_value(0).raw(), 0x0100_00fa);
    }

    #[test]
    fn the_split_receive_value_matches_the_one_configure_rx_ring_programs() {
        // igc_main.c igc_configure_rx_ring: BSIZEHDR(IGC_RX_HDR_LEN = 256)
        // | BSIZEPKT(2048) | DESCTYPE_ADV_ONEBUF.
        let srrctl = SplitReceiveControl::one_buffer(2048, 256);
        assert_eq!(srrctl.raw(), (4 << 8) | 2 | (1 << 25));
        assert_eq!(srrctl.packet_bytes(), 2048);
        assert_eq!(srrctl.descriptor_type(), 1);
        // Larger buffers round to the 1 KiB field they are encoded in, which
        // is what the vendor macros do when they divide.
        assert_eq!(
            SplitReceiveControl::one_buffer(3072, 256).packet_bytes(),
            3072
        );
        assert_eq!(SplitReceiveControl::one_buffer(1023, 64).packet_bytes(), 0);
    }

    #[test]
    fn the_queue_control_values_are_the_words_the_configure_functions_write() {
        // igc_main.c igc_configure_rx_ring: PTHRESH | HTHRESH << 8 |
        // WTHRESH << 16 | QUEUE_ENABLE, with 8/8/4.
        let rx = QueueControl::receive_defaults().with_queue_enable();
        assert_eq!(rx.raw(), 0x0204_0808);
        assert!(rx.queue_enabled());
        assert_eq!(rx.prefetch_threshold(), 8);
        assert_eq!(rx.host_threshold(), 8);
        assert_eq!(rx.write_back_threshold(), 4);
        // igc_configure_tx_ring: the same shape with 8/1/16.
        let tx = QueueControl::transmit_defaults().with_queue_enable();
        assert_eq!(tx.raw(), 0x0210_0108);
        assert_eq!(tx.prefetch_threshold(), 8);
        assert_eq!(tx.host_threshold(), 1);
        assert_eq!(tx.write_back_threshold(), 16);
        // Both configure functions start by writing zero to stop the queue.
        assert_eq!(QueueControl::disabled().raw(), 0);
        assert!(!QueueControl::disabled().queue_enabled());
        // The fields must not truncate the vendor's own constants: 8 does not
        // fit in three bits, and a three-bit mask silently produced
        // 0x02040000 instead of 0x02040808.
        assert_eq!(QueueControl::receive_defaults().raw(), 0x0004_0808);
        assert_eq!(QueueControl::transmit_defaults().raw(), 0x0010_0108);
    }

    #[test]
    fn the_ring_geometry_matches_what_the_registers_take() {
        // 256 descriptors of 16 bytes is 4096 bytes, the vendor default
        // (igc.h:442-447) and a multiple of 128.
        let ring = RingLength::new(256, 16).unwrap();
        assert_eq!(ring.bytes(), 4096);
        assert_eq!(ring.descriptors(16), 256);
        // 8 descriptors is the smallest ring whose byte length is a multiple
        // of 128 -- the alignment a descriptor ring length must have.
        assert!(RingLength::new(8, 16).is_some());
        assert!(RingLength::new(7, 16).is_none());
        assert!(RingLength::new(0, 16).is_none());
        assert!(RingLength::new(256, 0).is_none());

        let base = RingBase::new(0x0000_0001_f7a0_4000);
        assert_eq!(base.low(), 0xf7a0_4000);
        assert_eq!(base.high(), 0x0000_0001);
        assert_eq!(base.address(), 0x0000_0001_f7a0_4000);
        // A 32-bit address has a zero high dword, which is what a 32-bit BAR
        // produces and what the register must hold.
        assert_eq!(RingBase::new(0xf7a0_4000).high(), 0);
    }

    #[test]
    fn a_read_returns_what_the_aperture_holds_and_a_refused_read_returns_nothing() {
        let mut scratch = Scratch::new();
        let window = scratch.window();
        scratch.words[0x00008 / 4] = 0xdead_beef;
        scratch.words[0x05400 / 4] = 0x3322_1100;
        scratch.words[0x01500 / 4] = 0x0000_0001;
        assert_eq!(window.read(named("IGC_STATUS").unwrap()), Some(0xdead_beef));
        assert_eq!(window.read(named("IGC_RAL(0)").unwrap()), Some(0x3322_1100));
        // Read-to-clear reads are allowed -- the reset sequence needs one --
        // and the access mode is what says so.
        assert_eq!(window.read(named("IGC_ICR").unwrap()), Some(1));
        // The write-only mask register cannot be read at all.
        assert_eq!(window.read(named("IGC_IMC").unwrap()), None);
    }

    #[test]
    fn a_read_of_a_register_outside_the_window_is_refused() {
        let mut scratch = Scratch::new();
        let start = scratch.words.as_mut_ptr() as usize;
        // A window that stops one dword short of the status register.
        let window = unsafe { RegisterWindow::from_mapped(start, 0x8) };
        assert!(window.read(named("IGC_CTRL").unwrap()).is_some());
        assert_eq!(window.read(named("IGC_STATUS").unwrap()), None);
        assert_eq!(window.read(named("IGC_RAL(0)").unwrap()), None);
        assert!(!window.contains(named("IGC_EECD").unwrap()));
        // `IGC_CTRL` is writable in this phase, so a refusal here has to come
        // from the window, not the access rule: with an 0x8-byte window the
        // control register is reachable and the status register is not.
        assert!(window.write(named("IGC_CTRL").unwrap(), 0));
        assert!(!window.write(named("IGC_STATUS").unwrap(), 1));
    }

    #[test]
    fn writes_are_allowed_exactly_where_the_table_says_they_are() {
        let mut scratch = Scratch::new();
        let window = scratch.window();
        // Writable registers take the value.
        assert!(window.write(named("IGC_CTRL").unwrap(), 0x0400_0040));
        assert_eq!(window.read(named("IGC_CTRL").unwrap()), Some(0x0400_0040));
        assert!(window.write(
            named("IGC_IMC").unwrap(),
            bits::INTERRUPT_MASK_ALL
        ));
        // Read-only registers refuse it and leave the aperture alone.
        scratch.words[0x00008 / 4] = 0x1234_5678;
        assert!(!window.write(named("IGC_STATUS").unwrap(), 0xffff_ffff));
        assert_eq!(window.read(named("IGC_STATUS").unwrap()), Some(0x1234_5678));
        assert!(!window.write(named("IGC_RAH(0)").unwrap(), 0xffff_ffff));
        // As does a register outside the window, even a writable one: the
        // window is the second of the two rules the table enforces.
        let short =
            unsafe { RegisterWindow::from_mapped(scratch.words.as_mut_ptr() as usize, 0x400) };
        assert!(short.contains(named("IGC_RCTL").unwrap()));
        assert!(short.write(named("IGC_RCTL").unwrap(), 1));
        assert!(!short.write(named("IGC_TCTL").unwrap(), 1));
        assert_eq!(short.read(named("IGC_TCTL").unwrap()), None);
    }

    #[test]
    fn a_register_is_named_by_its_linux_spelling() {
        assert_eq!(named("IGC_STATUS").unwrap().meaning(), Meaning::DeviceStatus);
        assert_eq!(
            named("IGC_RAL(0)").unwrap().meaning(),
            Meaning::ReceiveAddressLow
        );
        assert_eq!(
            named("IGC_RAH(0)").unwrap().meaning(),
            Meaning::ReceiveAddressHigh
        );
        assert_eq!(named("IGC_RCTL").unwrap().group(), Group::Receive);
        assert_eq!(named("IGC_TCTL").unwrap().group(), Group::Transmit);
        assert_eq!(named("IGC_MDIC").unwrap().group(), Group::Phy);
        assert_eq!(named("IGC_IMC").unwrap().group(), Group::Interrupt);
        assert_eq!(named("IGC_RDT(0)").unwrap().group(), Group::Receive);
        // A register named for a queue other than zero is not in this table:
        // the driver uses one queue, and the table says so by not having it.
        assert!(named("IGC_RDT(1)").is_none());
        assert!(named("IGC_RETA(0)").is_none());
        assert!(named("igc_status").is_none(), "names are exact");
        assert_eq!(
            at_offset(0x0c018).unwrap().name(),
            "IGC_RDT(0)",
            "the table is the only way from an offset back to a register"
        );
        assert!(at_offset(0x0c01c).is_none());
    }
}
