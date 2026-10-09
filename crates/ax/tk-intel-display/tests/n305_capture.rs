// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Real capture bytes remain outside Git, since no redistribution grant exists.
use std::{fs, path::PathBuf};

use tk_intel_display::{
    intel_bios::Vbt,
    device::Port,
    dmc::DmcPlatform,
    opregion::{OpRegion, SIZE},
};

#[test]
#[ignore = "requires THEKERNEL_N305_CAPTURE private input; run explicitly"]
fn captured_vbt_identifies_both_legacy_tc_hdmi_ports() {
    let root = PathBuf::from(
        std::env::var_os("THEKERNEL_N305_CAPTURE").expect("set THEKERNEL_N305_CAPTURE"),
    );
    let debug = root.join("graphics/debugfs-0000:00:02.0");
    let data = fs::read(debug.join("i915_vbt")).unwrap();
    let vbt = Vbt::parse(&data).unwrap();
    assert_eq!(vbt.version, 249);
    assert_eq!(vbt.data().len(), 8701);
    assert!(vbt.checksum_valid());
    let defs = vbt
        .parse_general_definitions(13, DmcPlatform::AlderLakeN)
        .unwrap();
    assert_eq!(defs.record_size, 39);
    assert!(defs.record_size_expected);
    assert_eq!(defs.children().count(), 3);
    let b = defs.encoder(Port::B).unwrap();
    assert!(b.supports_dp() && !b.supports_hdmi());
    assert_eq!(b.aux_channel, 0x10);
    for (port, handle, pin) in [(Port::Tc1, 64, 9), (Port::Tc2, 32, 10)] {
        let c = defs.encoder(port).unwrap();
        assert!(c.supports_hdmi() && !c.supports_dp());
        assert!(!c.usb_type_c && !c.thunderbolt);
        assert_eq!(
            (c.handle, c.gmbus_pin(), c.hdmi_level_shift),
            (handle, Some(pin), 5)
        );
        println!("captured HDMI: {c:?}");
    }
    assert!(defs.encoder(Port::A).is_none());
    let region = fs::read(debug.join("i915_opregion")).unwrap();
    assert_eq!(region.len(), SIZE);
    let op = OpRegion::parse(&region, 0x100000).unwrap();
    assert_eq!((op.major, op.minor), (2, 1));
    let external = op.external_vbt().unwrap().unwrap();
    assert_eq!((external.physical, external.size), (0x102000, 8704));
    let from_op = op.vbt(Some(&data)).unwrap();
    assert_eq!(from_op.data(), vbt.data());
    // The Linux capture's TC1 route is a fact, not a combo-PHY assumption.
    let report = fs::read_to_string(debug.join("i915_display_info")).unwrap();
    assert!(report.contains("DDI TC1/PHY TC1"));
    assert!(report.contains("297000"));
}
