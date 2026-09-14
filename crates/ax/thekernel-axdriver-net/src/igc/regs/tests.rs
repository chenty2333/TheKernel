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
    assert_eq!(bits::TCTL_COLD, 0x003f_f000); // :333
    assert_eq!(bits::TCTL_RTLC, 0x0100_0000); // :334
    assert_eq!(bits::COLLISION_THRESHOLD, 15); // :217
    assert_eq!(bits::CT_SHIFT, 4); // :218
    assert_eq!(bits::RCTL_RST, 0x0000_0001); // :348
    assert_eq!(bits::RCTL_EN, 0x0000_0002); // :349
    assert_eq!(bits::RCTL_SBP, 0x0000_0004); // :350
    assert_eq!(bits::RCTL_UPE, 0x0000_0008); // :351
    assert_eq!(bits::RCTL_MPE, 0x0000_0010); // :352
    assert_eq!(bits::RCTL_LPE, 0x0000_0020); // :353
    // The loopback field is two bits from two adjacent lines (:354-355).
    assert_eq!(bits::RCTL_LBM, 0x0000_00c0); // :354-355
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
    assert_eq!(bits::NWAY_AR_10T_HD_CAPS, 0x0020); // :161
    assert_eq!(bits::NWAY_AR_10T_FD_CAPS, 0x0040); // :162
    assert_eq!(bits::NWAY_AR_100TX_HD_CAPS, 0x0080); // :163
    assert_eq!(bits::NWAY_AR_100TX_FD_CAPS, 0x0100); // :164
    assert_eq!(bits::NWAY_AR_PAUSE, 0x0400); // :165
    assert_eq!(bits::NWAY_AR_ASM_DIR, 0x0800); // :166
    assert_eq!(bits::NWAY_LPAR_PAUSE, 0x0400); // :169
    assert_eq!(bits::NWAY_LPAR_ASM_DIR, 0x0800); // :170
    assert_eq!(bits::CR_1000T_HD_CAPS, 0x0100); // :173
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
    assert!(
        DeviceControl::new(0)
            .with_master_disabled()
            .master_disabled()
    );
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
    let noisy = MdicResult::new(bits::MDIC_READY | bits::MDIC_ERROR | bits::MDIC_OP_READ | 0x00ff);
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
    assert!(window.write(named("IGC_IMC").unwrap(), bits::INTERRUPT_MASK_ALL));
    // Read-only registers refuse it and leave the aperture alone.
    scratch.words[0x00008 / 4] = 0x1234_5678;
    assert!(!window.write(named("IGC_STATUS").unwrap(), 0xffff_ffff));
    assert_eq!(window.read(named("IGC_STATUS").unwrap()), Some(0x1234_5678));
    assert!(!window.write(named("IGC_RAH(0)").unwrap(), 0xffff_ffff));
    // As does a register outside the window, even a writable one: the
    // window is the second of the two rules the table enforces.
    let short = unsafe { RegisterWindow::from_mapped(scratch.words.as_mut_ptr() as usize, 0x400) };
    assert!(short.contains(named("IGC_RCTL").unwrap()));
    assert!(short.write(named("IGC_RCTL").unwrap(), 1));
    assert!(!short.write(named("IGC_TCTL").unwrap(), 1));
    assert_eq!(short.read(named("IGC_TCTL").unwrap()), None);
}

#[test]
fn a_register_is_named_by_its_linux_spelling() {
    assert_eq!(
        named("IGC_STATUS").unwrap().meaning(),
        Meaning::DeviceStatus
    );
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
