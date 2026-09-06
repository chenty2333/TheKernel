//! Intel architectural-PMU discovery and local counter leases.
//!
//! PMU use is a fleet property: each online CPU snapshots the complete state
//! this module can own, then the fleet becomes usable only after every CPU has
//! prepared successfully.  The snapshot is also the kexec restoration point.
#[cfg(target_os = "none")]
use core::arch::x86_64::__cpuid_count;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use kspin::SpinNoIrq;
use x86::msr::{rdmsr, wrmsr};

/// Public PMU-facing core classification.  The topology module remains the
/// source of truth; this re-export keeps fleet snapshots usable through the
/// HAL without exposing its private module path.
pub use crate::cpu::IntelCoreType;
const PMC: u32 = 0xc1;
const EVT: u32 = 0x186;
const FIXED: u32 = 0x309;
const STATUS: u32 = 0x38e;
const GLOBAL: u32 = 0x38f;
const FIXED_CTRL: u32 = 0x38d;
const OVF: u32 = 0x390;
const DEBUGCTL: u32 = 0x1d9;
const DS_AREA: u32 = 0x600;
const PEBS_ENABLE: u32 = 0x3f1;
const MAX: usize = 32;
const SLOTS: usize = 64;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Cycles,
    Instructions,
    Architectural {
        event_select: u64,
        availability_bit: u8,
    },
    /// A raw IA32_PERFEVTSEL encoding, accepted only on the explicitly
    /// whitelisted Panther Lake product and always placed on a programmable
    /// counter.  It is never translated across hybrid core types.
    Raw {
        event_select: u64,
        core_type: IntelCoreType,
    },
}
impl Event {
    const fn code(self) -> u8 {
        match self {
            Self::Cycles => 0x3c,
            Self::Instructions => 0xc0,
            Self::Architectural { event_select, .. } => event_select as u8,
            Self::Raw { event_select, .. } => event_select as u8,
        }
    }
    const fn bit(self) -> u32 {
        match self {
            Self::Cycles => 1,
            Self::Instructions => 2,
            Self::Architectural {
                availability_bit, ..
            } => 1 << availability_bit,
            Self::Raw { .. } => 0,
        }
    }
    const fn fixed(self) -> Option<u8> {
        match self {
            Self::Instructions => Some(0),
            Self::Cycles => Some(1),
            Self::Architectural { .. } => None,
            Self::Raw { .. } => None,
        }
    }
    const fn raw_config(self) -> Option<u64> {
        match self {
            Self::Raw { event_select, .. } | Self::Architectural { event_select, .. } => {
                Some(event_select)
            }
            _ => None,
        }
    }
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
/// The PMU product class selected from locally observed CPUID data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductClass {
    /// Family 6 model 0xCC: the supported Panther Lake hybrid product.
    PantherLake,
    /// Another bare-metal Intel processor with PMU version 4 or newer.
    ArchitecturalOnly,
}

/// Per-CPU PMU capability snapshot.  `core_type` is intentionally local: a
/// hybrid machine must not infer it from the BSP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilitySnapshot {
    pub capabilities: Capabilities,
    pub family: u8,
    pub model: u8,
    pub core_type: crate::cpu::IntelCoreType,
    pub product: ProductClass,
}

/// Product-specific placement limits discovered per CPU.  A zero value is a
/// real "not advertised" answer, never a guessed Panther Lake capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlacementCapabilities {
    pub programmable_mask: u64,
    pub pebs_counter_mask: u64,
    pub lbr: bool,
    pub offcore_slots: u8,
    pub topdown_slots: u8,
    pub smt_shared_slots: u8,
}

pub fn placement_capabilities() -> Result<PlacementCapabilities, Error> {
    let snapshot = capability_snapshot()?;
    let programmable_mask = if snapshot.capabilities.programmable_counters == 64 {
        u64::MAX
    } else {
        (1u64 << snapshot.capabilities.programmable_counters) - 1
    };
    if snapshot.product != ProductClass::PantherLake {
        return Ok(PlacementCapabilities {
            programmable_mask,
            pebs_counter_mask: 0,
            lbr: false,
            offcore_slots: 0,
            topdown_slots: 0,
            smt_shared_slots: 0,
        });
    }
    #[cfg(target_os = "none")]
    {
        let perf_caps = unsafe { rdmsr(0x345) };
        let pebs_format = (perf_caps >> 8) & 0xff;
        let lbr = __cpuid_count(0, 0).eax >= 0x1c && __cpuid_count(0x1c, 0).eax != 0;
        // PerfMon Discovery has not supplied box descriptors in this module
        // yet, so leave the non-core resources at zero.  This preserves the
        // advertised-versus-usable distinction at the placement boundary.
        Ok(PlacementCapabilities {
            programmable_mask,
            pebs_counter_mask: if pebs_format == 4 {
                programmable_mask
            } else {
                0
            },
            lbr,
            offcore_slots: 0,
            topdown_slots: 0,
            smt_shared_slots: 0,
        })
    }
    #[cfg(not(target_os = "none"))]
    Ok(PlacementCapabilities {
        programmable_mask,
        pebs_counter_mask: 0,
        lbr: false,
        offcore_slots: 0,
        topdown_slots: 0,
        smt_shared_slots: 0,
    })
}

/// Generic aliases are legal on a hybrid fleet only when CPUID reports the
/// architectural event available on *every* committed core type.  Callers
/// must otherwise use the cpu_core/cpu_atom typed PMU and a type-local raw
/// encoding; this function deliberately has no model table.
pub fn architectural_event_supported_fleet(availability_bit: u8) -> Result<(), Error> {
    if availability_bit >= 32 || !is_active() {
        return Err(Error::Unsupported);
    }
    let count = crate::cpu::cpu_num();
    for state in FLEET_STATES.get(..count).ok_or(Error::Unsupported)? {
        let state = state.lock();
        if !state.prepared
            || state.snapshot.capabilities.unavailable_events & (1 << availability_bit) != 0
        {
            return Err(Error::Unsupported);
        }
    }
    Ok(())
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
        self.version >= 4
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
            && (matches!(e, Event::Raw { .. })
                || (self.event_mask_length > e.bit().trailing_zeros() as u8
                    && self.unavailable_events & e.bit() == 0))
    }
    const fn fixed_ok(self, e: Event) -> bool {
        if !self.valid()
            || self.fixed_width == 0
            || self.fixed_width > 64
            || self.fixed_counters as usize > MAX
        {
            return false;
        }
        match e.fixed() {
            Some(fixed) => self.fixed_counters > fixed,
            None => false,
        }
    }
    const fn usable(self) -> bool {
        (self.programmable_counters > 0 && self.programmable_width > 0)
            || (self.fixed_counters > 0 && self.fixed_width > 0)
    }
}

const EMPTY_CAPABILITIES: Capabilities = Capabilities {
    version: 0,
    programmable_counters: 0,
    programmable_width: 0,
    event_mask_length: 0,
    unavailable_events: 0,
    fixed_counters: 0,
    fixed_width: 0,
};

const EMPTY_SNAPSHOT: CapabilitySnapshot = CapabilitySnapshot {
    capabilities: EMPTY_CAPABILITIES,
    family: 0,
    model: 0,
    core_type: crate::cpu::IntelCoreType::Unknown(0),
    product: ProductClass::ArchitecturalOnly,
};

/// PMU registers which this module may change.  Arrays are bounded by the
/// architectural CPUID counter maximum, so snapshots never touch unknown MSRs.
#[derive(Clone, Copy)]
struct Baseline {
    global_ctrl: u64,
    fixed_ctrl: u64,
    overflow_status: u64,
    debugctl: u64,
    ds_area: u64,
    debug_store: bool,
    event_select: [u64; MAX],
    programmable: [u64; MAX],
    fixed: [u64; MAX],
    #[cfg(feature = "pmu-sampling")]
    lvt_perf: u32,
}

const EMPTY_BASELINE: Baseline = Baseline {
    global_ctrl: 0,
    fixed_ctrl: 0,
    overflow_status: 0,
    debugctl: 0,
    ds_area: 0,
    debug_store: false,
    event_select: [0; MAX],
    programmable: [0; MAX],
    fixed: [0; MAX],
    #[cfg(feature = "pmu-sampling")]
    lvt_perf: 0,
};

#[derive(Clone, Copy)]
struct FleetState {
    prepared: bool,
    snapshot: CapabilitySnapshot,
    baseline: Baseline,
}
const EMPTY_FLEET_STATE: FleetState = FleetState {
    prepared: false,
    snapshot: EMPTY_SNAPSHOT,
    baseline: EMPTY_BASELINE,
};
static FLEET_STATES: [SpinNoIrq<FleetState>; crate::config::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(EMPTY_FLEET_STATE) }; crate::config::plat::MAX_CPU_NUM];
const FLEET_COLLECTING: u8 = 0;
const FLEET_ACTIVE: u8 = 1;
const FLEET_ABORTED: u8 = 2;
static FLEET_PHASE: AtomicU8 = AtomicU8::new(FLEET_COLLECTING);
static PREPARED_CPUS: AtomicUsize = AtomicUsize::new(0);
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
    // LVT Performance is a CPU-wide delivery resource. The first sampler
    // saves its dormant baseline; the final sampler restores it.
    sampling_lvt: Option<u32>,
}
impl Manager {
    const fn new() -> Self {
        Self {
            slots: [FREE; SLOTS],
            sampling_lvt: None,
        }
    }
}
static MANAGERS: [SpinNoIrq<Manager>; crate::config::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(Manager::new()) }; crate::config::plat::MAX_CPU_NUM];
/// A group may consume every architectural counter exposed by this platform.
/// This is an API bound, not an artificial two-event policy: the solver still
/// rejects requests beyond the local CPUID-discovered capacity.
pub const MAX_COUNTING_GROUP: usize = SLOTS;

