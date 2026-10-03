//! Bounded HID short-item parser for pointer Input reports (HID 1.11 §6.2.2).
//! Keyboard boot protocol remains separate. Unknown reports are ignored; long
//! items, arrays and oversized layouts are refused rather than guessed.
use alloc::{collections::VecDeque, vec::Vec};

use axdriver_base::{DevError, DevResult};
use axdriver_input::Event;
#[derive(Clone, Copy, Default)]
struct Global {
    page: u32,
    min: i32,
    max: i32,
    size: u32,
    count: u32,
    id: u8,
}
#[derive(Clone, Copy)]
struct Field {
    id: u8,
    bit: u16,
    size: u8,
    page: u32,
    usage: u32,
    min: i32,
    max: i32,
    relative: bool,
}
pub(super) struct PointerReport {
    fields: Vec<Field>,
    bits: [u16; 16],
    ids: bool,
}
fn signed(value: u32, bytes: usize) -> i32 {
    if bytes == 0 {
        0
    } else {
        ((value << (32 - bytes * 8)) as i32) >> (32 - bytes * 8)
    }
}
impl PointerReport {
    pub(super) fn parse(bytes: &[u8]) -> DevResult<Self> {
        let mut global = Global::default();
        let mut saved = Vec::new();
        let mut usages = Vec::new();
        let mut usage_min = None;
        let mut usage_max = None;
        let mut fields = Vec::new();
        let mut bits = [0u16; 16];
        let mut ids = false;
        let mut offset = 0;
        let mut pointer = false;
        let mut collections = Vec::new();
        while offset < bytes.len() {
            let prefix = bytes[offset];
            offset += 1;
            if prefix == 0xfe {
                return Err(DevError::Unsupported);
            }
            let width = match prefix & 3 {
                3 => 4,
                x => x as usize,
            };
            let data = bytes
                .get(offset..offset + width)
                .ok_or(DevError::InvalidParam)?;
            offset += width;
            let value = data
                .iter()
                .enumerate()
                .fold(0, |v, (i, b)| v | (u32::from(*b) << (i * 8)));
            let tag = prefix >> 4;
            match (prefix >> 2 & 3, tag) {
                (1, 0) => global.page = value,
                (1, 1) => global.min = signed(value, width),
                (1, 2) => {
                    global.max = if global.min < 0 {
                        signed(value, width)
                    } else {
                        i32::try_from(value).map_err(|_| DevError::Unsupported)?
                    }
                }
                (1, 7) => global.size = value,
                (1, 8) => {
                    if !(1..16).contains(&value) {
                        return Err(DevError::Unsupported);
                    }
                    global.id = value as u8;
                    ids = true;
                }
                (1, 9) => global.count = value,
                (1, 10) => {
                    if saved.len() == 4 {
                        return Err(DevError::Unsupported);
                    }
                    saved.push(global);
                }
                (1, 11) => global = saved.pop().ok_or(DevError::InvalidParam)?,
                (2, 0) => {
                    if usages.len() == 32 {
                        return Err(DevError::Unsupported);
                    }
                    usages.push(value);
                }
                (2, 1) => usage_min = Some(value),
                (2, 2) => usage_max = Some(value),
                (0, 10) => {
                    if collections.len() == 8 {
                        return Err(DevError::Unsupported);
                    }
                    collections.push(pointer);
                    pointer |=
                        global.page == 1 && usages.first().is_some_and(|u| matches!(u, 1 | 2));
                }
                (0, 12) => pointer = collections.pop().ok_or(DevError::InvalidParam)?,
                (0, 8) => {
                    if global.size == 0 || global.size > 32 || global.count > 32 {
                        return Err(DevError::Unsupported);
                    }
                    let input = &mut bits[global.id as usize];
                    if global.id != 0 && *input == 0 {
                        *input = 8;
                    }
                    if let (Some(min), Some(max)) = (usage_min, usage_max) {
                        if max < min || max - min >= 32 {
                            return Err(DevError::Unsupported);
                        }
                        usages.extend(min..=max);
                    }
                    for index in 0..global.count as usize {
                        let usage = usages.get(index).copied().unwrap_or(0);
                        if pointer
                            && value & 1 == 0
                            && value & 2 != 0
                            && ((global.page == 1 && matches!(usage, 0x30 | 0x31 | 0x38))
                                || (global.page == 9 && (1..=8).contains(&usage)))
                        {
                            if fields.len() == 32 || global.max < global.min {
                                return Err(DevError::Unsupported);
                            }
                            fields.push(Field {
                                id: global.id,
                                bit: *input,
                                size: global.size as u8,
                                page: global.page,
                                usage,
                                min: global.min,
                                max: global.max,
                                relative: value & 4 != 0,
                            });
                        }
                        *input = input
                            .checked_add(global.size as u16)
                            .ok_or(DevError::InvalidParam)?;
                        if *input > 512 {
                            return Err(DevError::Unsupported);
                        }
                    }
                }
                _ => {}
            }
            if prefix >> 2 & 3 == 0 {
                usages.clear();
                usage_min = None;
                usage_max = None;
            }
        }
        if fields
            .iter()
            .filter(|f| f.page == 1 && f.usage == 0x30)
            .count()
            == 0
            || !saved.is_empty()
            || !collections.is_empty()
            || (ids && bits[0] != 0)
        {
            return Err(DevError::Unsupported);
        }
        Ok(Self { fields, bits, ids })
    }
    pub(super) fn absolute_range(&self, axis: u8) -> Option<(i32, i32)> {
        self.fields
            .iter()
            .find(|f| f.page == 1 && f.usage == u32::from(axis) + 0x30 && !f.relative)
            .map(|f| (f.min, f.max))
    }
    pub(super) fn relative_axis(&self, code: u16) -> bool {
        let usage = match code {
            0 | 1 => u32::from(code) + 0x30,
            8 => 0x38,
            _ => return false,
        };
        self.fields
            .iter()
            .any(|f| f.page == 1 && f.usage == usage && f.relative)
    }
    pub(super) fn decode(
        &self,
        report: &[u8],
        buttons: &mut u8,
        events: &mut VecDeque<Event>,
    ) -> bool {
        let id = if self.ids {
            report.first().copied().unwrap_or(255)
        } else {
            0
        };
        let Some(bits) = self.bits.get(id as usize).filter(|bits| **bits != 0) else {
            return false;
        };
        if report.len() * 8 < usize::from(*bits) {
            return false;
        }
        let mut next_buttons = *buttons;
        let mut has_buttons = false;
        for field in self.fields.iter().filter(|field| field.id == id) {
            let mut value = 0u32;
            for bit in 0..field.size as usize {
                let offset = field.bit as usize + bit;
                value |= u32::from((report[offset / 8] >> (offset % 8)) & 1) << bit;
            }
            let value = if field.min < 0 {
                ((value << (32 - field.size)) as i32) >> (32 - field.size)
            } else {
                value as i32
            };
            if field.page == 9 {
                has_buttons = true;
                let mask = 1 << (field.usage - 1);
                if value != 0 {
                    next_buttons |= mask;
                } else {
                    next_buttons &= !mask;
                }
            } else {
                let (ty, code) = if field.usage == 0x38 {
                    (2, 8)
                } else {
                    (
                        if field.relative { 2 } else { 3 },
                        (field.usage - 0x30) as u16,
                    )
                };
                if !field.relative || value != 0 {
                    super::hid::push(events, ty, code, value.clamp(field.min, field.max));
                }
            }
        }
        if has_buttons {
            for bit in 0..8 {
                if (*buttons ^ next_buttons) & (1 << bit) != 0 {
                    super::hid::push(
                        events,
                        1,
                        0x110 + bit,
                        i32::from(next_buttons & (1 << bit) != 0),
                    );
                }
            }
            *buttons = next_buttons;
        }
        if !events.is_empty() {
            super::hid::push(events, 0, 0, 0);
        }
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn pointer(absolute: bool, id: Option<u8>) -> Vec<u8> {
        let mut d = alloc::vec![0x05, 1, 0x09, 2, 0xa1, 1];
        if let Some(id) = id {
            d.extend([0x85, id]);
        }
        d.extend([
            0x05, 9, 0x19, 1, 0x29, 3, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 3, 0x81, 2, 0x75, 5, 0x95,
            1, 0x81, 1, 0x05, 1, 0x09, 0x30, 0x09, 0x31,
        ]);
        if absolute {
            d.extend([0x15, 0, 0x26, 0xff, 0x7f, 0x75, 16, 0x95, 2, 0x81, 2]);
        } else {
            d.extend([0x15, 0x81, 0x25, 0x7f, 0x75, 8, 0x95, 2, 0x81, 6]);
        }
        d.push(0xc0);
        d
    }
    #[test]
    fn absolute_and_relative_report_ids_have_independent_bit_layouts() {
        let mut d = pointer(false, Some(1));
        d.extend(pointer(true, Some(2)));
        let parser = PointerReport::parse(&d).unwrap();
        assert_eq!(parser.absolute_range(0), Some((0, 32767)));
        let mut buttons = 0;
        let mut events = VecDeque::new();
        assert!(parser.decode(&[1, 1, 0xff, 2], &mut buttons, &mut events));
        assert!(
            events
                .iter()
                .any(|e| e.event_type == 2 && e.code == 0 && e.value == u32::MAX)
        );
        events.clear();
        assert!(parser.decode(&[2, 0, 0x34, 0x12, 0x78, 0x56], &mut buttons, &mut events));
        assert!(
            events
                .iter()
                .any(|e| e.event_type == 3 && e.code == 0 && e.value == 0x1234)
        );
        assert!(!parser.decode(&[2, 0, 1], &mut buttons, &mut events));
        assert!(!parser.decode(&[3, 0, 1, 2], &mut buttons, &mut events));
    }
    #[test]
    fn boot_mouse_descriptor_and_hostile_layouts_are_bounded() {
        assert!(PointerReport::parse(&pointer(false, None)).is_ok());
        assert!(PointerReport::parse(&[0x75, 32, 0x95, 255, 0x81, 2]).is_err());
        assert!(PointerReport::parse(&[0x85, 255]).is_err());
        assert!(PointerReport::parse(&[0x26, 1]).is_err());
        assert!(PointerReport::parse(&[0xfe]).is_err());
    }
}
