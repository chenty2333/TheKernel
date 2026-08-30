//! Local-only x86 architectural PMU counter leases.
//!
//! The first-stage interface intentionally has no sampling, PMI, LVT, or
//! cross-CPU operation.  A lease owns one counter while preemption is disabled
//! and restores every PMU register it changes before releasing that pin.

use core::{arch::x86_64::__cpuid_count, marker::PhantomData};

use kernel_guard::NoPreemptIrqSave;
use x86::msr::{rdmsr, wrmsr};

const IA32_PMC0: u32 = 0x0c1;
const IA32_PERFEVTSEL0: u32 = 0x186;
const IA32_FIXED_CTR0: u32 = 0x309;
const IA32_PERF_GLOBAL_STATUS: u32 = 0x38e;
const IA32_PERF_GLOBAL_CTRL: u32 = 0x38f;
const IA32_FIXED_CTR_CTRL: u32 = 0x38d;
const IA32_PERF_GLOBAL_OVF_CTRL: u32 = 0x390;
const MAX_COUNTERS: u8 = 32;

/// The two architectural events exposed by this counting-only HAL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Cycles,
    Instructions,
}

impl Event {
    const fn encoding(self) -> u8 {
        match self {
            Self::Cycles => 0x3c,
            Self::Instructions => 0xc0,
        }
    }
    const fn unavailable_bit(self) -> u32 {
        match self {
            Self::Cycles => 1,
            Self::Instructions => 1 << 1,
        }
    }
    const fn fixed_index(self) -> u8 {
        match self {
            Self::Instructions => 0,
            Self::Cycles => 1,
        }
    }
}

/// Counter bank from which to request an event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CounterKind {
    Programmable,
    Fixed,
}

/// CPUID.0Ah information for the CPU executing this call.
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
    const fn decode(eax: u32, ebx: u32, edx: u32) -> Self {
        Self {
            version: eax as u8,
            programmable_counters: (eax >> 8) as u8,
            programmable_width: (eax >> 16) as u8,
            event_mask_length: (eax >> 24) as u8,
            unavailable_events: ebx,
            fixed_counters: edx as u8 & 0x1f,
            fixed_width: (edx >> 5) as u8 & 0xff,
        }
    }
    const fn mask(width: u8) -> u64 {
        if width >= 64 {
            u64::MAX
        } else if width == 0 {
            0
        } else {
            (1u64 << width) - 1
        }
    }
    pub const fn programmable_mask(self) -> u64 {
        Self::mask(self.programmable_width)
    }
    pub const fn fixed_mask(self) -> u64 {
        Self::mask(self.fixed_width)
    }
    const fn supports_programmable(self, event: Event) -> bool {
        self.version != 0
            && self.programmable_counters != 0
            && self.programmable_width != 0
            && self.event_mask_length > event.unavailable_bit().trailing_zeros() as u8
            && (self.unavailable_events & event.unavailable_bit()) == 0
    }
    const fn supports_fixed(self, event: Event) -> bool {
        self.fixed_width != 0 && self.fixed_counters > event.fixed_index()
    }
}