/// Hardware placement constraints carried with a group.  They are explicit
/// because counter availability alone cannot represent PEBS mask or the
/// singleton/shared resources on a hybrid PMU.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct CountingConstraints {
    pub pebs_counter_mask: u64,
    pub needs_lbr: bool,
    pub offcore_slots: u8,
    pub needs_topdown: bool,
    pub smt_shared_slots: u8,
}

/// A task-context request to place one architectural counter in a group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CountingProgram {
    pub event: Event,
    /// Kernel-owned identity used to match a completion without retaining an
    /// Arc or a file reference in interrupt context.
    pub cookie: u64,
}

/// Opaque result of an all-or-nothing local placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CountingPlacement {
    pub cpu: usize,
    pub generation: u64,
    pub slots: [u8; MAX_COUNTING_GROUP],
    pub len: u8,
    pub constraints: CountingConstraints,
}

/// A final counter delta published by the no-lock reconciliation path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CountingCompletion {
    pub cookie: u64,
    pub generation: u64,
    pub delta: u64,
    pub overflowed: bool,
}

#[repr(C, align(64))]
struct CountingSlot {
    /// IDLE -> RESERVED -> RUNNING -> COMPLETED.  The IPI accepts only the
    /// exact RUNNING generation, making old mailboxes harmless.
    state: AtomicU8,
    generation: AtomicU64,
    owner_cookie: AtomicU64,
    config: AtomicU64,
    start: AtomicU64,
    width: AtomicU8,
    saved_global: AtomicU64,
    saved_control: AtomicU64,
    saved_counter: AtomicU64,
    delta: AtomicU64,
    overflowed: AtomicBool,
}

impl CountingSlot {
    const IDLE: u8 = 0;
    const RESERVED: u8 = 1;
    const RUNNING: u8 = 2;
    const STOPPING: u8 = 3;
    const COMPLETED: u8 = 4;

    const fn new() -> Self {
        Self {
            state: AtomicU8::new(Self::IDLE),
            generation: AtomicU64::new(0),
            owner_cookie: AtomicU64::new(0),
            config: AtomicU64::new(0),
            start: AtomicU64::new(0),
            width: AtomicU8::new(0),
            saved_global: AtomicU64::new(0),
            saved_control: AtomicU64::new(0),
            saved_counter: AtomicU64::new(0),
            delta: AtomicU64::new(0),
            overflowed: AtomicBool::new(false),
        }
    }
}

static COUNTING_SLOTS: [[CountingSlot; SLOTS]; crate::config::plat::MAX_CPU_NUM] =
    [const { [const { CountingSlot::new() }; SLOTS] }; crate::config::plat::MAX_CPU_NUM];
#[cfg(feature = "pmu-sampling")]
/// A programmable-counter overflow sampling configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplingProgram {
    pub event: Event,
    pub period: u64,
    pub count_user: bool,
    pub count_kernel: bool,
    pub cookie: u64,
}
#[cfg(feature = "pmu-sampling")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PmiSample {
    pub cookie: u64,
    pub period: u64,
}
/// A completed PMI period handed from the NMI-safe local slot to normal
/// context.  This is plain data: consuming it never retains a PMU lease.
#[cfg(feature = "pmu-sampling")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PmiCompletion {
    pub sample: PmiSample,
    pub counter_bit: u64,
    pub generation: u64,
    pub residual: u64,
    pub overflowed: bool,
    /// Number of earlier periods dropped because this CPU's fixed NMI
    /// completion ring was full.  A later successful reservation carries the
    /// exact accumulated loss to normal context.
    pub lost: u64,
    pub ip: u64,
    pub user: bool,
}
#[cfg(feature = "pmu-sampling")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StopSample {
    pub residual: u64,
    pub overflowed: bool,
    pub lost: bool,
}
#[cfg(feature = "pmu-sampling")]
/// A linear local-CPU sampling owner; it is deliberately not `Copy`.
#[derive(Debug)]
pub struct SamplingToken {
    cpu: usize,
    slot: Slot,
    generation: u64,
    cookie: u64,
    active: bool,
}

/// Architectural programmable-counter bit selected for this live sampling
/// generation.  Exact PEBS setup consumes this before the token is published
/// to another context; it must never be inferred from an event encoding.
#[cfg(feature = "pmu-sampling")]
pub const fn sampling_token_counter_bit(token: &SamplingToken) -> u64 {
    bit(token.slot)
}

// The NMI half never touches `MANAGERS`: that state is deliberately protected
// by an ordinary lock for task-context programming/restoration.  Keep all
// data the NMI needs in one cache-line aligned, preallocated slot per CPU.
#[cfg(feature = "pmu-sampling")]
#[repr(C, align(64))]
struct NmiPmiSlot {
    state: AtomicU8,
    generation: AtomicU64,
    /// Generation attached to the one preallocated completion reservation.
    /// Ownership may be invalidated by close before normal context drains it.
    completion_generation: AtomicU64,
    counter_bit: AtomicU64,
    counter_msr: AtomicUsize,
    cookie: AtomicU64,
    period: AtomicU64,
    width: AtomicU8,
    lvt_perf: AtomicU32,
    residual: AtomicU64,
    overflowed: AtomicBool,
    lost: AtomicU64,
    nested: AtomicU64,
    ip: AtomicU64,
    user: AtomicBool,
}

#[cfg(feature = "pmu-sampling")]
impl NmiPmiSlot {
    const IDLE: u8 = 0;
    const ARMED: u8 = 1;
    const HANDLING: u8 = 2;

    const fn new() -> Self {
        Self {
            state: AtomicU8::new(Self::IDLE),
            generation: AtomicU64::new(0),
            completion_generation: AtomicU64::new(0),
            counter_bit: AtomicU64::new(0),
            counter_msr: AtomicUsize::new(0),
            cookie: AtomicU64::new(0),
            period: AtomicU64::new(0),
            width: AtomicU8::new(0),
            lvt_perf: AtomicU32::new(0),
            residual: AtomicU64::new(0),
            overflowed: AtomicBool::new(false),
            lost: AtomicU64::new(0),
            nested: AtomicU64::new(0),
            ip: AtomicU64::new(0),
            user: AtomicBool::new(false),
        }
    }

    fn arm(&self, slot: Slot, generation: u64, cookie: u64, period: u64, width: u8, lvt_perf: u32) {
        self.counter_bit.store(bit(slot), Ordering::Relaxed);
        self.counter_msr
            .store(slot_msr(slot) as usize, Ordering::Relaxed);
        self.cookie.store(cookie, Ordering::Relaxed);
        self.period.store(period, Ordering::Relaxed);
        self.width.store(width, Ordering::Relaxed);
        self.lvt_perf.store(lvt_perf, Ordering::Relaxed);
        self.residual.store(0, Ordering::Relaxed);
        self.overflowed.store(false, Ordering::Relaxed);
        self.generation.store(generation, Ordering::Release);
        self.state.store(Self::ARMED, Ordering::Release);
    }

    fn disarm_generation(&self, generation: u64) -> bool {
        if self
            .generation
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.state.store(Self::IDLE, Ordering::Release);
        true
    }
}

#[cfg(feature = "pmu-sampling")]
static NMI_PMI: [[NmiPmiSlot; MAX]; crate::config::plat::MAX_CPU_NUM] =
    [const { [const { NmiPmiSlot::new() }; MAX] }; crate::config::plat::MAX_CPU_NUM];

/// The NMI producer and normal-context consumer are strictly SPSC per CPU.
/// 256 entries cover a full high-rate batch without tying progress to the
/// scheduler tick; the ring itself and every entry are statically allocated.
#[cfg(feature = "pmu-sampling")]
const NMI_COMPLETION_RING_CAPACITY: usize = 256;

#[cfg(feature = "pmu-sampling")]
#[repr(C, align(64))]
struct NmiCompletionCell {
    cookie: AtomicU64,
    counter_bit: AtomicU64,
    period: AtomicU64,
    generation: AtomicU64,
    residual: AtomicU64,
    lost: AtomicU64,
    ip: AtomicU64,
    overflowed: AtomicBool,
    user: AtomicBool,
}

#[cfg(feature = "pmu-sampling")]
impl NmiCompletionCell {
    const fn new() -> Self {
        Self {
            cookie: AtomicU64::new(0),
            counter_bit: AtomicU64::new(0),
            period: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            residual: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            ip: AtomicU64::new(0),
            overflowed: AtomicBool::new(false),
            user: AtomicBool::new(false),
        }
    }
}

#[cfg(feature = "pmu-sampling")]
#[repr(C, align(64))]
struct NmiCompletionRing {
    head: AtomicUsize,
    tail: AtomicUsize,
    cells: [NmiCompletionCell; NMI_COMPLETION_RING_CAPACITY],
}

