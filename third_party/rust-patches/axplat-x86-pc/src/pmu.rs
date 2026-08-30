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
/// The local-APIC vector reserved for architectural PMIs.
///
/// Consumers register their contextual IRQ handler through the HAL constant
/// instead of duplicating the x86 vector allocation.
pub const SAMPLING_IRQ_VECTOR: usize = crate::apic::vectors::APIC_PMI_VECTOR as usize;
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
    InvalidProgram,
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
    sampling: bool,
    cookie: u64,
    period: u64,
    lvt_perf: u32,
}
const FREE: State = State {
    generation: 0,
    owned: false,
    abandoned: false,
    retired: false,
    saved: EMPTY,
    width: 0,
    sampling: false,
    cookie: 0,
    period: 0,
    lvt_perf: 0,
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
#[cfg(feature = "pmu-sampling")]
/// A programmable-counter overflow sampling configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplingProgram { pub event: Event, pub period: u64, pub count_user: bool, pub count_kernel: bool, pub cookie: u64 }
#[cfg(feature = "pmu-sampling")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PmiSample { pub cookie: u64, pub period: u64 }
#[cfg(feature = "pmu-sampling")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StopSample { pub residual: u64, pub overflowed: bool, pub lost: bool }
#[cfg(feature = "pmu-sampling")]
/// A linear local-CPU sampling owner; it is deliberately not `Copy`.
#[derive(Debug)]
pub struct SamplingToken { cpu: usize, slot: Slot, generation: u64, cookie: u64, active: bool }
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
            let _ = reap(m);
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
            claim_generation(s)?;
            // Generation exhaustion retires before this first register write.
            program(slot, event);
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

#[cfg(feature = "pmu-sampling")]
const LVT_MASKED: u32 = 1 << 16;
#[cfg(feature = "pmu-sampling")]
const LVT_DELIVERY_MODE: u32 = 0b111 << 8;
#[cfg(feature = "pmu-sampling")]
const fn sampling_preload(width: u8, period: u64) -> u64 {
    Capabilities::mask(width).wrapping_add(1).wrapping_sub(period)
}
#[cfg(feature = "pmu-sampling")]
const fn sampling_event(event: Event, user: bool, kernel: bool) -> u64 {
    event.code() as u64 | (user as u64) << 16 | (kernel as u64) << 17 | 1 << 20 | 1 << 22
}
#[cfg(feature = "pmu-sampling")]
fn sampling_owner(m: &Manager) -> Result<Option<usize>, Error> {
    let mut owner = None;
    for i in 0..MAX {
        if m.slots[i].owned && m.slots[i].sampling {
            if owner.replace(i).is_some() { return Err(Error::Busy); }
        }
    }
    Ok(owner)
}
#[cfg(feature = "pmu-sampling")]
const fn lvt_is_safe_sampling_baseline(lvt: u32) -> bool {
    // A masked LVT cannot deliver PMIs.  Its remaining bits are preserved and
    // restored verbatim, so a firmware/platform dormant configuration is not
    // claimed or destroyed.
    lvt & LVT_MASKED != 0
}
#[cfg(feature = "pmu-sampling")]
const fn sampling_active_lvt(saved: u32) -> u32 {
    // PMIs must enter the ordinary fixed-vector IRQ path.  A dormant LVT
    // configuration may carry a different delivery mode; retain it only in
    // the saved value and restore it on stop, never while sampling is active.
    (saved & !(0xff | LVT_MASKED | LVT_DELIVERY_MODE))
        | SAMPLING_IRQ_VECTOR as u32
}

/// Arms exactly one local programmable PMU counter for PMI delivery.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_arm_local(program: SamplingProgram) -> Result<SamplingToken, Error> {
    local(|cpu, m| {
        let _ = reap(m);
        if sampling_owner(m)?.is_some() { return Err(Error::Busy); }
        let c = capabilities()?;
        if !c.programmable(program.event) || !program.count_user && !program.count_kernel
            || program.period < 4096 || program.period > c.programmable_mask() { return Err(Error::InvalidProgram); }
        let lvt = unsafe { crate::apic::read_lvt_perf() };
        if !lvt_is_safe_sampling_baseline(lvt) { return Err(Error::Busy); }
        let (slot, saved) = select(m, c)?;
        let state = &mut m.slots[slot.idx()];
        claim_generation(state)?; // before the first register write: no ABA on exhaustion.
        unsafe { crate::apic::write_lvt_perf(lvt | LVT_MASKED); }
        disable(slot);
        write(OVF, bit(slot)); // candidate was idle/status-clear; acknowledge only our bit.
        write(EVT + match slot { Slot::P(i) => i as u32, _ => unreachable!() }, sampling_event(program.event, program.count_user, program.count_kernel));
        write(slot_msr(slot), sampling_preload(c.programmable_width, program.period));
        state.owned = true;
        state.sampling = true;
        state.abandoned = false;
        state.saved = saved;
        state.width = c.programmable_width;
        state.cookie = program.cookie;
        state.period = program.period;
        state.lvt_perf = lvt;
        unsafe { crate::apic::write_lvt_perf((lvt & !0xff) | crate::apic::vectors::APIC_PMI_VECTOR as u32 | LVT_MASKED); }
        write(GLOBAL, read(GLOBAL) | bit(slot));
        unsafe { crate::apic::write_lvt_perf(sampling_active_lvt(lvt)); }
        Ok(SamplingToken { cpu, slot, generation: state.generation, cookie: program.cookie, active: true })
    })
}

