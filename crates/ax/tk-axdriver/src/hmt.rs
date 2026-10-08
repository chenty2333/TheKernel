//! FreeBSD `sys/dev/hid/hmt.c` multitouch mapping policy.
//! HID field decoding and Linux ABS_MT code conversion are shared with USB HID.

use alloc::vec::Vec;

use tk_i2c_hid::{Device, Error as HidError, Transport};

use crate::hid_report::Report;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Type {
    Touchpad,
    Touchscreen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MultiTouch {
    pub kind: Type,
    pub slots: u16,
    pub tlc_index: u8,
    /// Upstream `hmt_attach()` sets INPUT_PROP_BUTTONPAD when the feature
    /// report identifies an integrated click surface.
    pub clickpad: bool,
}

impl MultiTouch {
    // upstream: hmt.c hmt_hid_parse()
    pub(super) fn probe(report: &Report) -> Option<Self> {
        for tlc_index in 0..report.top_level_collection_count() {
            if !report.has_code_in_collection(tlc_index, 3, 0x35)
                || !report.has_code_in_collection(tlc_index, 3, 0x36)
                || !report.has_code_in_collection(tlc_index, 3, 0x39)
                || !report.has_mt_tip_switch_in_collection(tlc_index)
            {
                continue;
            }
            let kind = if report.is_touchpad_in_collection(tlc_index) {
                Type::Touchpad
            } else if report.is_touchscreen_in_collection(tlc_index) {
                Type::Touchscreen
            } else {
                continue;
            };
            let Some(max_contacts) = crate::hidbus::locate(
                report,
                crate::hid_report::ReportKind::Feature,
                0x0d,
                0x55,
                tlc_index,
                0,
            ) else {
                continue;
            };
            if max_contacts.flags & 2 == 0 || max_contacts.flags & 4 != 0 {
                continue;
            }
            let default_slots = if kind == Type::Touchscreen { 10 } else { 5 };
            let slots = if max_contacts.logical_max > 0 {
                u16::try_from(max_contacts.logical_max)
                    .unwrap_or(32)
                    .min(32)
            } else {
                default_slots
            };
            return Some(Self {
                kind,
                slots,
                tlc_index,
                clickpad: false,
            });
        }
        None
    }

    // upstream: hmt_attach() button-type feature report handling
    pub(super) fn set_button_type(&mut self, value: Option<i32>) {
        if let Some(value) = value {
            self.clickpad = value == 0;
        }
    }

    // upstream: hmt.c hmt_set_input_mode() / hconf.c hconf_set_feature_control()
    pub(super) fn set_input_mode<T: Transport>(
        self,
        device: &mut Device<T>,
        report: &Report,
        mode: u8,
    ) -> Result<(), HidError> {
        if self.kind != Type::Touchpad {
            return Ok(());
        }
        let (report_id, feature) = input_mode_feature_report(report, mode, self.tlc_index)?;
        device.set_report(3, report_id, &feature)
    }
}

fn input_mode_feature_report(
    report: &Report,
    mode: u8,
    tlc_index: u8,
) -> Result<(u8, Vec<u8>), HidError> {
    let mode_location = crate::hidbus::locate(
        report,
        crate::hid_report::ReportKind::Feature,
        0x0d,
        0x52,
        tlc_index,
        0,
    )
    .filter(|location| location.flags & 2 != 0 && location.flags & 4 == 0)
    .ok_or(HidError::Unsupported)?;
    let report_id = mode_location.report_id;
    let report_length = report.report_size(crate::hid_report::ReportKind::Feature, report_id);
    if report_length <= 1 {
        return Err(HidError::Unsupported);
    }
    let mut feature = Vec::new();
    feature
        .try_reserve_exact(report_length)
        .map_err(|_| HidError::PacketTooLarge)?;
    feature.resize(report_length, 0);
    feature[0] = report_id;
    for (usage, value) in [(0x52, u32::from(mode)), (0x57, 1), (0x58, 1)] {
        if let Some(location) = crate::hidbus::locate(
            report,
            crate::hid_report::ReportKind::Feature,
            0x0d,
            usage,
            tlc_index,
            0,
        ) && location.report_id == report_id
            && location.flags & 2 != 0
            && location.flags & 4 == 0
            && !crate::hid_report::put_hid_udata(&mut feature, location, value)
        {
            return Err(HidError::InvalidPacket);
        }
    }
    Ok((report_id, feature))
}

#[cfg(test)]
mod tests {
    use alloc::{collections::VecDeque, vec, vec::Vec};

    use super::*;

    #[test]
    fn finger_collections_require_mt_axes_and_advertise_slots() {
        let finger = [
            0x09, 0x22, 0xa1, 2, // Finger collection
            0x09, 0x42, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 1, 0x81, 2, // Tip Switch
            0x75, 7, 0x95, 1, 0x81, 3, // alignment padding
            0x09, 0x51, 0x15, 0, 0x25, 31, 0x75, 8, 0x95, 1, 0x81, 2, // Contact ID
            0x05, 1, 0x09, 0x30, 0x15, 0, 0x25, 100, 0x75, 8, 0x95, 1, 0x81, 2, // X
            0x09, 0x31, 0x15, 0, 0x25, 100, 0x75, 8, 0x95, 1, 0x81, 2, // Y
            0x05, 0x0d, 0x09, 0x48, 0x15, 0, 0x25, 40, 0x75, 8, 0x95, 1, 0x81, 2, // Width
            0x09, 0x49, 0x15, 0, 0x25, 40, 0x75, 8, 0x95, 1, 0x81, 2, // Height
            0xc0,
        ];
        let mut descriptor = vec![0x05, 0x0d, 0x09, 0x05, 0xa1, 1];
        descriptor.extend_from_slice(&[
            0x05, 0x0d, 0x09, 0x55, 0x15, 0, 0x25, 31, 0x75, 8, 0x95, 1, 0xb1, 2,
        ]);
        descriptor.extend_from_slice(&finger);
        descriptor.extend_from_slice(&finger);
        descriptor.push(0xc0);
        let mut report = Report::parse(&descriptor).unwrap();
        let mut hmt = MultiTouch::probe(&report).unwrap();
        assert_eq!(hmt.kind, Type::Touchpad);
        assert_eq!(hmt.slots, 31);
        assert!(!hmt.clickpad);
        hmt.set_button_type(Some(0));
        assert!(hmt.clickpad);
        hmt.set_button_type(Some(1));
        assert!(!hmt.clickpad);
        assert!(report.has_code(3, 0x2f)); // ABS_MT_SLOT
        let mut events = VecDeque::new();
        assert!(report.decode(&[1, 7, 50, 60, 10, 20, 0, 8, 70, 80, 0, 0], &mut events));
        let triples: Vec<_> = events
            .iter()
            .map(|event| (event.event_type, event.code, event.value as i32))
            .collect();
        assert!(triples.contains(&(3, 0x39, 7)));
        assert!(triples.contains(&(3, 0x35, 50)));
        assert!(triples.contains(&(3, 0x36, 60)));
        assert!(triples.contains(&(3, 0x30, 10)));
        assert!(triples.contains(&(3, 0x31, 5)));
        assert!(triples.contains(&(3, 0x34, 0)));
        assert!(!triples.contains(&(3, 0x35, 70)));
        events.clear();
        assert!(report.decode(&[0, 7, 50, 60, 10, 20, 0, 8, 70, 80, 0, 0], &mut events));
        let triples: Vec<_> = events
            .iter()
            .map(|event| (event.event_type, event.code, event.value as i32))
            .collect();
        assert!(triples.contains(&(3, 0x39, -1)));
    }

    #[test]
    fn hmt_probe_selects_its_top_level_collection_in_composite_descriptors() {
        let descriptor = [
            0x05, 1, 0x09, 6, 0xa1, 1, // TLC 0: keyboard
            0x85, 1, 0x05, 7, 0x09, 4, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 1, 0x81, 2, 0xc0, 0x05,
            0x0d, 0x09, 5, 0xa1, 1, // TLC 1: touchpad
            0x09, 0x55, 0x15, 0, 0x25, 10, 0x75, 8, 0x95, 1, 0xb1, 2, // Contact Count Max
            0x09, 0x22, 0xa1, 2, // Finger collection
            0x09, 0x42, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 1, 0x81, 2, // Tip Switch
            0x75, 7, 0x95, 1, 0x81, 3, // Padding
            0x09, 0x51, 0x15, 0, 0x25, 31, 0x75, 8, 0x95, 1, 0x81, 2, // Contact ID
            0x05, 1, 0x09, 0x30, 0x15, 0, 0x25, 100, 0x75, 8, 0x95, 1, 0x81, 2, // X
            0x09, 0x31, 0x15, 0, 0x25, 100, 0x75, 8, 0x95, 1, 0x81, 2, // Y
            0xc0, 0xc0,
        ];
        let report = Report::parse(&descriptor).unwrap();
        assert_eq!(report.top_level_collection_count(), 2);
        assert!(
            report
                .locate_usage(crate::hid_report::ReportKind::Input, 7, 4, 0)
                .is_some()
        );
        let hmt = MultiTouch::probe(&report).unwrap();
        assert_eq!(hmt.kind, Type::Touchpad);
        assert_eq!(hmt.tlc_index, 1);
    }

    #[test]
    fn hconf_input_mode_writes_shared_feature_controls() {
        let descriptor = [
            0x05, 0x0d, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x52, 0x15, 0, 0x25, 3, 0x75, 2,
            0x95, 1, 0xb1, 2, 0x09, 0x57, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 1, 0xb1, 2, 0x09, 0x58,
            0x15, 0, 0x25, 1, 0x75, 1, 0x95, 1, 0xb1, 2, 0xc0,
        ];
        let report = Report::parse(&descriptor).unwrap();
        let (report_id, feature) = input_mode_feature_report(&report, 3, 0).unwrap();
        assert_eq!(report_id, 1);
        assert_eq!(feature, [1, 0x0f]);
    }
}