#[cfg(feature = "pmu-sampling")]
impl NmiCompletionRing {
    const fn new() -> Self {
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            cells: [const { NmiCompletionCell::new() }; NMI_COMPLETION_RING_CAPACITY],
        }
    }

    /// NMI-only producer half.  No locks, allocation, usercopy or wakeups.
    fn try_push(&self, completion: PmiCompletion) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail.wrapping_sub(self.head.load(Ordering::Acquire)) >= NMI_COMPLETION_RING_CAPACITY {
            return false;
        }
        let cell = &self.cells[tail & (NMI_COMPLETION_RING_CAPACITY - 1)];
        cell.cookie
            .store(completion.sample.cookie, Ordering::Relaxed);
        cell.counter_bit
            .store(completion.counter_bit, Ordering::Relaxed);
        cell.period
            .store(completion.sample.period, Ordering::Relaxed);
        cell.generation
            .store(completion.generation, Ordering::Relaxed);
        cell.residual.store(completion.residual, Ordering::Relaxed);
        cell.lost.store(completion.lost, Ordering::Relaxed);
        cell.ip.store(completion.ip, Ordering::Relaxed);
        cell.overflowed
            .store(completion.overflowed, Ordering::Relaxed);
        cell.user.store(completion.user, Ordering::Relaxed);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Normal-context consumer half; bounded callers supply their batch.
    fn pop(&self) -> Option<PmiCompletion> {
        let head = self.head.load(Ordering::Relaxed);
        if head == self.tail.load(Ordering::Acquire) {
            return None;
        }
        let cell = &self.cells[head & (NMI_COMPLETION_RING_CAPACITY - 1)];
        let completion = PmiCompletion {
            sample: PmiSample {
                cookie: cell.cookie.load(Ordering::Relaxed),
                period: cell.period.load(Ordering::Relaxed),
            },
            counter_bit: cell.counter_bit.load(Ordering::Relaxed),
            generation: cell.generation.load(Ordering::Relaxed),
            residual: cell.residual.load(Ordering::Relaxed),
            overflowed: cell.overflowed.load(Ordering::Relaxed),
            lost: cell.lost.load(Ordering::Relaxed),
            ip: cell.ip.load(Ordering::Relaxed),
            user: cell.user.load(Ordering::Relaxed),
        };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(completion)
    }

    fn has_pending(&self) -> bool {
        self.head.load(Ordering::Acquire) != self.tail.load(Ordering::Acquire)
    }
}

#[cfg(feature = "pmu-sampling")]
static NMI_COMPLETIONS: [NmiCompletionRing; crate::config::plat::MAX_CPU_NUM] =
    [const { NmiCompletionRing::new() }; crate::config::plat::MAX_CPU_NUM];
pub fn capabilities() -> Result<Capabilities, Error> {
    capability_snapshot().map(|snapshot| snapshot.capabilities)
}

/// Return this CPU's fleet-committed capability record.
///
/// No counter API is exposed before the entire online fleet has committed;
/// this prevents a hybrid system from accepting an event on a CPU whose peer
/// later proves incompatible.
pub fn capability_snapshot() -> Result<CapabilitySnapshot, Error> {
    #[cfg(not(target_os = "none"))]
    return Err(Error::Unsupported);

    #[cfg(target_os = "none")]
    {
        let cpu = crate::cpu::current_logical_cpu_id();
        if FLEET_PHASE.load(Ordering::Acquire) != FLEET_ACTIVE {
            return Err(Error::Unsupported);
        }
        let state = FLEET_STATES[cpu].lock();
        state
            .prepared
            .then_some(state.snapshot)
            .ok_or(Error::Unsupported)
    }
}

/// Return a committed capability record for an arbitrary online CPU without
/// touching that CPU's MSRs.  This is for topology/sysfs publication only;
/// counter programming remains strictly local-CPU.
pub fn fleet_capability_snapshot(cpu: usize) -> Result<CapabilitySnapshot, Error> {
    if FLEET_PHASE.load(Ordering::Acquire) != FLEET_ACTIVE || cpu >= crate::cpu::cpu_num() {
        return Err(Error::Unsupported);
    }
    let state = FLEET_STATES.get(cpu).ok_or(Error::Unsupported)?.lock();
    state
        .prepared
        .then_some(state.snapshot)
        .ok_or(Error::Unsupported)
}

/// Number of online CPUs covered by the committed PMU fleet.
pub fn fleet_cpu_count() -> Result<usize, Error> {
    (FLEET_PHASE.load(Ordering::Acquire) == FLEET_ACTIVE)
        .then_some(crate::cpu::cpu_num())
        .ok_or(Error::Unsupported)
}

#[cfg(target_os = "none")]
fn detect_current() -> Result<CapabilitySnapshot, Error> {
    let l0 = __cpuid_count(0, 0);
    if l0.eax < 0x0a || [l0.ebx, l0.edx, l0.ecx] != [0x756e6547, 0x49656e69, 0x6c65746e] {
        return Err(Error::Unsupported);
    }
    let leaf1 = __cpuid_count(1, 0);
    if leaf1.ecx & (1 << 31) != 0 {
        return Err(Error::Hypervisor);
    }
    let (family, model) = decode_display_family_model(leaf1.eax);
    let c = {
        let p = __cpuid_count(0x0a, 0);
        Capabilities::decode(p.eax, p.ebx, p.edx)
    };
    if !c.valid() || !c.usable() {
        return Err(Error::Unsupported);
    }
    let product = if family == 6 && model == 0xcc {
        ProductClass::PantherLake
    } else {
        ProductClass::ArchitecturalOnly
    };
    let core_type = crate::cpu::topology_for_logical(crate::cpu::current_logical_cpu_id())
        .map(|topology| topology.core_type)
        .unwrap_or(crate::cpu::IntelCoreType::Unknown(0));
    if product == ProductClass::PantherLake
        && !matches!(
            core_type,
            crate::cpu::IntelCoreType::Core | crate::cpu::IntelCoreType::Atom
        )
    {
        return Err(Error::Unsupported);
    }
    Ok(CapabilitySnapshot {
        capabilities: c,
        family,
        model,
        core_type,
        product,
    })
}

#[cfg(not(target_os = "none"))]
fn detect_current() -> Result<CapabilitySnapshot, Error> {
    Err(Error::Unsupported)
}

/// Snapshot the PMU state this module owns on the current CPU.  PMU overflow
/// bits are W1C and cannot be recreated, so a pending overflow is an external
/// owner and rejects the fleet before it can claim any state.
#[cfg(target_os = "none")]
fn capture_baseline(c: Capabilities) -> Result<Baseline, Error> {
    let mut baseline = Baseline {
        global_ctrl: read(GLOBAL),
        fixed_ctrl: read(FIXED_CTRL),
        overflow_status: read(STATUS),
        ..EMPTY_BASELINE
    };
    if baseline.overflow_status != 0 {
        return Err(Error::Busy);
    }
    for index in 0..c.programmable_counters as usize {
        baseline.event_select[index] = read(EVT + index as u32);
        baseline.programmable[index] = read(PMC + index as u32);
    }
    for index in 0..c.fixed_counters as usize {
        baseline.fixed[index] = read(FIXED + index as u32);
    }
    // Debug Store is independent from architectural PMU enumeration.  Never
    // probe these MSRs unless CPUID.1 explicitly advertises it.
    if __cpuid_count(1, 0).edx & (1 << 21) != 0 {
        baseline.debugctl = read(DEBUGCTL);
        baseline.ds_area = read(DS_AREA);
        baseline.debug_store = true;
    }
    #[cfg(feature = "pmu-sampling")]
    {
        baseline.lvt_perf = unsafe { crate::apic::read_lvt_perf() };
    }
    Ok(baseline)
}

/// Prepare this CPU for fleet PMU use without changing hardware state.
pub fn prepare_current() -> Result<(), Error> {
    if FLEET_PHASE.load(Ordering::Acquire) != FLEET_COLLECTING {
        return Err(Error::Unsupported);
    }
    let cpu = crate::cpu::current_logical_cpu_id();
    let result = (|| {
        let snapshot = detect_current()?;
        #[cfg(target_os = "none")]
        let baseline = capture_baseline(snapshot.capabilities)?;
        #[cfg(not(target_os = "none"))]
        let baseline = EMPTY_BASELINE;
        let mut state = FLEET_STATES[cpu].lock();
        if state.prepared {
            return Ok(false);
        }
        state.snapshot = snapshot;
        state.baseline = baseline;
        state.prepared = true;
        Ok(true)
    })();
    match result {
        Ok(newly_prepared) => {
            if newly_prepared {
                PREPARED_CPUS.fetch_add(1, Ordering::AcqRel);
            }
            Ok(())
        }
        Err(error) => {
            abort_fleet();
            Err(error)
        }
    }
}