/// Why a local PMU lease could not be obtained or used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unsupported,
    Hypervisor,
    NoCounter,
    Busy,
    Migrated,
    Overflowed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    Programmable(u8),
    Fixed(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Snapshot {
    global_ctrl: u64,
    status: u64,
    control: u64,
    counter: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Restore {
    Active(Snapshot),
    Disabled(Snapshot),
    Restored,
}

impl Restore {
    fn disable(&mut self) {
        if let Self::Active(s) = *self {
            *self = Self::Disabled(s);
        }
    }
    fn finish(&mut self) {
        *self = Self::Restored;
    }
}

/// A local CPU PMU counter lease.  It is neither `Send` nor `Sync`.
pub struct CounterLease {
    // IRQs are saved as well as preemption-disabled: this closes the only
    // same-CPU acquisition race while a counter's idle state is inspected.
    _pin: NoPreemptIrqSave,
    cpu: usize,
    slot: Slot,
    snapshot: Restore,
    width: u8,
    _not_send_sync: PhantomData<*mut ()>,
}

/// Return the PMU capabilities of the CPU executing this call.
pub fn capabilities() -> Result<Capabilities, Error> {
    // SAFETY: CPUID is available on every x86_64 CPU.
    let leaf0 = __cpuid_count(0, 0);
    if leaf0.eax < 0x0a {
        return Err(Error::Unsupported);
    }
    let leaf1 = __cpuid_count(1, 0);
    if leaf1.ecx & (1 << 31) != 0 {
        return Err(Error::Hypervisor);
    }
    let pmu = __cpuid_count(0x0a, 0);
    let caps = Capabilities::decode(pmu.eax, pmu.ebx, pmu.edx);
    if caps.version == 0 {
        Err(Error::Unsupported)
    } else {
        Ok(caps)
    }
}

impl CounterLease {
    /// Acquire an idle counter on this CPU.  Acquisition pins the caller, so
    /// later reads and release cannot accidentally access an MSR after migration.
    pub fn acquire(event: Event, kind: CounterKind) -> Result<Self, Error> {
        let pin = NoPreemptIrqSave::new();
        let cpu = crate::cpu::current_logical_cpu_id();
        let caps = capabilities()?;
        let slot = match kind {
            CounterKind::Programmable if caps.supports_programmable(event) => {
                find_programmable(caps)?
            }
            CounterKind::Fixed if caps.supports_fixed(event) => Slot::Fixed(event.fixed_index()),
            _ => return Err(Error::Unsupported),
        };
        let snapshot = read_snapshot(slot);
        if !is_idle(slot, snapshot) {
            return Err(Error::Busy);
        }
        program(slot, event, snapshot.global_ctrl);
        Ok(Self {
            _pin: pin,
            cpu,
            slot,
            snapshot: Restore::Active(snapshot),
            width: match slot {
                Slot::Programmable(_) => caps.programmable_width,
                Slot::Fixed(_) => caps.fixed_width,
            },
            _not_send_sync: PhantomData,
        })
    }

    /// Read the counter modulo its CPUID-reported architectural width.
    pub fn read(&self) -> Result<u64, Error> {
        self.ensure_local()?;
        Ok(read_counter(self.slot) & Capabilities::mask(self.width))
    }

    /// Restore the exact saved configuration and release this local lease.
    pub fn release(mut self) -> Result<(), Error> {
        self.restore()
    }

    fn ensure_local(&self) -> Result<(), Error> {
        if crate::cpu::current_logical_cpu_id() == self.cpu {
            Ok(())
        } else {
            Err(Error::Migrated)
        }
    }
    fn restore(&mut self) -> Result<(), Error> {
        self.ensure_local()?;
        let snapshot = match self.snapshot {
            Restore::Active(s) | Restore::Disabled(s) => s,
            Restore::Restored => return Ok(()),
        };
        // Stop first.  This also ensures no new overflow is generated while restoring.
        disable_global(self.slot, snapshot.global_ctrl);
        self.snapshot.disable();
        let overflow = read_msr(IA32_PERF_GLOBAL_STATUS) & slot_bit(self.slot);
        restore_registers(self.slot, snapshot, overflow != 0);
        self.snapshot.finish();
        if overflow != 0 {
            Err(Error::Overflowed)
        } else {
            Ok(())
        }
    }
}

impl Drop for CounterLease {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn find_programmable(caps: Capabilities) -> Result<Slot, Error> {
    let count = caps.programmable_counters.min(MAX_COUNTERS) as usize;
    let mut snapshots = [Snapshot {
        global_ctrl: 0,
        status: 0,
        control: 1,
        counter: 0,
    }; MAX_COUNTERS as usize];
    for (index, snapshot) in snapshots[..count].iter_mut().enumerate() {
        *snapshot = read_snapshot(Slot::Programmable(index as u8));
    }
    select_idle_programmable(caps, &snapshots[..count]).ok_or(Error::NoCounter)
}

fn select_idle_programmable(caps: Capabilities, snapshots: &[Snapshot]) -> Option<Slot> {
    snapshots
        .iter()
        .take(caps.programmable_counters.min(MAX_COUNTERS) as usize)
        .enumerate()
        .find_map(|(index, snapshot)| {
            let slot = Slot::Programmable(index as u8);
            is_idle(slot, *snapshot).then_some(slot)
        })
}
fn slot_bit(slot: Slot) -> u64 {
    match slot {
        Slot::Programmable(i) => 1u64 << i,
        Slot::Fixed(i) => 1u64 << (32 + i),
    }
}
fn counter_msr(slot: Slot) -> u32 {
    match slot {
        Slot::Programmable(i) => IA32_PMC0 + i as u32,
        Slot::Fixed(i) => IA32_FIXED_CTR0 + i as u32,
    }
}
fn read_counter(slot: Slot) -> u64 {
    read_msr(counter_msr(slot))
}
fn read_snapshot(slot: Slot) -> Snapshot {
    Snapshot {
        global_ctrl: read_msr(IA32_PERF_GLOBAL_CTRL),
        status: read_msr(IA32_PERF_GLOBAL_STATUS),
        control: match slot {
            Slot::Programmable(i) => read_msr(IA32_PERFEVTSEL0 + i as u32),
            Slot::Fixed(_) => read_msr(IA32_FIXED_CTR_CTRL),
        },
        counter: read_counter(slot),
    }
}
fn is_idle(slot: Slot, s: Snapshot) -> bool {
    let control_idle = match slot {
        Slot::Programmable(_) => s.control == 0,
        Slot::Fixed(i) => ((s.control >> (i * 4)) & 0xf) == 0,
    };
    control_idle && (s.global_ctrl & slot_bit(slot)) == 0 && (s.status & slot_bit(slot)) == 0
}
fn program(slot: Slot, event: Event, global_ctrl: u64) {
    disable_global(slot, global_ctrl);
    match slot {
        Slot::Programmable(i) => write_msr(
            IA32_PERFEVTSEL0 + i as u32,
            event.encoding() as u64 | (1 << 17) | (1 << 22),
        ),
        Slot::Fixed(i) => {
            let old = read_msr(IA32_FIXED_CTR_CTRL);
            write_msr(
                IA32_FIXED_CTR_CTRL,
                (old & !(0xf << (i * 4))) | (1 << (i * 4)),
            );
        }
    }
    write_msr(counter_msr(slot), 0);
    write_msr(IA32_PERF_GLOBAL_CTRL, global_ctrl | slot_bit(slot));
}
fn disable_global(slot: Slot, global_ctrl: u64) {
    write_msr(IA32_PERF_GLOBAL_CTRL, global_ctrl & !slot_bit(slot));
}
fn restore_registers(slot: Slot, s: Snapshot, overflowed: bool) {
    match slot {
        Slot::Programmable(i) => write_msr(IA32_PERFEVTSEL0 + i as u32, s.control),
        Slot::Fixed(_) => write_msr(IA32_FIXED_CTR_CTRL, s.control),
    }
    write_msr(counter_msr(slot), s.counter);
    // OVF_CTRL is write-one-to-clear; acquisition requires the saved bit clear,
    // so clearing only a newly observed bit restores the saved status boundary.
    if overflowed {
        write_msr(IA32_PERF_GLOBAL_OVF_CTRL, slot_bit(slot));
    }
    write_msr(IA32_PERF_GLOBAL_CTRL, s.global_ctrl);
}
fn read_msr(msr: u32) -> u64 {
    unsafe { rdmsr(msr) }
}
fn write_msr(msr: u32, value: u64) {
    unsafe { wrmsr(msr, value) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decode_capabilities_and_width_masks() {
        let c = Capabilities::decode(4 | (4 << 8) | (48 << 16) | (7 << 24), 2, 3 | (40 << 5));
        assert_eq!(c.programmable_mask(), (1u64 << 48) - 1);
        assert_eq!(c.fixed_mask(), (1u64 << 40) - 1);
        assert!(c.supports_programmable(Event::Cycles));
        assert!(!c.supports_programmable(Event::Instructions));
        assert!(c.supports_fixed(Event::Cycles));
        assert!(c.supports_fixed(Event::Instructions));
    }
    #[test]
    fn allocation_skips_busy_slots() {
        let free = Snapshot {
            global_ctrl: 0,
            status: 0,
            control: 0,
            counter: 9,
        };
        let busy = Snapshot { control: 1, ..free };
        assert!(is_idle(Slot::Programmable(0), free));
        assert!(!is_idle(Slot::Programmable(0), busy));
        assert!(!is_idle(
            Slot::Fixed(1),
            Snapshot {
                control: 0x10,
                ..free
            }
        ));
        let caps = Capabilities::decode(1 | (2 << 8) | (40 << 16) | (2 << 24), 0, 0);
        assert_eq!(
            select_idle_programmable(caps, &[busy, free]),
            Some(Slot::Programmable(1))
        );
    }
    #[test]
    fn restore_state_machine_is_idempotent() {
        let s = Snapshot {
            global_ctrl: 1,
            status: 0,
            control: 0,
            counter: 0,
        };
        let mut state = Restore::Active(s);
        state.disable();
        assert_eq!(state, Restore::Disabled(s));
        state.finish();
        assert_eq!(state, Restore::Restored);
    }
}
