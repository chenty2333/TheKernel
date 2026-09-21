//! Phase 2 end to end, through the same controller model the GMBUS tests use.
//!
//! What these tests establish is the chain rather than the pieces: a monitor
//! behind a modelled GMBUS controller, read over the real transaction state
//! machine, validated, handed to the real mode layer, and reported.  The other
//! thing they establish is what happens when there is nothing there, which is
//! the state the target machine is most likely to be in the first time it
//! boots this kernel.

use alloc::{format, vec};

use super::*;
use crate::{
    drm::intel::{
        gmbus::tests::{FakeClock, FakeController, valid_edid},
        regs::SHOTPLUG_CTL_DDI,
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
/// The 18 bytes are the standard detailed timing descriptor for the CEA-861
/// 1080p60 format, in the field layout `drm::modes::edid` parses: pixel clock
/// in 10 kHz units, then the active and blanking fields split across a low byte
/// and a high nibble, then the sync offsets and widths, the image size in
/// millimetres, and the flags byte whose bits 4:3 say "digital separate sync"
/// and whose bits 2 and 1 say "positive H, positive V".
fn edid_with_1080p60(extension_count: u8) -> [u8; gmbus::EDID_BLOCK_LEN] {
    let mut block = valid_edid(extension_count);
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

#[test]
fn a_monitor_is_found_and_the_mode_layer_reads_the_bytes_this_kernel_asked_for() {
    let _guard = scheduler_test_context();
    let edid = edid_with_1080p60(0);
    let controller = FakeController::with_monitor(Pin::DdiB, &edid);
    let device = probe_one(bdf(), &controller, &FakeClock::new(), Narration::Boot);

    assert_eq!(device.monitor, Some(Pin::DdiB));
    assert_eq!(device.edid.map(|bytes| *bytes.bytes()), Some(edid));
    // The mode layer's own answer, from bytes that came over the modelled bus:
    // a strict parse of the sink's preferred timing, not the fallback.
    let plan = device.plan.expect("a plan for a monitor that answered");
    assert!(
        plan.strict,
        "the block was well formed: {:?}",
        plan.edid_error
    );
    assert!(plan.used_edid(), "the sink's own timing must be used");
    assert_eq!(plan.selection.mode.hdisplay, 1920);
    assert_eq!(plan.selection.mode.vdisplay, 1080);
    assert_eq!(plan.selection.mode.clock_khz, 148_500);

    // And the whole thing is legible in the report a person reads off the
    // screen or out of the debug file.
    let text = device.render();
    assert!(text.contains("display 0000:00:02.0"), "{text}");
    assert!(text.contains("monitor on pin 2"), "{text}");
    assert!(text.contains("1920x1080"), "{text}");
    assert!(text.contains("DetailedTiming"), "{text}");
}

#[test]
fn nothing_plugged_in_says_so_on_every_pin_and_names_the_power_well() {
    let _guard = scheduler_test_context();
    let controller = FakeController::bare();
    controller.detach();
    let device = probe_one(bdf(), &controller, &FakeClock::new(), Narration::Boot);

    assert_eq!(device.monitor, None);
    assert_eq!(device.edid, None);
    assert_eq!(device.extension, None);
    assert!(device.plan.is_none(), "no plan is made from no monitor");
    assert_eq!(
        device.pins.outcomes.len(),
        3,
        "DDI A, B and C are all asked"
    );
    let text = device.render();
    assert!(text.contains("no monitor"), "{text}");
    // The diagnosis the power workstream needs, in the words of the reference.
    assert!(text.contains("AUX_A"), "{text}");
    assert!(text.contains("not enabled"), "{text}");
    assert!(text.contains("AUX_C"), "{text}");
}

#[test]
fn hotplug_is_enabled_and_read_for_every_ddi_in_the_same_pass() {
    let _guard = scheduler_test_context();
    let controller = FakeController::with_monitor(Pin::DdiA, &edid_with_1080p60(0));
    let device = probe_one(bdf(), &controller, &FakeClock::new(), Narration::Boot);

    assert_eq!(device.hotplug.len(), Ddi::ALL.len());
    assert!(device.hotplug_errors.is_empty());
    for status in &device.hotplug {
        assert!(status.enabled, "{status:?}");
    }
    // All four enable bits are set, and nothing else in the register was
    // disturbed: the four DDIs share it.
    let control = controller.peek(SHOTPLUG_CTL_DDI).expect("in the window");
    for ddi in Ddi::ALL {
        assert_ne!(control & ddi.enable_bit(), 0, "{ddi}");
    }
    assert_eq!(control & 0x3333, 0, "no detect field was programmed");
}

#[test]
fn the_extension_block_the_sink_declares_is_read_and_parsed_with_the_base() {
    let _guard = scheduler_test_context();
    // A base block that declares one extension, and an extension that is a
    // valid block in its own right.
    let base = edid_with_1080p60(1);
    let mut extension = valid_edid(0);
    // Tag 0x70 is DisplayID, which this kernel's parser deliberately does not
    // decode: it checks the block's checksum and exposes it as unknown.  The
    // test is about the transport carrying a second block and the strict parse
    // being satisfied, not about CTA data-block validity.
    extension[0] = 0x70;
    extension[1] = 0x13;
    let sum = extension[..gmbus::EDID_BLOCK_LEN - 1]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    extension[gmbus::EDID_BLOCK_LEN - 1] = 0u8.wrapping_sub(sum);

    let controller = FakeController::with_monitor(Pin::DdiA, &base);
    let mut eeprom = vec![0u8; 2 * gmbus::EDID_BLOCK_LEN];
    eeprom[..gmbus::EDID_BLOCK_LEN].copy_from_slice(&base);
    eeprom[gmbus::EDID_BLOCK_LEN..].copy_from_slice(&extension);
    controller.load_eeprom(&eeprom);

    let device = probe_one(bdf(), &controller, &FakeClock::new(), Narration::Boot);
    assert_eq!(
        device.extension.map(|block| *block.bytes()),
        Some(extension)
    );
    // The base concatenated with the extension parses strictly, which is the
    // point of reading the extension at all: without it the strict parse fails
    // with `MissingExtensionBlocks`.
    let plan = device.plan.expect("a plan");
    assert!(plan.strict, "{:?}", plan.edid_error);
    assert_eq!(plan.selection.mode.hdisplay, 1920);
}

#[test]
fn a_window_that_does_not_reach_the_registers_is_reported_and_not_guessed_at() {
    let _guard = scheduler_test_context();
    let controller = FakeController::with_monitor(Pin::DdiA, &edid_with_1080p60(0));
    controller.set_window_len(0x1000);
    let device = probe_one(bdf(), &controller, &FakeClock::new(), Narration::Boot);

    assert_eq!(device.monitor, None);
    assert!(device.plan.is_none());
    // Every pin and every DDI reports the same refusal, by name: a window that
    // does not reach the registers is a bug in this kernel, not a hardware
    // condition, and it must not be reported as "no monitor".
    assert_eq!(device.hotplug_errors.len(), Ddi::ALL.len());
    for (ddi, error) in &device.hotplug_errors {
        assert!(matches!(error, HpdError::WindowTooSmall { .. }), "{ddi}");
    }
    let text = device.render();
    assert!(text.contains("does not reach"), "{text}");
    assert!(
        text.contains(&format!("register {}", "SHOTPLUG_CTL_DDI")) || text.contains("GMBUS0"),
        "{text}"
    );
}