/// Commit only after every online CPU has successfully prepared.
pub fn commit_current() -> bool {
    if FLEET_PHASE.load(Ordering::Acquire) == FLEET_ABORTED
        || PREPARED_CPUS.load(Ordering::Acquire) != crate::cpu::cpu_num()
    {
        return false;
    }
    // Product class is global: mixing a Panther Lake whitelist member with an
    // unrelated model would make a single PMU policy ambiguous. Counter
    // counts/widths intentionally remain per-CPU, and consumers must use the
    // local snapshot when placing an architectural event.
    if !fleet_product_is_consistent() {
        abort_fleet();
        return false;
    }
    FLEET_PHASE
        .compare_exchange(
            FLEET_COLLECTING,
            FLEET_ACTIVE,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
        || FLEET_PHASE.load(Ordering::Acquire) == FLEET_ACTIVE
}

fn fleet_product_is_consistent() -> bool {
    let expected = crate::cpu::cpu_num();
    let Some(fleet_states) = FLEET_STATES.get(..expected) else {
        return false;
    };
    let mut product = None;
    let mut package = None;
    let mut saw_core = false;
    let mut saw_atom = false;
    for (cpu, state) in fleet_states.iter().enumerate() {
        let state = state.lock();
        if !state.prepared {
            return false;
        }
        if let Some(previous) = product {
            if previous != state.snapshot.product {
                return false;
            }
        } else {
            product = Some(state.snapshot.product);
        }
        if state.snapshot.product == ProductClass::PantherLake {
            if !crate::cpu::x2apic_supported() {
                return false;
            }
            let Some(topology) = crate::cpu::topology_for_logical(cpu) else {
                return false;
            };
            if let Some(previous) = package {
                if previous != topology.package_id {
                    return false;
                }
            } else {
                package = Some(topology.package_id);
            }
            match state.snapshot.core_type {
                crate::cpu::IntelCoreType::Core => saw_core = true,
                crate::cpu::IntelCoreType::Atom => saw_atom = true,
                crate::cpu::IntelCoreType::Unknown(_) => return false,
            }
        }
    }
    product.is_some() && (product != Some(ProductClass::PantherLake) || (saw_core && saw_atom))
}

/// Disable the PMU fleet.  Prepare is read-only, thus abort never leaves a
/// partially programmed peer; each prepared CPU retains its saved baseline.
pub fn abort_current() {
    abort_fleet();
}

fn abort_fleet() {
    FLEET_PHASE.store(FLEET_ABORTED, Ordering::Release);
}

pub fn is_active() -> bool {
    FLEET_PHASE.load(Ordering::Acquire) == FLEET_ACTIVE
}

/// Restore this CPU's complete PMU baseline.  This is idempotent and is the
/// local rendezvous hook for kexec; a later kexec coordinator supplies IPIs.
pub fn restore_current_baseline() -> Result<(), Error> {
    let _guard = kernel_guard::NoPreemptIrqSave::new();
    let cpu = crate::cpu::current_logical_cpu_id();
    let state = FLEET_STATES[cpu].lock();
    if !state.prepared {
        return Err(Error::Unsupported);
    }
    // Restore is terminal for all current local tokens: invalidate them before
    // writing the hardware baseline so no stale lease can re-enable a PMU
    // register after kexec quiesce has started.
    let mut manager = MANAGERS[cpu].lock();
    for slot in &mut manager.slots {
        slot.owned = false;
        slot.abandoned = false;
        slot.sampling = false;
        match slot.generation.checked_add(1) {
            Some(next) => slot.generation = next,
            None => slot.retired = true,
        }
    }
    // Counting slots are lock-free so the terminal kexec restore path can
    // invalidate an in-flight reconcile without touching its ownership state.
    for slot in &COUNTING_SLOTS[cpu] {
        slot.generation.fetch_add(1, Ordering::AcqRel);
        slot.state.store(CountingSlot::IDLE, Ordering::Release);
    }
    #[cfg(target_os = "none")]
    restore_baseline(state.snapshot.capabilities, state.baseline);
    Ok(())
}

/// Irreversibly silence every architectural PMU source on the current CPU.
///
/// This is the crash-kexec counterpart of `restore_current_baseline`: it is
/// deliberately independent of the fleet and lease locks because a panic may
/// have interrupted their owner.  The destination kernel must never inherit
/// an enabled counter, pending PMI, or Debug Store producer from the failed
/// kernel.  Capability discovery is repeated with CPUID so no shared software
/// state is consulted on this terminal path.
#[cfg(target_os = "none")]
pub fn crash_quiesce_current() {
    let vendor = __cpuid_count(0, 0);
    if vendor.eax < 0x0a
        || [vendor.ebx, vendor.edx, vendor.ecx] != [0x756e6547, 0x49656e69, 0x6c65746e]
    {
        return;
    }
    let perf = __cpuid_count(0x0a, 0);
    let version = perf.eax as u8;
    if version < 4 {
        return;
    }
    let programmable = ((perf.eax >> 8) as u8 as usize).min(MAX);
    let fixed = ((perf.edx as u8 & 31) as usize).min(MAX);

    write(GLOBAL, 0);
    #[cfg(feature = "pmu-sampling")]
    unsafe {
        crate::apic::write_lvt_perf(crate::apic::read_lvt_perf() | (1 << 16));
    }
    for index in 0..programmable {
        write(EVT + index as u32, 0);
        write(PMC + index as u32, 0);
    }
    for index in 0..fixed {
        write(FIXED + index as u32, 0);
    }
    write(FIXED_CTRL, 0);
    write(
        OVF,
        supported_counter_bits(Capabilities::decode(perf.eax, perf.ebx, perf.edx)),
    );

    let leaf1 = __cpuid_count(1, 0);
    if leaf1.edx & (1 << 21) != 0 {
        write(PEBS_ENABLE, 0);
        write(DEBUGCTL, 0);
        write(DS_AREA, 0);
    }
}

#[cfg(target_os = "none")]
fn restore_baseline(c: Capabilities, baseline: Baseline) {
    write(GLOBAL, 0);
    #[cfg(feature = "pmu-sampling")]
    unsafe {
        crate::apic::write_lvt_perf(baseline.lvt_perf)
    };
    for index in 0..c.programmable_counters as usize {
        write(EVT + index as u32, baseline.event_select[index]);
        write(PMC + index as u32, baseline.programmable[index]);
    }
    for index in 0..c.fixed_counters as usize {
        write(FIXED + index as u32, baseline.fixed[index]);
    }
    write(FIXED_CTRL, baseline.fixed_ctrl);
    if baseline.debug_store {
        write(DEBUGCTL, baseline.debugctl);
        write(DS_AREA, baseline.ds_area);
    }
    // `capture_baseline` accepts only a clear overflow latch, so W1C is a
    // faithful restoration rather than an attempt to manufacture old latches.
    write(OVF, supported_counter_bits(c));
    write(GLOBAL, baseline.global_ctrl);
}

const fn supported_counter_bits(c: Capabilities) -> u64 {
    let programmable = if c.programmable_counters >= 64 {
        u64::MAX
    } else {
        (1u64 << c.programmable_counters).wrapping_sub(1)
    };
    let fixed = if c.fixed_counters >= 32 {
        0xffff_ffff_0000_0000
    } else {
        ((1u64 << c.fixed_counters).wrapping_sub(1)) << 32
    };
    programmable | fixed
}

/// Decode CPUID.1:EAX's display family and model without relying on a host
/// CPUID library; pure so product gating stays unit-testable.
const fn decode_display_family_model(eax: u32) -> (u8, u8) {
    let base_family = ((eax >> 8) & 0xf) as u8;
    let base_model = ((eax >> 4) & 0xf) as u8;
    let ext_model = ((eax >> 16) & 0xf) as u8;
    let ext_family = ((eax >> 20) & 0xff) as u8;
    let family = if base_family == 0xf {
        base_family.wrapping_add(ext_family)
    } else {
        base_family
    };
    let model = if base_family == 0x6 || base_family == 0xf {
        base_model | (ext_model << 4)
    } else {
        base_model
    };
    (family, model)
}

/* old single-CPU probe retained below only as the fleet's CPUID primitive. */
#[allow(dead_code)]
fn legacy_capabilities_probe() -> Result<Capabilities, Error> {
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
/// Atomically reserve and program every counter in a task group, or leave
/// every counter untouched.  The caller is local to the target CPU; remote
/// teardown is intentionally delegated to `counting_stop_settle_current`.
pub fn counting_place_group_local(
    generation: u64,
    programs: &[CountingProgram],
) -> Result<CountingPlacement, Error> {
    counting_place_group_constrained_local(generation, programs, CountingConstraints::default())
}

/// Constrained variant of placement.  It reserves all ordinary counters
/// before programming any one of them and rejects a group whose constraints
/// cannot be represented by this exact CPU.  Raw encodings are accepted only
/// on the committed Panther Lake core type on which they were opened.
pub fn counting_place_group_constrained_local(
    generation: u64,
    programs: &[CountingProgram],
    constraints: CountingConstraints,
) -> Result<CountingPlacement, Error> {
    if generation == 0 || programs.is_empty() || programs.len() > MAX_COUNTING_GROUP {
        return Err(Error::InvalidProgram);
    }
    let _guard = kernel_guard::NoPreemptIrqSave::new();
    let cpu = crate::cpu::current_logical_cpu_id();
    let snapshot = capability_snapshot()?;
    let caps = snapshot.capabilities;
    let placement_caps = placement_capabilities()?;
    let requested_pebs_mask = if constraints.pebs_counter_mask == u64::MAX {
        placement_caps.pebs_counter_mask
    } else {
        constraints.pebs_counter_mask
    };
    if (constraints.pebs_counter_mask != 0 && requested_pebs_mask == 0)
        || requested_pebs_mask & !placement_caps.pebs_counter_mask != 0
        || constraints.offcore_slots > placement_caps.offcore_slots
        || (constraints.needs_topdown && placement_caps.topdown_slots == 0)
        || constraints.smt_shared_slots > placement_caps.smt_shared_slots
    {
        // Discovery did not advertise this resource. Refuse rather than
        // allocating an arbitrary counter and calling it PEBS/offcore/etc.
        return Err(Error::InvalidProgram);
    }
    if constraints.needs_lbr && !placement_caps.lbr {
        return Err(Error::Unsupported);
    }
    for program in programs {
        if let Event::Raw {
            event_select,
            core_type,
        } = program.event
        {
            // EventSel/umask, edge, invert and cmask only.  USR/OS/INT/EN
            // and AnyThread are ownership/delivery policy and are always
            // installed by the kernel, never accepted from a raw attr.
            const RAW_EVENTSEL_ALLOWED: u64 = 0x0000_0000_ff84_ffff;
            if snapshot.product != ProductClass::PantherLake
                || core_type != snapshot.core_type
                || event_select & !RAW_EVENTSEL_ALLOWED != 0
            {
                return Err(Error::InvalidProgram);
            }
        }
    }
    let slots = COUNTING_SLOTS.get(cpu).ok_or(Error::Unsupported)?;
    // Placement and sampling share the same physical PMC namespace.  The
    // scheduler is local-CPU serialized under NoPreemptIrqSave, and this
    // manager lock closes the remaining task-context race with sampler arm.
    let sampler = MANAGERS[cpu].lock();
    let mut selected = [Slot::P(0); MAX_COUNTING_GROUP];
    let mut selected_len = 0;
    for program in programs {
        let preferred = if caps.fixed_ok(program.event) {
            program.event.fixed().map(Slot::F)
        } else {
            None
        };
        let candidate = preferred
            .filter(|slot| slots[slot.idx()].state.load(Ordering::Acquire) == CountingSlot::IDLE)
            .filter(|slot| !sampler.slots[slot.idx()].owned)
            .or_else(|| {
                (0..caps.programmable_counters as usize)
                    .map(|i| Slot::P(i as u8))
                    .find(|slot| {
                        caps.programmable(program.event)
                            && (requested_pebs_mask == 0
                                || requested_pebs_mask
                                    & (1 << match slot {
                                        Slot::P(i) => i,
                                        Slot::F(_) => unreachable!(),
                                    })
                                    != 0)
                            && !selected[..selected_len].contains(slot)
                            && !sampler.slots[slot.idx()].owned
                            && slots[slot.idx()].state.load(Ordering::Acquire) == CountingSlot::IDLE
                    })
            })
            .ok_or(Error::NoCounter)?;
        if selected[..selected_len].contains(&candidate) {
            return Err(Error::NoCounter);
        }
        selected[selected_len] = candidate;
        selected_len += 1;
    }
    // Reserve first: no counter is programmed until every requested slot is
    // exclusively ours. A racing placement rolls back its own reservations.
    for slot in selected[..selected_len].iter().copied() {
        if slots[slot.idx()]
            .state
            .compare_exchange(
                CountingSlot::IDLE,
                CountingSlot::RESERVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            for prior in selected[..selected_len].iter().copied() {
                if prior == slot {
                    break;
                }
                slots[prior.idx()]
                    .state
                    .store(CountingSlot::IDLE, Ordering::Release);
            }
            return Err(Error::Busy);
        }
    }
    let mut placement = CountingPlacement {
        cpu,
        generation,
        slots: [0; MAX_COUNTING_GROUP],
        len: selected_len as u8,
        constraints,
    };
    for (programmed, (slot, program_request)) in selected[..selected_len]
        .iter()
        .copied()
        .zip(programs.iter().copied())
        .enumerate()
    {
        let header = &slots[slot.idx()];
        let Some(saved) = prepare_idle(slot) else {
            for rollback in selected[..programmed].iter().copied() {
                restore_counting_slot(&slots[rollback.idx()], rollback);
                slots[rollback.idx()]
                    .state
                    .store(CountingSlot::IDLE, Ordering::Release);
            }
            for rollback in selected[programmed..selected_len].iter().copied() {
                slots[rollback.idx()]
                    .state
                    .store(CountingSlot::IDLE, Ordering::Release);
            }
            return Err(Error::Busy);
        };
        let width = match slot {
            Slot::P(_) => caps.programmable_width,
            Slot::F(_) => caps.fixed_width,
        };
        header.generation.store(generation, Ordering::Relaxed);
        header
            .owner_cookie
            .store(program_request.cookie, Ordering::Relaxed);
        header
            .config
            .store(counting_event(program_request.event), Ordering::Relaxed);
        header.start.store(0, Ordering::Relaxed);
        header.width.store(width, Ordering::Relaxed);
        header.saved_global.store(saved.global, Ordering::Relaxed);
        header.saved_control.store(saved.control, Ordering::Relaxed);
        header.saved_counter.store(saved.counter, Ordering::Relaxed);
        header.delta.store(0, Ordering::Relaxed);
        header.overflowed.store(false, Ordering::Relaxed);
        program(slot, program_request.event);
        placement.slots[programmed] = slot.idx() as u8;
    }
    for slot in selected[..selected_len].iter().copied() {
        slots[slot.idx()]
            .state
            .store(CountingSlot::RUNNING, Ordering::Release);
    }
    drop(sampler);
    Ok(placement)
}

/// IRQ/IPI-safe local half of a group close.  It never takes `MANAGERS`,
/// allocates, retains ownership, or acknowledges foreign overflow bits.
pub fn counting_stop_settle_current(generation: u64) -> Result<usize, Error> {
    let cpu = crate::cpu::current_logical_cpu_id();
    let slots = COUNTING_SLOTS.get(cpu).ok_or(Error::Unsupported)?;
    let mut stopped = 0;
    for (index, header) in slots.iter().enumerate() {
        if header.generation.load(Ordering::Acquire) != generation
            || header
                .state
                .compare_exchange(
                    CountingSlot::RUNNING,
                    CountingSlot::STOPPING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
        {
            continue;
        }
        let slot = if index < MAX {
            Slot::P(index as u8)
        } else {
            Slot::F((index - MAX) as u8)
        };
        disable(slot);
        let width = header.width.load(Ordering::Relaxed);
        let delta = read(slot_msr(slot)).wrapping_sub(header.start.load(Ordering::Relaxed))
            & Capabilities::mask(width);
        let owned_bit = bit(slot);
        let status = read(STATUS);
        let overflowed = owned_overflow_w1c(status, owned_bit) != 0;
        if overflowed {
            write(OVF, owned_bit);
        }
        restore_counting_slot(header, slot);
        header.delta.store(delta, Ordering::Relaxed);
        header.overflowed.store(overflowed, Ordering::Relaxed);
        // completion before state is the release publication consumed after IPI ack.
        header
            .state
            .store(CountingSlot::COMPLETED, Ordering::Release);
        stopped += 1;
    }
    Ok(stopped)
}

/// Consume completed counters in normal task context. Each completion is
/// claimed exactly once by the COMPLETED -> IDLE transition.
pub fn counting_take_completion_local(
    generation: u64,
    out: &mut [CountingCompletion],
) -> Result<usize, Error> {
    let cpu = crate::cpu::current_logical_cpu_id();
    let slots = COUNTING_SLOTS.get(cpu).ok_or(Error::Unsupported)?;
    let mut count = 0;
    for header in slots {
        if count == out.len() || header.generation.load(Ordering::Acquire) != generation {
            continue;
        }
        if header
            .state
            .compare_exchange(
                CountingSlot::COMPLETED,
                CountingSlot::IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            out[count] = CountingCompletion {
                cookie: header.owner_cookie.load(Ordering::Relaxed),
                generation,
                delta: header.delta.load(Ordering::Relaxed),
                overflowed: header.overflowed.load(Ordering::Relaxed),
            };
            count += 1;
        }
    }
    Ok(count)
}

/// Snapshot completed entries for a local IPI mailbox without consuming them.
/// The receiver must later call `counting_take_completion_local` to claim the
/// entries; this makes an IPI acknowledgement and normal-context accounting
/// observe the same exact completion without a second settlement.
pub fn counting_copy_completion_current(
    generation: u64,
    out: &mut [CountingCompletion],
) -> Result<usize, Error> {
    let cpu = crate::cpu::current_logical_cpu_id();
    let slots = COUNTING_SLOTS.get(cpu).ok_or(Error::Unsupported)?;
    let mut count = 0;
    for header in slots {
        if count == out.len() || header.generation.load(Ordering::Acquire) != generation {
            continue;
        }
        if header.state.load(Ordering::Acquire) == CountingSlot::COMPLETED {
            out[count] = CountingCompletion {
                cookie: header.owner_cookie.load(Ordering::Relaxed),
                generation,
                delta: header.delta.load(Ordering::Relaxed),
                overflowed: header.overflowed.load(Ordering::Relaxed),
            };
            count += 1;
        }
    }
    Ok(count)
}

/// Release snapshots after an IPI has copied them into its preallocated
/// kernel mailbox.  This is deliberately separate from copying so a sender
/// can account mailbox contents exactly once after observing the IPI ACK.
pub fn counting_release_completed_current(generation: u64) -> Result<usize, Error> {
    let cpu = crate::cpu::current_logical_cpu_id();
    let slots = COUNTING_SLOTS.get(cpu).ok_or(Error::Unsupported)?;
    let mut released = 0;
    for header in slots {
        if header.generation.load(Ordering::Acquire) == generation
            && header
                .state
                .compare_exchange(
                    CountingSlot::COMPLETED,
                    CountingSlot::IDLE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        {
            released += 1;
        }
    }
    Ok(released)
}

fn restore_counting_slot(header: &CountingSlot, slot: Slot) {
    match slot {
        Slot::P(i) => write(EVT + i as u32, header.saved_control.load(Ordering::Relaxed)),
        Slot::F(i) => {
            let now = read(FIXED_CTRL);
            let mask = 15 << (i * 4);
            write(
                FIXED_CTRL,
                (now & !mask) | (header.saved_control.load(Ordering::Relaxed) & mask),
            );
        }
    }
    write(slot_msr(slot), header.saved_counter.load(Ordering::Relaxed));
    let now = read(GLOBAL);
    write(
        GLOBAL,
        (now & !bit(slot)) | (header.saved_global.load(Ordering::Relaxed) & bit(slot)),
    );
}

#[cfg(feature = "pmu-sampling")]
const LVT_MASKED: u32 = 1 << 16;
#[cfg(feature = "pmu-sampling")]
const LVT_DELIVERY_MODE: u32 = 0b111 << 8;
#[cfg(feature = "pmu-sampling")]
const LVT_DELIVERY_NMI: u32 = 0b100 << 8;
#[cfg(feature = "pmu-sampling")]
const fn sampling_preload(width: u8, period: u64) -> u64 {
    Capabilities::mask(width)
        .wrapping_add(1)
        .wrapping_sub(period)
}
#[cfg(feature = "pmu-sampling")]
const fn sampling_event(event: Event, user: bool, kernel: bool) -> u64 {
    let raw = match event {
        Event::Raw { event_select, .. } => event_select,
        _ => event.code() as u64,
    };
    // EN/INT/USR/OS/AnyThread are PMU ownership bits. Raw callers may choose
    // event/umask/edge/invert/cmask only; the kernel supplies delivery policy.
    (raw & !((1 << 16) | (1 << 17) | (1 << 20) | (1 << 21) | (1 << 22)))
        | (user as u64) << 16
        | (kernel as u64) << 17
        | 1 << 20
        | 1 << 22
}
const fn counting_event(event: Event) -> u64 {
    let raw = match event {
        Event::Raw { event_select, .. } => event_select,
        _ => event.code() as u64,
    };
    (raw & !((1 << 16) | (1 << 17) | (1 << 20) | (1 << 21) | (1 << 22)))
        | (1 << 16)
        | (1 << 17)
        | (1 << 22)
}
#[cfg(feature = "pmu-sampling")]
fn sampling_live(m: &Manager) -> usize {
    m.slots[..MAX]
        .iter()
        .filter(|state| state.owned && state.sampling)
        .count()
}

#[cfg(feature = "pmu-sampling")]
fn nmi_slot(cpu: usize, slot: Slot) -> Result<&'static NmiPmiSlot, Error> {
    let Slot::P(index) = slot else {
        return Err(Error::InvalidProgram);
    };
    NMI_PMI
        .get(cpu)
        .and_then(|slots| slots.get(index as usize))
        .ok_or(Error::Unsupported)
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
    // APIC delivery mode NMI ignores the vector field and enters vector 2.
    // Retain dormant firmware configuration only in the saved value; never
    // deliver a live PMI through the ordinary maskable IRQ path.
    (saved & !(0xff | LVT_MASKED | LVT_DELIVERY_MODE)) | LVT_DELIVERY_NMI
}

const fn owned_overflow_w1c(status: u64, owned: u64) -> u64 {
    // IA32_PERF_GLOBAL_OVF_CTRL is W1C.  Writing any foreign bit would steal
    // another PMU consumer's interrupt, so return precisely the owned latch.
    status & owned
}

/// Arms one local programmable PMU counter for PMI delivery. Multiple calls
/// may coexist up to the CPUID-advertised programmable-counter count; each
/// PMC has its own generation, cookie, period and cacheline-local NMI state.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_arm_local(program: SamplingProgram) -> Result<SamplingToken, Error> {
    local(|cpu, m| {
        let _ = reap(m);
        if capability_snapshot()?.product != ProductClass::PantherLake {
            return Err(Error::Unsupported);
        }
        let c = capabilities()?;
        if !c.programmable(program.event)
            || !program.count_user && !program.count_kernel
            || program.period < 4096
            || program.period > c.programmable_mask()
        {
            return Err(Error::InvalidProgram);
        }
        let lvt = unsafe { crate::apic::read_lvt_perf() };
        if m.sampling_lvt.is_none() && !lvt_is_safe_sampling_baseline(lvt) {
            return Err(Error::Busy);
        }
        let (slot, saved) = select(m, c)?;
        if COUNTING_SLOTS[cpu][slot.idx()]
            .state
            .load(Ordering::Acquire)
            != CountingSlot::IDLE
        {
            return Err(Error::NoCounter);
        }
        if m.sampling_lvt.is_none() {
            m.sampling_lvt = Some(lvt);
        }
        let baseline_lvt = m.sampling_lvt.unwrap_or(lvt);
        let state = &mut m.slots[slot.idx()];
        claim_generation(state)?; // before the first register write: no ABA on exhaustion.
        unsafe {
            crate::apic::write_lvt_perf(lvt | LVT_MASKED);
        }
        disable(slot);
        write(OVF, bit(slot)); // candidate was idle/status-clear; acknowledge only our bit.
        write(
            EVT + match slot {
                Slot::P(i) => i as u32,
                _ => unreachable!(),
            },
            sampling_event(program.event, program.count_user, program.count_kernel),
        );
        write(
            slot_msr(slot),
            sampling_preload(c.programmable_width, program.period),
        );
        state.owned = true;
        state.sampling = true;
        state.abandoned = false;
        state.saved = saved;
        state.width = c.programmable_width;
        state.cookie = program.cookie;
        state.period = program.period;
        state.lvt_perf = baseline_lvt;
        // Publish only after all metadata and the counter preload are ready;
        // vector-2 can now consume this without taking `MANAGERS`.
        nmi_slot(cpu, slot)?.arm(
            slot,
            state.generation,
            program.cookie,
            program.period,
            state.width,
            baseline_lvt,
        );
        unsafe {
            crate::apic::write_lvt_perf(
                // NMI delivery ignores this field. Keep a masked LVT's
                // vector zero instead of advertising a maskable PMI path.
                (baseline_lvt & !0xff) | LVT_MASKED,
            );
        }
        write(GLOBAL, read(GLOBAL) | bit(slot));
        unsafe {
            crate::apic::write_lvt_perf(sampling_active_lvt(baseline_lvt));
        }
        Ok(SamplingToken {
            cpu,
            slot,
            generation: state.generation,
            cookie: program.cookie,
            active: true,
        })
    })
}

/// Consumes this CPU's owned PMI latch and leaves the sample disarmed.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_take_pmi() -> Result<Option<(PmiSample, u64)>, Error> {
    local(|_, m| {
        let Some(i) = (0..MAX).find(|&i| m.slots[i].owned && m.slots[i].sampling) else {
            return Ok(None);
        };
        let slot = Slot::P(i as u8);
        // A stray PMI is not ours.  Do not perturb the sampler (or any other
        // PMU owner's latch) before proving that this owned bit is asserted.
        if read(STATUS) & bit(slot) == 0 {
            return Ok(None);
        }
        let s = &m.slots[i];
        let sample = PmiSample {
            cookie: s.cookie,
            period: s.period,
        };
        let generation = s.generation;
        unsafe {
            crate::apic::write_lvt_perf(crate::apic::read_lvt_perf() | LVT_MASKED);
        }
        disable(slot);
        write(OVF, bit(slot));
        Ok(Some((sample, generation)))
    })
}

/// Consume an owned overflow in vector-2 NMI context.
///
/// This routine touches only preallocated atomics and PMU/APIC MSRs.  It does
/// not acquire `MANAGERS`, allocate, copy to user memory, drop ownership, or
/// wake tasks.  A full consumer ring is drained in normal context.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_nmi_take_pmis(
    ip: u64,
    user: bool,
    out: &mut [PmiCompletion],
) -> Result<usize, Error> {
    let cpu = crate::cpu::current_logical_cpu_id();
    let Some(nmis) = NMI_PMI.get(cpu) else {
        return Err(Error::Unsupported);
    };
    let Some(completions) = NMI_COMPLETIONS.get(cpu) else {
        return Err(Error::Unsupported);
    };
    let status = read(STATUS);
    let mut count = 0;
    // A single PMI may carry several programmable-counter overflow latches.
    // Process every *owned* armed slot and W1C only that slot's bit.  There is
    // no manager access, allocation, ordinary lock, or foreign-bit write.
    for nmi in nmis {
        let counter_bit = nmi.counter_bit.load(Ordering::Relaxed);
        if counter_bit == 0 || owned_overflow_w1c(status, counter_bit) == 0 {
            continue;
        }
        if nmi
            .state
            .compare_exchange(
                NmiPmiSlot::ARMED,
                NmiPmiSlot::HANDLING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            nmi.nested.fetch_add(1, Ordering::Relaxed);
            nmi.lost.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        write(OVF, counter_bit); // W1C precisely the owned overflow bit.
        write(GLOBAL, read(GLOBAL) & !counter_bit);
        let residual = read(nmi.counter_msr.load(Ordering::Relaxed) as u32)
            & Capabilities::mask(nmi.width.load(Ordering::Relaxed));
        let generation = nmi.generation.load(Ordering::Acquire);
        let completion = PmiCompletion {
            sample: PmiSample {
                cookie: nmi.cookie.load(Ordering::Relaxed),
                period: nmi.period.load(Ordering::Relaxed),
            },
            counter_bit,
            generation,
            residual,
            overflowed: true,
            lost: nmi.lost.swap(0, Ordering::AcqRel),
            ip,
            user,
        };
        if completions.try_push(completion) {
            nmi.residual.store(residual, Ordering::Relaxed);
            nmi.overflowed.store(true, Ordering::Relaxed);
            nmi.ip.store(ip, Ordering::Relaxed);
            nmi.user.store(user, Ordering::Relaxed);
            nmi.completion_generation
                .store(generation, Ordering::Release);
            if count < out.len() {
                out[count] = completion;
                count += 1;
            }
        } else {
            // Preserve all losses for the next successful reservation.
            nmi.lost
                .fetch_add(completion.lost.saturating_add(1), Ordering::Relaxed);
        }
        // The exact generation remains the ownership gate.  A concurrent
        // close changes it to zero before disabling the counter, so this NMI
        // cannot resurrect a retired descriptor.
        if nmi.generation.load(Ordering::Acquire) == generation
            && nmi.state.load(Ordering::Acquire) == NmiPmiSlot::HANDLING
        {
            write(
                nmi.counter_msr.load(Ordering::Relaxed) as u32,
                sampling_preload(
                    nmi.width.load(Ordering::Relaxed),
                    nmi.period.load(Ordering::Relaxed),
                ),
            );
            write(OVF, counter_bit);
            write(GLOBAL, read(GLOBAL) | counter_bit);
            unsafe {
                crate::apic::write_lvt_perf(sampling_active_lvt(
                    nmi.lvt_perf.load(Ordering::Relaxed),
                ));
            }
            nmi.state.store(NmiPmiSlot::ARMED, Ordering::Release);
        }
    }
    Ok(count)
}

/// Compatibility single-completion view used by older callers. New NMI
/// consumers must use `sampling_nmi_take_pmis` so simultaneous overflows are
/// each handed to their precise-event owner.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_nmi_take_pmi(ip: u64, user: bool) -> Result<Option<PmiCompletion>, Error> {
    let mut completion = [PmiCompletion {
        sample: PmiSample {
            cookie: 0,
            period: 0,
        },
        counter_bit: 0,
        generation: 0,
        residual: 0,
        overflowed: false,
        lost: 0,
        ip: 0,
        user: false,
    }; 1];
    Ok((sampling_nmi_take_pmis(ip, user, &mut completion)? != 0).then_some(completion[0]))
}