/// Consumes this CPU's owned PMI latch and leaves the sample disarmed.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_take_pmi() -> Result<Option<(PmiSample, u64)>, Error> {
    local(|_, m| {
        let Some(i) = sampling_owner(m)? else { return Ok(None); };
        let slot = Slot::P(i as u8);
        // A stray PMI is not ours.  Do not perturb the sampler (or any other
        // PMU owner's latch) before proving that this owned bit is asserted.
        if read(STATUS) & bit(slot) == 0 { return Ok(None); }
        let s = &m.slots[i];
        let sample = PmiSample { cookie: s.cookie, period: s.period };
        let generation = s.generation;
        unsafe { crate::apic::write_lvt_perf(crate::apic::read_lvt_perf() | LVT_MASKED); }
        disable(slot);
        write(OVF, bit(slot));
        Ok(Some((sample, generation)))
    })
}

/// Rearms a disarmed local sample only when its cookie and generation still match.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_rearm_local(cookie: u64, generation: u64) -> Result<(), Error> {
    local(|_, m| {
        for i in 0..MAX { let s = &mut m.slots[i]; if s.owned && s.sampling && s.cookie == cookie && s.generation == generation {
            let slot = Slot::P(i as u8); write(slot_msr(slot), sampling_preload(s.width, s.period)); write(OVF, bit(slot));
            write(GLOBAL, read(GLOBAL) | bit(slot));
            unsafe { crate::apic::write_lvt_perf(sampling_active_lvt(s.lvt_perf)); }
            return Ok(());
        }} Err(Error::Stale)
    })
}

#[cfg(feature = "pmu-sampling")]
fn stop_sampling(slot: Slot, s: &mut State) -> StopSample {
    unsafe { crate::apic::write_lvt_perf(crate::apic::read_lvt_perf() | LVT_MASKED); }
    disable(slot); let residual = read(slot_msr(slot)) & Capabilities::mask(s.width); let overflowed = read(STATUS) & bit(slot) != 0;
    if overflowed { write(OVF, bit(slot)); }
    write(EVT + match slot { Slot::P(i) => i as u32, _ => unreachable!() }, s.saved.control);
    write(slot_msr(slot), s.saved.counter); let now = read(GLOBAL); write(GLOBAL, (now & !bit(slot)) | (s.saved.global & bit(slot)));
    unsafe { crate::apic::write_lvt_perf(s.lvt_perf); }
    s.owned = false; s.sampling = false; s.abandoned = false;
    StopSample { residual, overflowed, lost: false }
}

/// Stops a local sampling token and restores exactly its saved PMU/LVT state.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_stop_local(mut token: SamplingToken) -> Result<StopSample, Error> {
    let result = local(|cpu, m| { if cpu != token.cpu { return Err(Error::Migrated); } let s = &mut m.slots[token.slot.idx()]; if !s.owned || !s.sampling || s.generation != token.generation || s.cookie != token.cookie { return Err(Error::Stale); } Ok(stop_sampling(token.slot, s)) });
    if result.is_ok() { token.active = false; } result
}

/// Bounded, idempotent local cleanup for a locally abandoned sampling token.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_quiesce_local() -> Result<usize, Error> {
    local(|_, m| { let mut count = 0; for i in 0..MAX { let s = &mut m.slots[i]; if s.owned && s.sampling && s.abandoned { let _ = stop_sampling(Slot::P(i as u8), s); count += 1; } } Ok(count) })
}

