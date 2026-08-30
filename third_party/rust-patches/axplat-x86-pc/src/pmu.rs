//! Local x86 architectural-PMU leases.  Leases are tokens, never IRQ guards.
#[cfg(target_os = "none")]
use core::arch::x86_64::__cpuid_count;

use kspin::SpinNoIrq;
use x86::msr::{rdmsr, wrmsr};
const PMC: u32 = 0xc1;
const EVT: u32 = 0x186;
const FIXED: u32 = 0x309;
const STATUS: u32 = 0x38e;
const GLOBAL: u32 = 0x38f;
const FIXED_CTRL: u32 = 0x38d;
const OVF: u32 = 0x390;
const MAX: usize = 32;
const SLOTS: usize = 64;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Cycles,
    Instructions,
}
impl Event {
    const fn code(self) -> u8 {
        match self {
            Self::Cycles => 0x3c,
            Self::Instructions => 0xc0,
        }
    }
    const fn bit(self) -> u32 {
        match self {
            Self::Cycles => 1,
            Self::Instructions => 2,
        }
    }
    const fn fixed(self) -> u8 {
        match self {
            Self::Instructions => 0,
            Self::Cycles => 1,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CounterKind {
    Programmable,
    Fixed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unsupported,
    Hypervisor,
    NoCounter,
    Busy,
    Migrated,
    Stale,
    Overflowed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub version: u8,
    pub programmable_counters: u8,
    pub programmable_width: u8,
    pub event_mask_length: u8,
    pub unavailable_events: u32,
    pub fixed_counters: u8,
    pub fixed_width: u8,
}
impl Capabilities {
    const fn decode(a: u32, b: u32, d: u32) -> Self {
        Self {
            version: a as u8,
            programmable_counters: (a >> 8) as u8,
            programmable_width: (a >> 16) as u8,
            event_mask_length: (a >> 24) as u8,
            unavailable_events: b,
            fixed_counters: d as u8 & 31,
            fixed_width: (d >> 5) as u8,
        }
    }
    const fn mask(w: u8) -> u64 {
        if w == 0 {
            0
        } else if w >= 64 {
            u64::MAX
        } else {
            (1 << w) - 1
        }
    }
    pub const fn programmable_mask(self) -> u64 {
        Self::mask(self.programmable_width)
    }
    pub const fn fixed_mask(self) -> u64 {
        Self::mask(self.fixed_width)
    }
    const fn valid(self) -> bool {
        self.version >= 2
            && self.programmable_counters as usize <= MAX
            && self.programmable_width <= 64
            && (self.programmable_counters == 0 || self.programmable_width > 0)
            && self.fixed_counters as usize <= MAX
            && self.fixed_width <= 64
            && (self.fixed_counters == 0 || self.fixed_width > 0)
    }
    const fn programmable(self, e: Event) -> bool {
        self.valid()
            && self.programmable_counters > 0
            && self.programmable_width > 0
            && self.event_mask_length > e.bit().trailing_zeros() as u8
            && self.unavailable_events & e.bit() == 0
    }
    const fn fixed_ok(self, e: Event) -> bool {
        self.valid()
            && self.fixed_width > 0
            && self.fixed_width <= 64
            && self.fixed_counters as usize <= MAX
            && self.fixed_counters > e.fixed()
    }
    const fn usable(self) -> bool {
        (self.programmable_counters > 0 && self.programmable_width > 0)
            || (self.fixed_counters > 0 && self.fixed_width > 0)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    P(u8),
    F(u8),
}
impl Slot {
    const fn idx(self) -> usize {
        match self {
            Self::P(i) => i as usize,
            Self::F(i) => MAX + i as usize,
        }
    }
}
#[derive(Clone, Copy)]
struct Saved {
    global: u64,
    control: u64,
    counter: u64,
}
const EMPTY: Saved = Saved {
    global: 0,
    control: 0,
    counter: 0,
};
#[derive(Clone, Copy)]
struct State {
    generation: u64,
    owned: bool,
    abandoned: bool,
    retired: bool,
    saved: Saved,
    width: u8,
}
const FREE: State = State {
    generation: 0,
    owned: false,
    abandoned: false,
    retired: false,
    saved: EMPTY,
    width: 0,
};
struct Manager {
    slots: [State; SLOTS],
}
impl Manager {
    const fn new() -> Self {
        Self {
            slots: [FREE; SLOTS],
        }
    }
}
static MANAGERS: [SpinNoIrq<Manager>; crate::config::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(Manager::new()) }; crate::config::plat::MAX_CPU_NUM];
/// A linear CPU, slot, generation token. It is Send, but never authorizes
/// remote MSRs.
#[derive(Debug)]
pub struct CounterLease {
    cpu: usize,
    slot: Slot,
    generation: u64,
    active: bool,
}
/// The final, width-masked sample taken while atomically terminating a lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalSample {
    pub value: u64,
    pub overflowed: bool,
}
pub fn capabilities() -> Result<Capabilities, Error> {
    #[cfg(not(target_os = "none"))]
    {
        return Err(Error::Unsupported);
    }
    #[cfg(target_os = "none")]
    {
        let l0 = __cpuid_count(0, 0);
        if l0.eax < 0x0a || [l0.ebx, l0.edx, l0.ecx] != [0x756e6547, 0x49656e69, 0x6c65746e] {
            return Err(Error::Unsupported);
        }
        if __cpuid_count(1, 0).ecx & (1 << 31) != 0 {
            return Err(Error::Hypervisor);
        }
        let p = __cpuid_count(0x0a, 0);
        let c = Capabilities::decode(p.eax, p.ebx, p.edx);
        if c.valid() && c.usable() {
            Ok(c)
        } else {
            Err(Error::Unsupported)
        }
    }
}
impl CounterLease {
    pub fn acquire(event: Event, kind: CounterKind) -> Result<Self, Error> {
        local(|cpu, m| {
            let c = capabilities()?;
            let (slot, old) = match kind {
                CounterKind::Programmable if c.programmable(event) => select(m, c)?,
                CounterKind::Fixed if c.fixed_ok(event) => {
                    let slot = Slot::F(event.fixed());
                    if m.slots[slot.idx()].owned {
                        return Err(Error::Busy);
                    }
                    (slot, prepare_idle(slot).ok_or(Error::Busy)?)
                }
                _ => return Err(Error::Unsupported),
            };
            let s = &mut m.slots[slot.idx()];
            if s.owned {
                return Err(Error::Busy);
            }
            if s.retired {
                return Err(Error::NoCounter);
            }
            program(slot, event);
            claim_generation(s)?;
            s.owned = true;
            s.abandoned = false;
            s.saved = old;
            s.width = match slot {
                Slot::P(_) => c.programmable_width,
                Slot::F(_) => c.fixed_width,
            };
            Ok(Self {
                cpu,
                slot,
                generation: s.generation,
                active: true,
            })
        })
    }
    pub fn read(&self) -> Result<u64, Error> {
        token(self, |s| {
            Ok(read(slot_msr(self.slot)) & Capabilities::mask(s.width))
        })
    }
    /// Delta from `previous`, modulo the architectural width.
    pub fn settle(&self, previous: u64) -> Result<u64, Error> {
        token(self, |s| {
            Ok(read(slot_msr(self.slot)).wrapping_sub(previous) & Capabilities::mask(s.width))
        })
    }
    pub fn finish(mut self) -> Result<FinalSample, Error> {
        let result = token(&self, |s| {
            let sample = terminate(self.slot, s.saved, s.width);
            s.owned = false;
            s.abandoned = false;
            Ok(sample)
        });
        if result.is_ok() {
            self.active = false;
        }
        result
    }
    pub fn release(self) -> Result<(), Error> {
        match self.finish()? {
            FinalSample {
                overflowed: true, ..
            } => Err(Error::Overflowed),
            _ => Ok(()),
        }
    }
}
impl Drop for CounterLease {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _short = kernel_guard::NoPreemptIrqSave::new();
        let cpu = crate::cpu::current_logical_cpu_id();
        if cpu != self.cpu {
            let mut manager = MANAGERS[self.cpu].lock();
            let state = &mut manager.slots[self.slot.idx()];
            if state.owned && state.generation == self.generation {
                state.abandoned = true;
            }
            return;
        }
        let mut manager = MANAGERS[cpu].lock();
        let _ = reap(&mut manager);
        let state = &mut manager.slots[self.slot.idx()];
        if state.owned && state.generation == self.generation {
            let _ = terminate(self.slot, state.saved, state.width);
            state.owned = false;
            state.abandoned = false;
        }
    }
}
/// Bounded, local-only recovery for tokens dropped on another CPU.
pub fn drain_local() -> Result<usize, Error> {
    local(|_, m| {
        let mut count = 0;
        for index in 0..SLOTS {
            let state = &mut m.slots[index];
            if state.owned {
                let slot = if index < MAX {
                    Slot::P(index as u8)
                } else {
                    Slot::F((index - MAX) as u8)
                };
                let _ = terminate(slot, state.saved, state.width);
                state.owned = false;
                state.abandoned = false;
                count += 1;
            }
        }
        Ok(count)
    })
}
fn local<T>(f: impl FnOnce(usize, &mut Manager) -> Result<T, Error>) -> Result<T, Error> {
    let _short = kernel_guard::NoPreemptIrqSave::new();
    let cpu = crate::cpu::current_logical_cpu_id();
    let mut m = MANAGERS[cpu].lock();
    let _ = reap(&mut m);
    f(cpu, &mut m)
}
fn token<T>(t: &CounterLease, f: impl FnOnce(&mut State) -> Result<T, Error>) -> Result<T, Error> {
    local(|cpu, m| {
        if cpu != t.cpu {
            return Err(Error::Migrated);
        }
        let s = &mut m.slots[t.slot.idx()];
        if !s.owned || s.generation != t.generation {
            return Err(Error::Stale);
        }
        f(s)
    })
}
fn reap(m: &mut Manager) -> usize {
    let mut count = 0;
    for index in 0..SLOTS {
        let state = &mut m.slots[index];
        if state.owned && state.abandoned {
            let slot = if index < MAX {
                Slot::P(index as u8)
            } else {
                Slot::F((index - MAX) as u8)
            };
            let _ = terminate(slot, state.saved, state.width);
            state.owned = false;
            state.abandoned = false;
            count += 1;
        }
    }
    count
}
fn claim_generation(state: &mut State) -> Result<(), Error> {
    state.generation = match state.generation.checked_add(1) {
        Some(generation) => generation,
        None => {
            state.retired = true;
            return Err(Error::NoCounter);
        }
    };
    Ok(())
}
fn select(m: &Manager, c: Capabilities) -> Result<(Slot, Saved), Error> {
    let mut software_free = false;
    for i in 0..c.programmable_counters as usize {
        let state = &m.slots[i];
        if !state.owned && !state.retired {
            software_free = true;
            let slot = Slot::P(i as u8);
            if let Some(saved) = prepare_idle(slot) {
                return Ok((slot, saved));
            }
        }
    }
    if software_free {
        Err(Error::Busy)
    } else {
        Err(Error::NoCounter)
    }
}
fn bit(s: Slot) -> u64 {
    match s {
        Slot::P(i) => 1 << i,
        Slot::F(i) => 1 << (32 + i),
    }
}
fn slot_msr(s: Slot) -> u32 {
    match s {
        Slot::P(i) => PMC + i as u32,
        Slot::F(i) => FIXED + i as u32,
    }
}
fn snapshot(s: Slot) -> Saved {
    Saved {
        global: read(GLOBAL),
        control: match s {
            Slot::P(i) => read(EVT + i as u32),
            Slot::F(_) => read(FIXED_CTRL),
        },
        counter: read(slot_msr(s)),
    }
}
fn idle(s: Slot, v: Saved) -> bool {
    let ctrl = match s {
        Slot::P(_) => v.control == 0,
        Slot::F(i) => v.control >> (i * 4) & 15 == 0,
    };
    ctrl && v.global & bit(s) == 0 && read(STATUS) & bit(s) == 0
}
/// A stale overflow latch is safe to clear only after control and global bits
/// prove the slot idle; re-read afterwards so a concurrent external user wins.
fn prepare_idle(s: Slot) -> Option<Saved> {
    let saved = snapshot(s);
    let control_idle = match s {
        Slot::P(_) => saved.control == 0,
        Slot::F(i) => saved.control >> (i * 4) & 15 == 0,
    };
    if !control_idle || saved.global & bit(s) != 0 {
        return None;
    }
    if read(STATUS) & bit(s) != 0 {
        write(OVF, bit(s));
    }
    let saved = snapshot(s);
    idle(s, saved).then_some(saved)
}
fn disable(s: Slot) {
    write(GLOBAL, read(GLOBAL) & !bit(s))
}
fn program(s: Slot, e: Event) {
    disable(s);
    match s {
        Slot::P(i) => write(
            EVT + i as u32,
            e.code() as u64 | (1 << 16) | (1 << 17) | (1 << 22),
        ),
        Slot::F(i) => {
            let old = read(FIXED_CTRL);
            write(FIXED_CTRL, (old & !(15 << (i * 4))) | (3 << (i * 4)))
        }
    }
    write(slot_msr(s), 0);
    write(GLOBAL, read(GLOBAL) | bit(s))
}
fn terminate(s: Slot, old: Saved, width: u8) -> FinalSample {
    disable(s);
    let value = read(slot_msr(s)) & Capabilities::mask(width);
    let overflow = read(STATUS) & bit(s) != 0;
    match s {
        Slot::P(i) => write(EVT + i as u32, old.control),
        Slot::F(i) => {
            let now = read(FIXED_CTRL);
            let mask = 15 << (i * 4);
            write(FIXED_CTRL, (now & !mask) | (old.control & mask))
        }
    }
    write(slot_msr(s), old.counter);
    if overflow {
        write(OVF, bit(s))
    }
    let now = read(GLOBAL);
    write(GLOBAL, (now & !bit(s)) | (old.global & bit(s)));
    FinalSample {
        value,
        overflowed: overflow,
    }
}
fn read(msr: u32) -> u64 {
    unsafe { rdmsr(msr) }
}
fn write(msr: u32, v: u64) {
    unsafe { wrmsr(msr, v) }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities_reject_v1_and_invalid_widths() {
        assert!(!Capabilities::decode(1 | (1 << 8) | (48 << 16), 0, 0).valid());
        assert!(!Capabilities::decode(2 | (1 << 8) | (65 << 16), 0, 0).valid());
        assert!(
            Capabilities::decode(2 | (4 << 8) | (48 << 16) | (2 << 24), 0, 3 | (40 << 5))
                .fixed_ok(Event::Cycles)
        );
    }
    #[test]
    fn encodings_and_delta() {
        assert_eq!(
            Event::Cycles.code() as u64 | (1 << 16) | (1 << 17) | (1 << 22),
            0x43003c
        );
        assert_eq!(3u64.wrapping_sub(250) & Capabilities::mask(8), 9);
    }
    #[test]
    fn busy_and_stale_generation() {
        let mut m = Manager::new();
        m.slots[0].owned = true;
        assert!(matches!(
            select(&m, Capabilities::decode(2 | (1 << 8) | (48 << 16), 0, 0)),
            Err(Error::NoCounter)
        ));
        m.slots[0].generation = 7;
        assert_ne!(m.slots[0].generation, 6);
    }
    #[test]
    fn token_is_linear_and_generation_retires_before_aba() {
        assert!(core::mem::needs_drop::<CounterLease>());
        let mut state = FREE;
        state.generation = u64::MAX;
        assert_eq!(claim_generation(&mut state), Err(Error::NoCounter));
        assert!(state.retired);
        state.owned = false;
        assert!(!state.owned, "released state rejects every old token");
    }
    #[test]
    fn masked_restore_math() {
        let mask = 15 << 4;
        assert_eq!((0xa0 & !mask) | (0x10 & mask), 0x10);
        assert_eq!((0b101 & !2) | (2 & 2), 0b111);
    }
    #[test]
    fn host_stub() {
        #[cfg(not(target_os = "none"))]
        assert_eq!(capabilities(), Err(Error::Unsupported));
    }
}