/// Stop this CPU's owned sampling hardware without an ordinary lock.
///
/// It is the reconciliation/IPI acknowledgement point: generation is first
/// invalidated, then LVT and the owned global-counter bit are stopped. Saved
/// MSR restoration remains normal-context work and is performed by the token
/// owner or terminal fleet baseline restore.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_nmi_stop_settle_current(generation: u64) -> Result<Option<PmiCompletion>, Error> {
    let cpu = crate::cpu::current_logical_cpu_id();
    let Some(nmis) = NMI_PMI.get(cpu) else {
        return Err(Error::Unsupported);
    };
    unsafe { crate::apic::write_lvt_perf(crate::apic::read_lvt_perf() | LVT_MASKED) };
    let mut first = None;
    for nmi in nmis {
        if !nmi.disarm_generation(generation) {
            continue;
        }
        let counter_bit = nmi.counter_bit.load(Ordering::Relaxed);
        write(GLOBAL, read(GLOBAL) & !counter_bit);
        let overflowed = owned_overflow_w1c(read(STATUS), counter_bit) != 0;
        if overflowed {
            write(OVF, counter_bit);
        }
        let residual = read(nmi.counter_msr.load(Ordering::Relaxed) as u32)
            & Capabilities::mask(nmi.width.load(Ordering::Relaxed));
        nmi.residual.store(residual, Ordering::Relaxed);
        nmi.overflowed.store(overflowed, Ordering::Relaxed);
        let completion = PmiCompletion {
            sample: PmiSample {
                cookie: nmi.cookie.load(Ordering::Relaxed),
                period: nmi.period.load(Ordering::Relaxed),
            },
            counter_bit,
            generation,
            residual,
            overflowed,
            lost: nmi.lost.swap(0, Ordering::AcqRel),
            ip: nmi.ip.load(Ordering::Relaxed),
            user: nmi.user.load(Ordering::Relaxed),
        };
        if first.is_none() {
            first = Some(completion);
        }
    }
    Ok(first)
}

