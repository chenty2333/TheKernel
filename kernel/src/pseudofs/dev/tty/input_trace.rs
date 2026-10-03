//! Opt-in first-byte provenance for the N305 NUL investigation. This does not
//! filter bytes: a legitimate Ctrl-Space NUL must remain a legitimate input.
use core::sync::atomic::{AtomicUsize, Ordering};
const LIMIT: usize = 64;
static NEXT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputSource {
    Serial,
    UsbKeyboard,
    VirtualKeyboard,
    OtherEvdev,
}
impl InputSource {
    pub(crate) fn from_bus(bus: u16) -> Self {
        match bus {
            3 => Self::UsbKeyboard,
            6 => Self::VirtualKeyboard,
            _ => Self::OtherEvdev,
        }
    }
}
fn enabled(line: &str) -> bool {
    line.split_ascii_whitespace()
        .filter_map(|token| token.strip_prefix("tty.input_trace="))
        .next_back()
        == Some("1")
}
fn take_prefix(next: &AtomicUsize, length: usize) -> Option<usize> {
    next.try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
        (old < LIMIT).then_some(old.saturating_add(length).min(LIMIT))
    })
    .ok()
}
fn emit_prefix(
    start: usize,
    source: InputSource,
    vt: u16,
    bytes: &[u8],
    mut emit: impl FnMut(usize, InputSource, u16, u8),
) {
    for (offset, &byte) in bytes.iter().take(LIMIT - start).enumerate() {
        emit(start + offset, source, vt, byte);
    }
}
/// Reserve in the serialized admission path; no logger/allocator is entered.
pub(crate) fn reserve(length: usize) -> Option<usize> {
    if !axhal::boot::command_line().is_some_and(enabled) {
        return None;
    }
    take_prefix(&NEXT, length)
}
/// Emit only after dropping VT/ldisc locks. Reservation preserves admission
/// order even if a later batch gets CPU time before this batch is logged.
pub(crate) fn accepted(reservation: Option<usize>, source: InputSource, vt: u16, bytes: &[u8]) {
    if let Some(start) = reservation {
        emit_prefix(start, source, vt, bytes, |sequence, source, vt, byte| {
            info!("vt-input seq={sequence} source={source:?} vt={vt} byte={byte:#04x}");
        });
    }
}
#[cfg(test)]
fn emit_first(
    next: &AtomicUsize,
    source: InputSource,
    vt: u16,
    bytes: &[u8],
    emit: impl FnMut(usize, InputSource, u16, u8),
) {
    if let Some(start) = take_prefix(next, bytes.len()) {
        emit_prefix(start, source, vt, bytes, emit);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zero_and_high_bytes_keep_their_source_and_shared_budget_never_wraps() {
        let next = AtomicUsize::new(0);
        let mut events = alloc::vec::Vec::new();
        emit_first(&next, InputSource::Serial, 1, &[0, 0xff], |i, s, v, b| {
            events.push((i, s, v, b))
        });
        emit_first(
            &next,
            InputSource::UsbKeyboard,
            2,
            &[0x41; 80],
            |i, s, v, b| events.push((i, s, v, b)),
        );
        emit_first(&next, InputSource::OtherEvdev, 3, &[0], |i, s, v, b| {
            events.push((i, s, v, b))
        });
        assert_eq!(events.len(), LIMIT);
        assert_eq!(events[0], (0, InputSource::Serial, 1, 0));
        assert_eq!(events[1], (1, InputSource::Serial, 1, 0xff));
        assert_eq!(events[63], (63, InputSource::UsbKeyboard, 2, 0x41));
        assert_eq!(next.load(Ordering::Acquire), LIMIT);
    }
    #[test]
    fn opt_in_is_exact_and_last_statement_wins() {
        assert!(!enabled("quiet"));
        assert!(enabled("quiet tty.input_trace=1"));
        assert!(!enabled("tty.input_trace=1 tty.input_trace=0"));
        assert!(!enabled("tty.input_trace=11"));
        assert_eq!(InputSource::from_bus(3), InputSource::UsbKeyboard);
        assert_eq!(InputSource::from_bus(6), InputSource::VirtualKeyboard);
        assert_eq!(InputSource::from_bus(1), InputSource::OtherEvdev);
    }
}
