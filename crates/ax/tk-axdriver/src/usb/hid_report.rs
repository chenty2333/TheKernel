//! Original, bounded HID report grammar/decoder (HID 1.11 sections 6.2.2, 8).
//! Arrays, bitmaps, report IDs and opaque long items share one parser. N305
//! report-protocol/CH9329 behavior is not hardware-validated.
use alloc::{collections::VecDeque, vec::Vec};

use axdriver_base::{DevError, DevResult};
use axdriver_input::Event;

use super::hid_usage::{Mapping, Usage, mapping};

const MAX_FIELDS: usize = 256;
const MAX_USAGES: usize = 1024;
#[derive(Clone, Copy, Default)]
struct Global {
    page: u32,
    min: i32,
    max: i32,
    size: u32,
    count: u32,
    id: u8,
}
enum Kind {
    Variable(Mapping),
    Array(Vec<Usage>),
}
struct Field {
    id: u8,
    bit: u16,
    size: u8,
    count: u16,
    min: i32,
    max: i32,
    app: Usage,
    relative: bool,
    kind: Kind,
    previous: Option<i32>,
    slot: Option<u8>,
}
pub(crate) struct Report {
    fields: Vec<Field>,
    bits: [u16; 256],
    ids: bool,
    // Report-ID ownership prevents a Consumer release from lifting keys held
    // by a separate keyboard report. Maximum retained owners is 1024.
    keys: Vec<(u8, u16)>,
    pointer: bool,
}
fn reserved<T>(count: usize) -> DevResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(count).map_err(|_| DevError::NoMemory)?;
    Ok(v)
}
fn signed(value: u32, width: usize) -> i32 {
    if width == 0 {
        0
    } else {
        ((value << (32 - width * 8)) as i32) >> (32 - width * 8)
    }
}
fn usage(value: u32, width: usize, page: u32) -> Usage {
    if width == 4 {
        Usage {
            page: value >> 16,
            code: value & 0xffff,
        }
    } else {
        Usage { page, code: value }
    }
}
fn extract(report: &[u8], bit: usize, size: u8, signed: bool) -> i32 {
    let mut v = 0u32;
    for i in 0..usize::from(size) {
        v |= u32::from((report[(bit + i) / 8] >> ((bit + i) % 8)) & 1) << i;
    }
    if signed {
        ((v << (32 - size)) as i32) >> (32 - size)
    } else {
        v as i32
    }
}
fn emit(events: &mut VecDeque<Event>, ty: u16, code: u16, value: i32) {
    events.push_back(Event {
        event_type: ty,
        code,
        value: value as u32,
    });
}
impl Report {
    pub(crate) fn parse(bytes: &[u8]) -> DevResult<Self> {
        if bytes.len() > 4096 {
            return Err(DevError::Unsupported);
        }
        let mut g = Global::default();
        let mut saved = reserved(8)?;
        let mut usages = reserved(MAX_USAGES)?;
        let mut ranges: Option<Usage> = None;
        let mut fields = reserved(MAX_FIELDS)?;
        let mut collections = reserved(16)?;
        let mut collection_slots = reserved(16)?;
        let mut current_slot = None;
        let mut next_slot = 0u8;
        let mut app = Usage::default();
        let mut pointer = false;
        let mut bits = [0u16; 256];
        let mut ids = false;
        let mut offset = 0;
        let mut usage_budget = 0usize;
        while offset < bytes.len() {
            let prefix = bytes[offset];
            offset += 1;
            if prefix == 0xfe {
                let length = usize::from(*bytes.get(offset).ok_or(DevError::InvalidParam)?);
                // long-item tag is deliberately opaque, including vendor items.
                bytes
                    .get(offset..offset + 2 + length)
                    .ok_or(DevError::InvalidParam)?;
                offset += 2 + length;
                continue;
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
                .fold(0, |v, (i, b)| v | (u32::from(*b) << (8 * i)));
            let class = prefix >> 2 & 3;
            let tag = prefix >> 4;
            match (class, tag) {
                (1, 0) => g.page = value,
                (1, 1) => g.min = signed(value, width),
                (1, 2) => {
                    g.max = if g.min < 0 {
                        signed(value, width)
                    } else {
                        i32::try_from(value).map_err(|_| DevError::Unsupported)?
                    }
                }
                (1, 7) => g.size = value,
                (1, 8) => {
                    if !(1..=255).contains(&value) {
                        return Err(DevError::InvalidParam);
                    }
                    g.id = value as u8;
                    ids = true;
                }
                (1, 9) => g.count = value,
                (1, 10) => {
                    if saved.len() == 8 {
                        return Err(DevError::Unsupported);
                    }
                    saved.push(g);
                }
                (1, 11) => g = saved.pop().ok_or(DevError::InvalidParam)?,
                (2, 0) => {
                    if usages.len() == MAX_USAGES {
                        return Err(DevError::Unsupported);
                    }
                    usages.push(usage(value, width, g.page));
                }
                (2, 1) => ranges = Some(usage(value, width, g.page)),
                (2, 2) => {
                    let min = ranges.take().ok_or(DevError::InvalidParam)?;
                    let max = usage(value, width, g.page);
                    if min.page != max.page
                        || max.code < min.code
                        || u64::from(max.code - min.code) + 1 > (MAX_USAGES - usages.len()) as u64
                    {
                        return Err(DevError::Unsupported);
                    }
                    for code in min.code..=max.code {
                        usages.push(Usage {
                            page: min.page,
                            code,
                        });
                    }
                }
                (2, 10) => return Err(DevError::Unsupported), // ambiguous alternate delimiter sets
                (0, 10) => {
                    if collections.len() == 16 {
                        return Err(DevError::Unsupported);
                    }
                    collections.push(app);
                    collection_slots.push(current_slot);
                    if matches!(value, 0 | 2)
                        && usages
                            .first()
                            .is_some_and(|usage| usage.page == 0x0d && usage.code == 0x22)
                    {
                        if next_slot >= 32 {
                            return Err(DevError::Unsupported);
                        }
                        current_slot = Some(next_slot);
                        next_slot += 1;
                    }
                    if value == 1 {
                        app = usages.first().copied().unwrap_or_default();
                        pointer |= app.page == 0x0d && matches!(app.code, 4 | 5);
                    }
                }
                (0, 12) => {
                    app = collections.pop().ok_or(DevError::InvalidParam)?;
                    current_slot = collection_slots.pop().ok_or(DevError::InvalidParam)?;
                }
                (0, 8) => {
                    if g.size == 0 || g.count == 0 || g.min > g.max {
                        return Err(DevError::InvalidParam);
                    }
                    let advance = g.size.checked_mul(g.count).ok_or(DevError::Unsupported)?;
                    let at = &mut bits[usize::from(g.id)];
                    if g.id != 0 && *at == 0 {
                        *at = 8;
                    }
                    let end = u32::from(*at)
                        .checked_add(advance)
                        .ok_or(DevError::Unsupported)?;
                    if end > 512 {
                        return Err(DevError::Unsupported);
                    }
                    if value & 1 == 0 && g.size <= 32 {
                        if value & 2 == 0 {
                            if fields.len() == MAX_FIELDS || usage_budget + usages.len() > 4096 {
                                return Err(DevError::Unsupported);
                            }
                            let mut choices = reserved(usages.len())?;
                            choices.extend_from_slice(&usages);
                            usage_budget += choices.len();
                            fields.push(Field {
                                id: g.id,
                                bit: *at,
                                size: g.size as u8,
                                count: g.count as u16,
                                min: g.min,
                                max: g.max,
                                app,
                                relative: value & 4 != 0,
                                kind: Kind::Array(choices),
                                previous: None,
                                slot: current_slot,
                            });
                        } else {
                            for index in 0..g.count {
                                let usage = usages
                                    .get(index as usize)
                                    .or(usages.last())
                                    .copied()
                                    .unwrap_or_default();
                                let mapped = if current_slot.is_some() && usage.page == 1 {
                                    match usage.code {
                                        0x30 => Some(Mapping::Axis(3, 0x35)),
                                        0x31 => Some(Mapping::Axis(3, 0x36)),
                                        _ => mapping(usage, app, value & 4 != 0),
                                    }
                                } else {
                                    mapping(usage, app, value & 4 != 0)
                                };
                                if let Some(mapped) = mapped {
                                    if fields.len() == MAX_FIELDS {
                                        return Err(DevError::Unsupported);
                                    }
                                    fields.push(Field {
                                        id: g.id,
                                        bit: *at + (index * g.size) as u16,
                                        size: g.size as u8,
                                        count: 1,
                                        min: g.min,
                                        max: g.max,
                                        app,
                                        relative: value & 4 != 0,
                                        kind: Kind::Variable(mapped),
                                        previous: None,
                                        slot: current_slot,
                                    });
                                    pointer |= (app.page == 1 && matches!(app.code, 1 | 2))
                                        || (app.page == 0x0d && matches!(app.code, 4 | 5));
                                }
                            }
                        }
                    }
                    *at = end as u16;
                }
                _ => {}
            }
            if class == 0 {
                usages.clear();
                ranges = None;
            }
        }
        if fields.is_empty()
            || !saved.is_empty()
            || !collections.is_empty()
            || !collection_slots.is_empty()
            || ranges.is_some()
            || (ids && bits[0] != 0)
        {
            return Err(DevError::Unsupported);
        }
        Ok(Self {
            fields,
            bits,
            ids,
            keys: reserved(1024)?,
            pointer,
        })
    }
    pub(crate) fn max_length(&self) -> usize {
        usize::from(*self.bits.iter().max().unwrap_or(&0)).div_ceil(8)
    }
    pub(crate) fn is_pointer(&self) -> bool {
        self.pointer
    }
    fn each_code(&self, mut visit: impl FnMut(u16, u16)) {
        let mut mapped = |m| match m {
            Mapping::Key(code) => visit(1, code),
            Mapping::Axis(ty, code) => visit(ty, code),
            Mapping::Wheel(code) => {
                visit(2, code);
                visit(2, if code == 8 { 11 } else { 12 });
            }
            Mapping::Hat => {
                visit(3, 16);
                visit(3, 17);
            }
        };
        for f in &self.fields {
            if f.slot.is_some() {
                mapped(Mapping::Axis(3, 0x2f)); // ABS_MT_SLOT
            }
            match &f.kind {
                Kind::Variable(m) => mapped(*m),
                Kind::Array(usages) => {
                    for &u in usages {
                        if let Some(m @ Mapping::Key(_)) = mapping(u, f.app, f.relative) {
                            mapped(m);
                        }
                    }
                }
            }
        }
    }
    pub(crate) fn has_code(&self, ty: u16, code: u16) -> bool {
        let mut found = false;
        self.each_code(|t, c| found |= t == ty && c == code);
        found
    }
    pub(crate) fn event_bits(&self, ty: u16, out: &mut [u8]) -> bool {
        out.fill(0);
        let mut any = false;
        let mut set = |code: u16| {
            any = true;
            if let Some(byte) = out.get_mut(usize::from(code) / 8) {
                *byte |= 1 << (code % 8);
            }
        };
        if ty == 0 {
            set(0);
        } else {
            self.each_code(|t, c| {
                if t == ty {
                    set(c);
                }
            });
        }
        any
    }
    pub(crate) fn absolute_range(&self, code: u8) -> Option<(i32, i32)> {
        if code == 0x2f && self.fields.iter().any(|field| field.slot.is_some()) {
            return Some((0, 31));
        }
        let mut range: Option<(i32, i32)> = None;
        for f in &self.fields {
            let r = match f.kind {
                Kind::Variable(Mapping::Axis(3, c)) if c == u16::from(code) => Some((f.min, f.max)),
                Kind::Variable(Mapping::Hat) if matches!(code, 16 | 17) => Some((-1, 1)),
                _ => None,
            };
            if let Some((lo, hi)) = r {
                range = Some(range.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
            }
        }
        range
    }
    pub(crate) fn decode(&mut self, report: &[u8], events: &mut VecDeque<Event>) -> bool {
        let id = if self.ids {
            let Some(&id) = report.first() else {
                return false;
            };
            id
        } else {
            0
        };
        let bits = self.bits[usize::from(id)];
        if bits == 0 || report.len() * 8 < usize::from(bits) {
            return false;
        }
        let Ok(mut next) = reserved::<u16>(512) else {
            return false;
        };
        if events.try_reserve(2048).is_err() {
            return false;
        }
        for f in self.fields.iter().filter(|f| f.id == id) {
            match &f.kind {
                Kind::Variable(Mapping::Key(code)) => {
                    let value = extract(report, usize::from(f.bit), f.size, f.min < 0);
                    if value != 0 && !next.contains(code) {
                        next.push(*code);
                    }
                }
                Kind::Array(usages) => {
                    for index in 0..f.count {
                        let value = extract(
                            report,
                            usize::from(f.bit) + usize::from(index) * usize::from(f.size),
                            f.size,
                            f.min < 0,
                        );
                        if value < f.min || value > f.max {
                            continue;
                        }
                        let Some(&usage) =
                            usages.get((i64::from(value) - i64::from(f.min)) as usize)
                        else {
                            continue;
                        };
                        if usage.page == 7 && (1..=3).contains(&usage.code) {
                            return false;
                        }
                        if let Some(Mapping::Key(code)) = mapping(usage, f.app, f.relative)
                            && !next.contains(&code)
                        {
                            next.push(code);
                        }
                    }
                }
                _ => {}
            }
        }
        if next.len() + self.keys.iter().filter(|(owner, _)| *owner != id).count() > 1024 {
            return false;
        }
        let before = events.len();
        for &(owner, code) in &self.keys {
            if owner == id
                && !next.contains(&code)
                && !self.keys.iter().any(|&(o, c)| o != id && c == code)
            {
                emit(events, 1, code, 0);
            }
        }
        for &code in &next {
            if !self.keys.iter().any(|&(_, c)| c == code) {
                emit(events, 1, code, 1);
            }
        }
        self.keys.retain(|(owner, _)| *owner != id);
        for code in next {
            self.keys.push((id, code));
        }
        for f in self.fields.iter_mut().filter(|f| f.id == id) {
            let Kind::Variable(mapped) = f.kind else {
                continue;
            };
            let raw = extract(report, usize::from(f.bit), f.size, f.min < 0);
            if let Some(slot) = f.slot {
                emit(events, 3, 0x2f, i32::from(slot));
            }
            match mapped {
                Mapping::Axis(ty, code) => {
                    let value = raw.clamp(f.min, f.max);
                    if (ty == 2 && value != 0) || (ty == 3 && f.previous != Some(value)) {
                        emit(events, ty, code, value);
                    }
                    f.previous = Some(value);
                }
                Mapping::Wheel(code) => {
                    let value = raw.clamp(f.min, f.max);
                    if value != 0 {
                        emit(events, 2, code, value);
                        emit(
                            events,
                            2,
                            if code == 8 { 11 } else { 12 },
                            value.saturating_mul(120),
                        );
                    }
                }
                Mapping::Hat => {
                    if f.previous != Some(raw) {
                        let direction = if (f.min..=f.max).contains(&raw) {
                            let span = i64::from(f.max) - i64::from(f.min) + 1;
                            (i64::from(raw) - i64::from(f.min)) * 8 / span
                        } else {
                            8
                        };
                        let (x, y) = match direction {
                            0 => (0, -1),
                            1 => (1, -1),
                            2 => (1, 0),
                            3 => (1, 1),
                            4 => (0, 1),
                            5 => (-1, 1),
                            6 => (-1, 0),
                            7 => (-1, -1),
                            _ => (0, 0),
                        };
                        emit(events, 3, 16, x);
                        emit(events, 3, 17, y);
                        f.previous = Some(raw);
                    }
                }
                Mapping::Key(_) => {}
            }
        }
        if events.len() != before {
            emit(events, 0, 0, 0);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEYBOARD: &[u8] = &[
        0x05, 1, 0x09, 6, 0xa1, 1, 0x05, 7, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0, 0x25, 1, 0x75, 1,
        0x95, 8, 0x81, 2, 0x75, 8, 0x95, 1, 0x81, 1, 0x19, 0, 0x29, 0x65, 0x15, 0, 0x25, 0x65,
        0x75, 8, 0x95, 6, 0x81, 0, 0xc0,
    ];
    fn triples(events: &VecDeque<Event>) -> Vec<(u16, u16, i32)> {
        events
            .iter()
            .map(|e| (e.event_type, e.code, e.value as i32))
            .collect()
    }
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
    fn keyboard_arrays_modifiers_duplicates_releases_and_rollover() {
        let mut parser = Report::parse(KEYBOARD).unwrap();
        let mut events = VecDeque::new();
        assert_eq!(parser.max_length(), 8);
        assert!(parser.has_code(1, 42));
        assert!(parser.has_code(1, 30));
        assert!(parser.decode(&[2, 0, 4, 4, 0, 0, 0, 0], &mut events));
        assert_eq!(triples(&events), [(1, 42, 1), (1, 30, 1), (0, 0, 0)]);
        events.clear();
        assert!(!parser.decode(&[0, 0, 1, 1, 1, 1, 1, 1], &mut events));
        assert!(events.is_empty());
        assert!(parser.decode(&[0; 8], &mut events));
        assert_eq!(triples(&events), [(1, 42, 0), (1, 30, 0), (0, 0, 0)]);
    }
    #[test]
    fn nkro_variables_and_unknown_long_items_preserve_alignment() {
        let descriptor = [
            0xfe, 3, 0x55, 0x08, 0x81, 0xc0, 0x05, 7, 0x19, 4, 0x29, 11, 0x15, 0, 0x25, 1, 0x75, 1,
            0x95, 8, 0x81, 2,
        ];
        let mut p = Report::parse(&descriptor).unwrap();
        let mut ev = VecDeque::new();
        assert!(p.decode(&[3], &mut ev));
        assert_eq!(triples(&ev), [(1, 30, 1), (1, 48, 1), (0, 0, 0)]);
        ev.clear();
        assert!(p.decode(&[0], &mut ev));
        assert_eq!(triples(&ev), [(1, 30, 0), (1, 48, 0), (0, 0, 0)]);
    }
    #[test]
    fn mouse_and_absolute_ch9329_layouts_with_independent_ids() {
        let mut d = pointer(false, Some(1));
        d.extend(pointer(true, Some(255)));
        let mut p = Report::parse(&d).unwrap();
        let mut ev = VecDeque::new();
        assert_eq!(p.absolute_range(0), Some((0, 32767)));
        assert!(p.decode(&[1, 1, 255, 4], &mut ev));
        assert_eq!(
            triples(&ev),
            [(1, 0x110, 1), (2, 0, -1), (2, 1, 4), (0, 0, 0)]
        );
        ev.clear();
        assert!(p.decode(&[255, 0, 0x34, 0x12, 0x78, 0x56], &mut ev));
        assert!(triples(&ev).contains(&(3, 0, 0x1234)));
        assert!(triples(&ev).contains(&(3, 1, 0x5678)));
        assert!(!triples(&ev).contains(&(1, 0x110, 0))); // still owned by ID1
        ev.clear();
        assert!(!p.decode(&[255, 0, 1], &mut ev));
        assert!(ev.is_empty());
        assert!(!p.decode(&[2, 0, 1, 2], &mut ev));
    }
    #[test]
    fn consumer_array_and_keyboard_key_owners_are_not_cross_released() {
        let mut d = alloc::vec![0x85, 1];
        d.extend(KEYBOARD);
        d.extend([
            0x85, 2, 0x05, 12, 0x09, 1, 0xa1, 1, 0x15, 0, 0x26, 0xff, 0x03, 0x19, 0, 0x2a, 0xff,
            0x03, 0x75, 16, 0x95, 1, 0x81, 0, 0xc0,
        ]);
        let mut p = Report::parse(&d).unwrap();
        let mut ev = VecDeque::new();
        assert!(p.has_code(1, 115));
        assert!(p.has_code(1, 164));
        assert!(p.decode(&[1, 0, 0, 4, 0, 0, 0, 0, 0], &mut ev));
        ev.clear();
        assert!(p.decode(&[2, 0xe9, 0], &mut ev));
        assert_eq!(triples(&ev), [(1, 115, 1), (0, 0, 0)]);
        ev.clear();
        assert!(p.decode(&[2, 0, 0], &mut ev));
        assert_eq!(triples(&ev), [(1, 115, 0), (0, 0, 0)]);
        ev.clear();
        assert!(p.decode(&[1, 0, 0, 0, 0, 0, 0, 0, 0], &mut ev));
        assert_eq!(triples(&ev), [(1, 30, 0), (0, 0, 0)]);
    }
    #[test]
    fn one_key_with_two_report_owners_releases_only_after_last_owner() {
        let mut d = alloc::vec![0x85, 1];
        d.extend(
            KEYBOARD
                .iter()
                .map(|byte| if *byte == 0x65 { 0xff } else { *byte }),
        );
        d.extend([
            0x85, 2, 0x05, 12, 0x15, 0, 0x26, 0xff, 0x03, 0x19, 0, 0x2a, 0xff, 0x03, 0x75, 16,
            0x95, 1, 0x81, 0,
        ]);
        let mut p = Report::parse(&d).unwrap();
        let mut ev = VecDeque::new();
        assert!(p.decode(&[1, 0, 0, 127, 0, 0, 0, 0, 0], &mut ev));
        assert_eq!(triples(&ev), [(1, 113, 1), (0, 0, 0)]);
        ev.clear();
        assert!(p.decode(&[2, 0xe2, 0], &mut ev));
        assert!(ev.is_empty());
        assert!(p.decode(&[1, 0, 0, 0, 0, 0, 0, 0, 0], &mut ev));
        assert!(ev.is_empty());
        assert!(p.decode(&[2, 0, 0], &mut ev));
        assert_eq!(triples(&ev), [(1, 113, 0), (0, 0, 0)]);
    }
    #[test]
    fn gamepad_axes_buttons_and_hat_null_state_are_evdev_compatible() {
        let d = [
            0x05, 1, 0x09, 5, 0xa1, 1, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 8,
            0x95, 2, 0x81, 2, 0x09, 0x39, 0x15, 0, 0x25, 7, 0x75, 4, 0x95, 1, 0x81, 0x42, 0x05, 9,
            0x19, 1, 0x29, 4, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 4, 0x81, 2, 0xc0,
        ];
        let mut p = Report::parse(&d).unwrap();
        let mut ev = VecDeque::new();
        assert_eq!(p.absolute_range(1), Some((-127, 127)));
        assert_eq!(p.absolute_range(16), Some((-1, 1)));
        assert!(p.has_code(1, 0x130));
        assert!(!p.has_code(1, 0x110));
        assert!(p.decode(&[0xff, 10, 0x12], &mut ev));
        assert!(triples(&ev).contains(&(1, 0x130, 1)));
        assert!(triples(&ev).contains(&(3, 0, -1)));
        assert!(triples(&ev).contains(&(3, 16, 1)));
        assert!(triples(&ev).contains(&(3, 17, 0)));
        ev.clear();
        assert!(p.decode(&[0, 0, 0x08], &mut ev));
        assert!(triples(&ev).contains(&(1, 0x130, 0)));
        assert!(triples(&ev).contains(&(3, 16, 0)));
        assert!(triples(&ev).contains(&(3, 17, 0)));
    }
    #[test]
    fn push_pop_globals_unknown_pages_and_output_bits_do_not_shift_input() {
        let d = [
            0x05, 9, 0x19, 1, 0x29, 1, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 1, 0xa4, 0x06, 0, 0xff,
            0x09, 1, 0x81, 2, 0xb4, 0x09, 1, 0x91, 2, 0x09, 1, 0x81, 2,
        ];
        let mut p = Report::parse(&d).unwrap();
        let mut ev = VecDeque::new();
        assert!(p.decode(&[2], &mut ev));
        assert_eq!(triples(&ev), [(1, 0x100, 1), (0, 0, 0)]);
    }
    #[test]
    fn wheel_emits_legacy_and_high_resolution_codes() {
        let d = [
            0x05, 1, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 8, 0x95, 1, 0x81, 6,
        ];
        let mut p = Report::parse(&d).unwrap();
        let mut ev = VecDeque::new();
        assert!(p.has_code(2, 8));
        assert!(p.has_code(2, 11));
        assert!(p.decode(&[255], &mut ev));
        assert_eq!(triples(&ev), [(2, 8, -1), (2, 11, -120), (0, 0, 0)]);
    }
    #[test]
    fn digitizer_finger_collection_emits_multitouch_slot_and_tracking_id() {
        let finger = [
            0x09, 0x22, 0xa1, 2, // Finger logical collection
            0x09, 0x51, 0x15, 0, 0x25, 31, 0x75, 8, 0x95, 1, 0x81, 2, // Contact ID
            0xc0,
        ];
        let mut descriptor = alloc::vec![0x05, 0x0d, 0x09, 0x05, 0xa1, 1];
        descriptor.extend_from_slice(&finger);
        descriptor.extend_from_slice(&finger);
        descriptor.push(0xc0);
        let mut report = Report::parse(&descriptor).unwrap();
        assert!(report.is_pointer());
        assert_eq!(report.absolute_range(0x2f), Some((0, 31)));
        let mut events = VecDeque::new();
        assert!(report.decode(&[7, 8], &mut events));
        assert_eq!(
            triples(&events),
            [
                (3, 0x2f, 0),
                (3, 0x39, 7),
                (3, 0x2f, 1),
                (3, 0x39, 8),
                (0, 0, 0)
            ]
        );
    }
    #[test]
    fn malformed_and_random_descriptors_are_bounded() {
        for d in [
            &[0xfe][..],
            &[0xfe, 9, 0][..],
            &[0xc0][..],
            &[0xb4][..],
            &[0x85, 0][..],
            &[0x26, 1][..],
            &[0x75, 32, 0x95, 255, 0x81, 2][..],
        ] {
            assert!(Report::parse(d).is_err());
        }
        for end in 0..KEYBOARD.len() {
            let _ = Report::parse(&KEYBOARD[..end]);
        }
        let mut seed = 7u32;
        for len in 0..256 {
            let mut b = Vec::new();
            for _ in 0..len {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                b.push((seed >> 24) as u8);
            }
            if let Ok(mut p) = Report::parse(&b) {
                let mut ev = VecDeque::new();
                for n in 0..64 {
                    let _ = p.decode(&b[..n.min(b.len())], &mut ev);
                    ev.clear();
                }
            }
        }
    }
}