/// Take the NMI's coalesced wake indication in normal context.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_nmi_take_pending_wake_local() -> bool {
    let cpu = crate::cpu::current_logical_cpu_id();
    NMI_COMPLETIONS
        .get(cpu)
        .is_some_and(NmiCompletionRing::has_pending)
}

/// Drain one NMI completion in normal context.  This is the only place a
/// consumer may wake waiters or publish into a user-visible perf data ring.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_nmi_take_completion_local() -> Option<PmiCompletion> {
    let cpu = crate::cpu::current_logical_cpu_id();
    NMI_COMPLETIONS.get(cpu)?.pop()
}

/// Drain all currently deferred PMI completions into caller-provided storage.
/// This is the multi-counter counterpart of `sampling_nmi_take_completion_local`;
/// it is bounded and does not allocate.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_nmi_take_completions_local(out: &mut [PmiCompletion]) -> usize {
    let mut count = 0;
    while count < out.len() {
        let Some(completion) = sampling_nmi_take_completion_local() else {
            break;
        };
        out[count] = completion;
        count += 1;
    }
    count
}

/// Rearms a disarmed local sample only when its cookie and generation still match.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_rearm_local(cookie: u64, generation: u64) -> Result<(), Error> {
    local(|cpu, m| {
        for i in 0..MAX {
            let s = &mut m.slots[i];
            if s.owned && s.sampling && s.cookie == cookie && s.generation == generation {
                let slot = Slot::P(i as u8);
                write(slot_msr(slot), sampling_preload(s.width, s.period));
                write(OVF, bit(slot));
                write(GLOBAL, read(GLOBAL) | bit(slot));
                unsafe {
                    crate::apic::write_lvt_perf(sampling_active_lvt(s.lvt_perf));
                }
                // A stale NMI/close cannot resurrect this counter: publish
                // ARMED only after both the generation and hardware state
                // still match this exact token.
                nmi_slot(cpu, slot)?.arm(slot, generation, cookie, s.period, s.width, s.lvt_perf);
                return Ok(());
            }
        }
        Err(Error::Stale)
    })
}