#[cfg(feature = "pmu-sampling")]
impl Drop for SamplingToken {
    fn drop(&mut self) {
        if !self.active { return; }
        let _guard = kernel_guard::NoPreemptIrqSave::new(); let cpu = crate::cpu::current_logical_cpu_id();
        let mut m = MANAGERS[self.cpu].lock(); let s = &mut m.slots[self.slot.idx()];
        if !s.owned || !s.sampling || s.generation != self.generation { return; }
        if cpu == self.cpu { let _ = stop_sampling(self.slot, s); } else { s.abandoned = true; }
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
    // Recovery only: live local leases retain their exclusive ownership.
    local(|_, m| Ok(reap(m)))
}
fn local<T>(f: impl FnOnce(usize, &mut Manager) -> Result<T, Error>) -> Result<T, Error> {
    let _short = kernel_guard::NoPreemptIrqSave::new();
    let cpu = crate::cpu::current_logical_cpu_id();
    let mut m = MANAGERS[cpu].lock();
    f(cpu, &mut m)
}
fn token<T>(t: &CounterLease, f: impl FnOnce(&mut State) -> Result<T, Error>) -> Result<T, Error> {
    local(|cpu, m| {
        let _ = reap(m);
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
        if should_reap(state) {
            let slot = if index < MAX {
                Slot::P(index as u8)
            } else {
                Slot::F((index - MAX) as u8)
            };
            #[cfg(feature = "pmu-sampling")]
            if state.sampling {
                let _ = stop_sampling(slot, state);
                count += 1;
                continue;
            }
            let _ = terminate(slot, state.saved, state.width);
            state.owned = false;
            state.abandoned = false;
            state.sampling = false;
            count += 1;
        }
    }
    count
}
const fn should_reap(state: &State) -> bool {
    state.owned && state.abandoned
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
/// An overflow latch on a slot not owned by this HAL is external state.  It is
/// never W1C'd here; the candidate remains busy and selection keeps scanning.
fn prepare_idle(s: Slot) -> Option<Saved> {
    let saved = snapshot(s);
    let control_idle = match s {
        Slot::P(_) => saved.control == 0,
        Slot::F(i) => saved.control >> (i * 4) & 15 == 0,
    };
    if !control_idle || saved.global & bit(s) != 0 || read(STATUS) & bit(s) != 0 {
        return None;
    }
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
    #[derive(Default)]
    struct RegisterModel {
        status: u64,
        programmed: usize,
        overflow_w1c: usize,
    }
    impl RegisterModel {
        fn claim_then_program(&mut self, state: &mut State) -> Result<(), Error> {
            claim_generation(state)?;
            self.programmed += 1;
            Ok(())
        }
        fn external_overflow_is_busy(&mut self, slot: Slot) -> bool {
            self.status & bit(slot) != 0
        }
    }
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
    fn drain_policy_preserves_an_effective_lease() {
        let mut state = FREE;
        state.owned = true;
        assert!(!should_reap(&state));
        state.abandoned = true;
        assert!(should_reap(&state));
    }
    #[test]
    fn external_overflow_never_causes_w1c() {
        let mut registers = RegisterModel {
            status: bit(Slot::P(2)),
            ..RegisterModel::default()
        };
        assert!(registers.external_overflow_is_busy(Slot::P(2)));
        assert_eq!(registers.overflow_w1c, 0);
    }
    #[test]
    fn exhausted_generation_never_programs() {
        let mut registers = RegisterModel::default();
        let mut state = FREE;
        state.generation = u64::MAX;
        assert_eq!(
            registers.claim_then_program(&mut state),
            Err(Error::NoCounter)
        );
        assert!(state.retired);
        assert_eq!(registers.programmed, 0);
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

    #[cfg(feature = "pmu-sampling")]
    #[test]
    fn sampling_encoding_preload_and_owned_ack_are_precise() {
        assert_eq!(SAMPLING_IRQ_VECTOR, 0xef);
        assert_eq!(sampling_preload(8, 16), 240);
        assert_eq!(sampling_event(Event::Cycles, true, false), 0x51003c);
        assert_eq!(sampling_event(Event::Instructions, false, true), 0x5200c0);
        let own = bit(Slot::P(1));
        let foreign = bit(Slot::P(2));
        assert_eq!((own | foreign) & own, own, "W1C selects only the owned latch");
        assert_eq!((0x1234u32 & !0xff) | 0xef, 0x12ef, "LVT restores non-vector bits exactly");
    }

    #[cfg(feature = "pmu-sampling")]
    #[test]
    fn sampling_lvt_baseline_owner_and_unmask_rules() {
        assert!(lvt_is_safe_sampling_baseline(LVT_MASKED | 0x41));
        assert!(!lvt_is_safe_sampling_baseline(0x41));
        let armed = sampling_active_lvt(LVT_MASKED | 0x41);
        assert_eq!(armed & LVT_MASKED, 0, "activation explicitly unmasks PMI");
        let nmi_baseline = LVT_MASKED | LVT_DELIVERY_MODE | 0x41;
        assert_eq!(sampling_active_lvt(nmi_baseline) & LVT_DELIVERY_MODE, 0, "activation uses fixed delivery");
        assert_eq!(sampling_active_lvt(nmi_baseline) & 0xff, 0xef);
        let mut manager = Manager::new();
        manager.slots[2].owned = true;
        manager.slots[2].sampling = true;
        assert_eq!(sampling_owner(&manager), Ok(Some(2)));
        manager.slots[3].owned = true;
        manager.slots[3].sampling = true;
        assert_eq!(sampling_owner(&manager), Err(Error::Busy));
    }

    #[cfg(feature = "pmu-sampling")]
    #[test]
    fn stray_pmi_predicate_keeps_sampler_armed() {
        let own = bit(Slot::P(0));
        let foreign = bit(Slot::P(1));
        assert_eq!(foreign & own, 0, "stray status must not cause mask/disable/W1C");
        assert_ne!((foreign | own) & own, 0, "only the unique owned bit authorizes disarm");
    }
}
