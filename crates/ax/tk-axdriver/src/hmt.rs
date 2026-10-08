//! FreeBSD `sys/dev/hid/hmt.c` multitouch mapping policy.
//! HID field decoding and Linux ABS_MT code conversion are shared with USB HID.

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
}

impl MultiTouch {
    // upstream: hmt.c hmt_hid_parse()
    pub(super) fn probe(report: &Report) -> Option<Self> {
        let slot_range = report.absolute_range(0x2f)?;
        let slots = u16::try_from(slot_range.1.saturating_sub(slot_range.0) + 1).ok()?;
        if slots == 0
            || !report.has_code(3, 0x35) // ABS_MT_POSITION_X
            || !report.has_code(3, 0x36) // ABS_MT_POSITION_Y
            || !report.has_code(3, 0x39)
            // ABS_MT_TRACKING_ID
            || !report.has_mt_tip_switch()
        {
            return None;
        }
        let kind = if report.is_touchpad() {
            Type::Touchpad
        } else {
            Type::Touchscreen
        };
        Some(Self { kind, slots })
    }

    // upstream: hmt.c hmt_set_input_mode()
    pub(super) const fn advertises_type(self) -> bool {
        true
    }
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
            0xc0,
        ];
        let mut descriptor = vec![0x05, 0x0d, 0x09, 0x05, 0xa1, 1];
        descriptor.extend_from_slice(&finger);
        descriptor.extend_from_slice(&finger);
        descriptor.push(0xc0);
        let mut report = Report::parse(&descriptor).unwrap();
        let hmt = MultiTouch::probe(&report).unwrap();
        assert_eq!(hmt.kind, Type::Touchpad);
        assert_eq!(hmt.slots, 32);
        assert!(report.has_code(3, 0x2f)); // ABS_MT_SLOT
        let mut events = VecDeque::new();
        assert!(report.decode(&[1, 7, 50, 60, 0, 8, 70, 80], &mut events));
        let triples: Vec<_> = events
            .iter()
            .map(|event| (event.event_type, event.code, event.value as i32))
            .collect();
        assert!(triples.contains(&(3, 0x39, 7)));
        assert!(triples.contains(&(3, 0x35, 50)));
        assert!(triples.contains(&(3, 0x36, 60)));
        assert!(!triples.contains(&(3, 0x35, 70)));
        events.clear();
        assert!(report.decode(&[0, 7, 50, 60, 0, 8, 70, 80], &mut events));
        let triples: Vec<_> = events
            .iter()
            .map(|event| (event.event_type, event.code, event.value as i32))
            .collect();
        assert!(triples.contains(&(3, 0x39, -1)));
    }
}