#[cfg(feature = "pmu-sampling")]
fn stop_sampling(slot: Slot, s: &mut State) -> StopSample {
    let cpu = crate::cpu::current_logical_cpu_id();
    let _ = nmi_slot(cpu, slot).map(|nmi| nmi.disarm_generation(s.generation));
    unsafe {
        crate::apic::write_lvt_perf(crate::apic::read_lvt_perf() | LVT_MASKED);
    }
    disable(slot);
    let residual = read(slot_msr(slot)) & Capabilities::mask(s.width);
    let overflowed = read(STATUS) & bit(slot) != 0;
    if overflowed {
        write(OVF, bit(slot));
    }
    write(
        EVT + match slot {
            Slot::P(i) => i as u32,
            _ => unreachable!(),
        },
        s.saved.control,
    );
    write(slot_msr(slot), s.saved.counter);
    let now = read(GLOBAL);
    write(GLOBAL, (now & !bit(slot)) | (s.saved.global & bit(slot)));
    s.owned = false;
    s.sampling = false;
    s.abandoned = false;
    StopSample {
        residual,
        overflowed,
        lost: false,
    }
}

#[cfg(feature = "pmu-sampling")]
fn sampling_restore_or_reactivate(m: &mut Manager) {
    let Some(baseline) = m.sampling_lvt else {
        return;
    };
    if sampling_live(m) == 0 {
        unsafe { crate::apic::write_lvt_perf(baseline) };
        m.sampling_lvt = None;
    } else {
        unsafe { crate::apic::write_lvt_perf(sampling_active_lvt(baseline)) };
    }
}

/// Stops a local sampling token and restores exactly its saved PMU/LVT state.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_stop_local(mut token: SamplingToken) -> Result<StopSample, Error> {
    let result = local(|cpu, m| {
        if cpu != token.cpu {
            return Err(Error::Migrated);
        }
        let sample = {
            let s = &mut m.slots[token.slot.idx()];
            if !s.owned
                || !s.sampling
                || s.generation != token.generation
                || s.cookie != token.cookie
            {
                return Err(Error::Stale);
            }
            stop_sampling(token.slot, s)
        };
        sampling_restore_or_reactivate(m);
        Ok(sample)
    });
    if result.is_ok() {
        token.active = false;
    }
    result
}

/// Bounded, idempotent local cleanup for a locally abandoned sampling token.
#[cfg(feature = "pmu-sampling")]
pub fn sampling_quiesce_local() -> Result<usize, Error> {
    local(|_, m| {
        let mut count = 0;
        for i in 0..MAX {
            let s = &mut m.slots[i];
            if s.owned && s.sampling && s.abandoned {
                let _ = stop_sampling(Slot::P(i as u8), s);
                count += 1;
            }
        }
        sampling_restore_or_reactivate(m);
        Ok(count)
    })
}

#[cfg(feature = "pmu-sampling")]
impl Drop for SamplingToken {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _guard = kernel_guard::NoPreemptIrqSave::new();
        let cpu = crate::cpu::current_logical_cpu_id();
        let mut m = MANAGERS[self.cpu].lock();
        if cpu == self.cpu {
            let stopped = {
                let s = &mut m.slots[self.slot.idx()];
                if !s.owned || !s.sampling || s.generation != self.generation {
                    return;
                }
                stop_sampling(self.slot, s)
            };
            let _ = stopped;
            sampling_restore_or_reactivate(&mut m);
        } else {
            let s = &mut m.slots[self.slot.idx()];
            if !s.owned || !s.sampling || s.generation != self.generation {
                return;
            }
            s.abandoned = true;
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
            terminate(slot, state.saved, state.width);
            state.owned = false;
            state.abandoned = false;
            state.sampling = false;
            count += 1;
        }
    }
    #[cfg(feature = "pmu-sampling")]
    sampling_restore_or_reactivate(m);
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
    select_on_cpu(m, c, crate::cpu::current_logical_cpu_id())
}

/// Pure placement half of [`select`].  The hardware-facing wrapper resolves
/// the current logical CPU, while host model tests provide a stable logical
/// slot without requiring a Multiboot/ACPI handoff.
fn select_on_cpu(m: &Manager, c: Capabilities, cpu: usize) -> Result<(Slot, Saved), Error> {
    if cpu >= COUNTING_SLOTS.len() {
        return Err(Error::NoCounter);
    }
    let mut software_free = false;
    for (i, state) in m.slots[..c.programmable_counters as usize].iter().enumerate() {
        if !state.owned
            && !state.retired
            && COUNTING_SLOTS[cpu][i].state.load(Ordering::Acquire) == CountingSlot::IDLE
        {
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
const fn bit(s: Slot) -> u64 {
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
            e.raw_config().unwrap_or(e.code() as u64) | (1 << 16) | (1 << 17) | (1 << 22),
        ),
        Slot::F(i) => {
            let old = read(FIXED_CTRL);
            write(FIXED_CTRL, (old & !(15 << (i * 4))) | (3 << (i * 4)))
        }
    }
    write(slot_msr(s), 0);
    write(GLOBAL, read(GLOBAL) | bit(s))
}
fn terminate(s: Slot, old: Saved, _width: u8) {
    disable(s);
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
    let _ = overflow;
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
        assert!(!Capabilities::decode(3 | (1 << 8) | (48 << 16), 0, 0).valid());
        assert!(!Capabilities::decode(4 | (1 << 8) | (65 << 16), 0, 0).valid());
        assert!(
            Capabilities::decode(4 | (4 << 8) | (48 << 16) | (2 << 24), 0, 3 | (40 << 5))
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
            select_on_cpu(&m, Capabilities::decode(4 | (1 << 8) | (48 << 16), 0, 0), 0),
            Err(Error::NoCounter)
        ));
        m.slots[0].generation = 7;
        assert_ne!(m.slots[0].generation, 6);
    }
    #[test]
    fn generation_exhaustion_retires_before_aba() {
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
    fn counting_slot_rejects_stale_generation_and_releases_once() {
        let slot = CountingSlot::new();
        slot.generation.store(9, Ordering::Release);
        slot.state.store(CountingSlot::RUNNING, Ordering::Release);
        assert_ne!(slot.generation.load(Ordering::Acquire), 8);
        assert!(
            slot.state
                .compare_exchange(
                    CountingSlot::RUNNING,
                    CountingSlot::STOPPING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        );
        slot.delta.store(41, Ordering::Relaxed);
        slot.state.store(CountingSlot::COMPLETED, Ordering::Release);
        assert_eq!(slot.delta.load(Ordering::Acquire), 41);
        assert!(
            slot.state
                .compare_exchange(
                    CountingSlot::COMPLETED,
                    CountingSlot::IDLE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        );
        assert!(
            slot.state
                .compare_exchange(
                    CountingSlot::COMPLETED,
                    CountingSlot::IDLE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
        );
    }
    #[test]
    fn owned_overflow_mask_excludes_foreign_bits() {
        let ours = bit(Slot::P(1));
        let foreign = bit(Slot::P(2));
        assert_eq!(owned_overflow_w1c(ours | foreign, ours), ours);
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

    #[test]
    fn cpuid_display_model_and_panther_lake_whitelist_are_exact() {
        // base family 6, extended model C, base model C => family 6/model CC.
        let eax = 6 << 8 | 0xc << 16 | 0xc << 4;
        assert_eq!(decode_display_family_model(eax), (6, 0xcc));
        assert_eq!(
            decode_display_family_model(0xf << 8 | 2 << 20 | 3 << 4),
            (17, 3)
        );
        let (family, model) = decode_display_family_model(eax);
        assert_eq!(
            if family == 6 && model == 0xcc {
                ProductClass::PantherLake
            } else {
                ProductClass::ArchitecturalOnly
            },
            ProductClass::PantherLake
        );
    }

    #[test]
    fn architectural_v4_is_the_minimum_and_counter_masks_are_bounded() {
        assert!(!Capabilities::decode(3 | (1 << 8) | (48 << 16), 0, 0).valid());
        let caps = Capabilities::decode(4 | (32 << 8) | (48 << 16), 0, 31 | (48 << 5));
        assert!(caps.valid());
        assert_eq!(supported_counter_bits(caps), 0x7fff_ffff_ffff_ffff);
    }

    #[test]
    fn fleet_model_only_activates_after_every_cpu_prepares() {
        #[derive(Clone, Copy, Eq, PartialEq, Debug)]
        enum Phase {
            Collecting,
            Active,
            Aborted,
        }
        let mut phase = Phase::Collecting;
        let expected = 4;
        let mut prepared = 0;
        for _ in 0..expected - 1 {
            prepared += 1;
        }
        assert_ne!(prepared, expected);
        assert_eq!(phase, Phase::Collecting);
        prepared += 1;
        if prepared == expected {
            phase = Phase::Active;
        }
        assert_eq!(phase, Phase::Active);
        // A failed prepare is terminal and cannot be followed by a commit.
        phase = Phase::Aborted;
        assert_ne!(phase, Phase::Active);
    }

    #[test]
    fn baseline_restore_clears_only_supported_latches() {
        let caps = Capabilities::decode(4 | (2 << 8) | (48 << 16), 0, 3 | (40 << 5));
        assert_eq!(supported_counter_bits(caps), 0x0000_0007_0000_0003);
        assert_eq!(EMPTY_BASELINE.overflow_status, 0);
    }

    #[cfg(feature = "pmu-sampling")]
    #[test]
    fn sampling_encoding_preload_and_owned_ack_are_precise() {
        assert_eq!(sampling_preload(8, 16), 240);
        assert_eq!(sampling_event(Event::Cycles, true, false), 0x51003c);
        assert_eq!(sampling_event(Event::Instructions, false, true), 0x5200c0);
        let own = bit(Slot::P(1));
        let foreign = bit(Slot::P(2));
        assert_eq!(owned_overflow_w1c(own, own), own, "owned latch is W1C");
        assert_eq!(
            owned_overflow_w1c(foreign, own),
            0,
            "foreign latch is preserved"
        );
        assert_eq!(
            owned_overflow_w1c(own | foreign, own),
            own,
            "mixed latch is exact"
        );
        assert_eq!(
            (0x1234u32 & !0xff) | 0xef,
            0x12ef,
            "LVT restores non-vector bits exactly"
        );
    }

    #[cfg(feature = "pmu-sampling")]
    #[test]
    fn sampling_lvt_uses_vector_two_nmi_delivery() {
        let active = sampling_active_lvt(LVT_MASKED | 0x41 | (0b111 << 8));
        assert_eq!(active & LVT_MASKED, 0);
        assert_eq!(active & LVT_DELIVERY_MODE, LVT_DELIVERY_NMI);
        assert_eq!(active & 0xff, 0, "NMI delivery has no programmable vector");
    }

    #[cfg(feature = "pmu-sampling")]
    #[test]
    fn nmi_slot_rejects_stale_generation_and_nested_owner() {
        let slot = NmiPmiSlot::new();
        slot.arm(Slot::P(0), 9, 7, 4096, 48, 0);
        assert!(!slot.disarm_generation(8));
        assert_eq!(slot.state.load(Ordering::Acquire), NmiPmiSlot::ARMED);
        assert!(slot.disarm_generation(9));
        assert_eq!(slot.state.load(Ordering::Acquire), NmiPmiSlot::IDLE);
    }

    #[cfg(feature = "pmu-sampling")]
    #[test]
    fn sampling_lvt_baseline_owner_and_unmask_rules() {
        assert!(lvt_is_safe_sampling_baseline(LVT_MASKED | 0x41));
        assert!(!lvt_is_safe_sampling_baseline(0x41));
        let armed = sampling_active_lvt(LVT_MASKED | 0x41);
        assert_eq!(armed & LVT_MASKED, 0, "activation explicitly unmasks PMI");
        let nmi_baseline = LVT_MASKED | LVT_DELIVERY_MODE | 0x41;
        assert_eq!(
            sampling_active_lvt(nmi_baseline) & LVT_DELIVERY_MODE,
            LVT_DELIVERY_NMI,
            "activation uses APIC NMI delivery"
        );
        assert_eq!(sampling_active_lvt(nmi_baseline) & 0xff, 0);
        let mut manager = Manager::new();
        manager.slots[2].owned = true;
        manager.slots[2].sampling = true;
        assert_eq!(sampling_live(&manager), 1);
        manager.slots[3].owned = true;
        manager.slots[3].sampling = true;
        assert_eq!(sampling_live(&manager), 2);
    }

    #[cfg(feature = "pmu-sampling")]
    #[test]
    fn stray_pmi_predicate_keeps_sampler_armed() {
        let own = bit(Slot::P(0));
        let foreign = bit(Slot::P(1));
        assert_eq!(
            foreign & own,
            0,
            "stray status must not cause mask/disable/W1C"
        );
        assert_ne!(
            (foreign | own) & own,
            0,
            "only the unique owned bit authorizes disarm"
        );
    }
}
