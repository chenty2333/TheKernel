//! Low-level NMI/data-ring backend for unified perf events.
//!
//! Descriptor policy, FD lifecycle, and task attachment live in
//! [`super::perf::PerfEventFile`] / `PerfGroup`.  This module owns only the
//! preallocated sampling ring, PMU token, and NMI-safe producer machinery.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    task::Context,
};

use axcpu::trap::{NMI, register_trap_handler};
use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axsync::spin::SpinNoIrq;
use axtask::current;
use kernel_guard::NoPreemptIrqSave;
use memory_addr::PAGE_SIZE_4K;
#[cfg(test)]
use thekernel_linux_perf::encode_sample_record;
use thekernel_linux_perf::{
    AuxRecord, PERF_EVENT_IOC_DISABLE, PERF_EVENT_IOC_ENABLE, PERF_EVENT_IOC_ID,
    PERF_EVENT_IOC_MODIFY_ATTRIBUTES, PERF_EVENT_IOC_PAUSE_OUTPUT, PERF_EVENT_IOC_PERIOD,
    PERF_EVENT_IOC_REFRESH, PERF_EVENT_IOC_RESET, PERF_EVENT_IOC_SET_FILTER, PERF_FORMAT_ID,
    PERF_FORMAT_LOST, PERF_FORMAT_TOTAL_TIME_ENABLED, PERF_FORMAT_TOTAL_TIME_RUNNING,
    PERF_SAMPLE_ADDR, PERF_SAMPLE_BRANCH_STACK, PERF_SAMPLE_DATA_SRC, PerfBranchEntry,
    PerfEventAttr, PerfSampleFields, SampleRecordPlan, encode_aux_record, encode_lost_record,
    encode_sample_record_fields,
};

use super::perf_aux::{AUX_HEAD, AUX_OFFSET, AUX_SIZE, AUX_TAIL, AuxRequest, aux_mapping_offset};
use crate::{
    file::{
        FileMmapProtection, FileMmapRequest, FileMmapSharing, FixedSharedMmapRegion, IoDst, IoSrc,
        IoctlContext, Kstat, PreparedFileMmap, anon_inode_stat,
    },
    mm::{SharedAtomicU64, SharedFixedView, SharedPages},
    perf_security::authorize_sampling_rate,
};

const PAGE: usize = PAGE_SIZE_4K;

static PERF_WAITER_NOTIFICATION: [AtomicBool; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicBool::new(false) }; axconfig::plat::MAX_CPU_NUM];

/// Perf notification wakeups must not recursively acquire producer locks
/// through sched_wakeup. The scheduler calls this with migration disabled.
pub(crate) fn notifying_perf_waiters() -> bool {
    PERF_WAITER_NOTIFICATION[axhal::percpu::this_cpu_id()].load(Ordering::Acquire)
}

struct PerfWaitNotificationGuard {
    cpu: usize,
    previous: bool,
    _irq: NoPreemptIrqSave,
}

impl PerfWaitNotificationGuard {
    fn new() -> Self {
        let irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        let previous = PERF_WAITER_NOTIFICATION[cpu].swap(true, Ordering::AcqRel);
        Self { cpu, previous, _irq: irq }
    }
}

impl Drop for PerfWaitNotificationGuard {
    fn drop(&mut self) {
        PERF_WAITER_NOTIFICATION[self.cpu].store(self.previous, Ordering::Release);
    }
}

fn wake_perf_waiters(waiters: &PollSet<4>) {
    let _notification = PerfWaitNotificationGuard::new();
    waiters.wake();
}

const MIN_DATA: usize = PAGE;
const MAX_DATA: usize = 1024 * 1024;
const DATA_HEAD: usize = 1024;
const DATA_TAIL: usize = 1032;
const DATA_OFFSET: usize = 1040;
const DATA_SIZE: usize = 1048;
// `perf_event_mmap_page` is a fixed 1KiB ABI prefix.  Keep these offsets
// local to the producer so the words that userspace reads under `lock` are
// always published as one seqlock snapshot.
const META_VERSION: usize = 0;
const META_COMPAT_VERSION: usize = 4;
const META_LOCK: usize = 8;
const META_INDEX: usize = 12;
const META_OFFSET: usize = 16;
const META_TIME_ENABLED: usize = 24;
const META_TIME_RUNNING: usize = 32;
const META_CAPABILITIES: usize = 40;
const META_PMC_WIDTH: usize = 48;
const META_TIME_SHIFT: usize = 50;
const META_TIME_MULT: usize = 52;
const META_TIME_OFFSET: usize = 56;
const META_TIME_ZERO: usize = 64;
const META_SIZE: usize = 72;

#[derive(Clone, Copy)]
pub(crate) enum SamplingEvent {
    Cycles,
    Instructions,
    /// An Intel raw EventSel is tied to the core type observed at open.  It
    /// reaches `SamplingProgram` unchanged; migration is rejected by the
    /// PMU rather than being reinterpreted as a source-ring notification.
    Raw {
        config: u64,
        core_type: axhal::pmu::IntelCoreType,
    },
    /// A software BPF_OUTPUT descriptor uses the same mmap data-ring
    /// producer but deliberately never arms a PMU counter.
    BpfOutput,
    /// Trace, probe, and software sources publish into the same preallocated
    /// mmap ring synchronously, without arming a PMU counter.
    Source,
}

#[derive(Clone, Copy)]
pub(crate) struct SamplingConfig {
    pub id: u64,
    pub target_task_id: u64,
    pub event: SamplingEvent,
    pub period: u64,
    /// Requested samples/second. `period` remains the current hardware
    /// period and is adjusted after each observed completion.
    pub frequency: Option<u64>,
    pub sample_type: u64,
    pub count_user: bool,
    pub count_kernel: bool,
    pub disabled: bool,
    pub read_format: u64,
    /// Exact AUX is never inferred from a generic sampling event.  The
    /// syscall performed the capability admission before constructing this
    /// immutable configuration.
    pub aux: Option<AuxRequest>,
    /// Fields that must never be changed through MODIFY_ATTRIBUTES.  Keep the
    /// original target and attr prefix with the OFD so a later ioctl cannot
    /// acquire a different source or broader authority from current creds.
    pub identity: PerfOpenIdentity,
}

/// One coherent sampling-counter snapshot consumed by the unified perf FD.
/// The mmap producer state remains private to this backend; group reads only
/// need the four Linux read-format values.
#[derive(Clone, Copy)]
pub(crate) struct SamplingCount {
    pub value: u64,
    pub enabled: u64,
    pub running: u64,
    pub lost: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct PerfOpenIdentity {
    pub attr: thekernel_linux_perf::PerfEventAttr,
    pub target: thekernel_linux_perf::PerfOpenTarget,
    pub authority: crate::perf_security::PerfAuthority,
}

#[derive(Clone)]
enum SamplingFilter {
    AcceptAll,
    RejectAll,
    CommonPid(u32),
}

impl SamplingFilter {
    fn parse_source(bytes: &[u8]) -> AxResult<Self> {
        match bytes {
            b"1" => Ok(Self::AcceptAll),
            b"0" => Ok(Self::RejectAll),
            _ => {
                const PREFIX: &[u8] = b"common_pid == ";
                let number = bytes.strip_prefix(PREFIX).ok_or(AxError::InvalidInput)?;
                let mut value = 0u32;
                if number.is_empty() {
                    return Err(AxError::InvalidInput);
                }
                for byte in number {
                    if !byte.is_ascii_digit() {
                        return Err(AxError::InvalidInput);
                    }
                    value = value
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(u32::from(byte - b'0')))
                        .ok_or(AxError::InvalidInput)?;
                }
                Ok(Self::CommonPid(value))
            }
        }
    }

    const fn matches_pid(&self, pid: u32) -> bool {
        match self {
            Self::AcceptAll => true,
            Self::RejectAll => false,
            Self::CommonPid(expected) => *expected == pid,
        }
    }
}

/// Linux perf address-filter spelling for a numeric kernel range.  File based
/// filters (`offset/size@path`) require VMA relocation tracking and are not
/// admitted by this bounded Panther Lake slice; accepting them here would
/// program a stale virtual range after exec/mmap.
fn parse_pt_address_filter(
    bytes: &[u8],
) -> AxResult<Option<axhal::perf_precise_aux::PtAddressFilter>> {
    if bytes.is_empty() || bytes == b"0" {
        return Ok(None);
    }
    let text = core::str::from_utf8(bytes).map_err(|_| AxError::InvalidInput)?;
    let mut words = text.split_ascii_whitespace();
    if words.next() != Some("filter") {
        return Err(AxError::InvalidInput);
    }
    let range = words.next().ok_or(AxError::InvalidInput)?;
    if words.next().is_some() || range.contains('@') {
        return Err(AxError::OperationNotSupported);
    }
    let (start, bytes) = range.split_once('/').ok_or(AxError::InvalidInput)?;
    let parse = |number: &str| {
        number
            .strip_prefix("0x")
            .or_else(|| number.strip_prefix("0X"))
            .map_or_else(|| number.parse::<u64>(), |hex| u64::from_str_radix(hex, 16))
            .map_err(|_| AxError::InvalidInput)
    };
    let start = parse(start)?;
    let size = parse(bytes)?;
    let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
    let filter = axhal::perf_precise_aux::PtAddressFilter { start, end };
    filter.validate().map_err(|_| AxError::InvalidInput)?;
    Ok(Some(filter))
}

struct Ring {
    region: FixedSharedMmapRegion,
    pages: Arc<SharedPages>,
    view: SharedFixedView,
    head: SharedAtomicU64,
    tail: SharedAtomicU64,
    sequence: crate::mm::SharedAtomicU32,
    data_size: usize,
    producer_head: u64,
    lost: u64,
    records_since_wakeup: u32,
}

fn publication_should_wake(
    wakeup: thekernel_linux_perf::Wakeup,
    ring: &mut Ring,
    published: bool,
) -> bool {
    if !published {
        return false;
    }
    if let thekernel_linux_perf::Wakeup::Watermark(threshold) = wakeup {
        let used = producer_window(ring.producer_head, ring.tail.load_acquire(), ring.data_size)
            .unwrap_or(ring.data_size as u64);
        used >= u64::from(threshold.max(1))
    } else {
        let thekernel_linux_perf::Wakeup::Events(threshold) = wakeup else {
            unreachable!()
        };
        ring.records_since_wakeup = ring.records_since_wakeup.saturating_add(1);
        if ring.records_since_wakeup < threshold.max(1) {
            false
        } else {
            ring.records_since_wakeup = 0;
            true
        }
    }
}

/// Separate, data-record-independent AUX mapping.  The ToPA table is pinned
/// with it so hardware can never retain a freed output page.
struct AuxRing {
    region: FixedSharedMmapRegion,
    pages: Arc<SharedPages>,
    topa: Arc<SharedPages>,
    bts_ds: Option<Arc<SharedPages>>,
    backend: super::perf_aux::AuxBackend,
    data_size: usize,
}

struct PebsRing {
    data: Arc<SharedPages>,
    ds: Arc<SharedPages>,
}

/// Fully admitted, but not yet published, MODIFY_ATTRIBUTES state.  Every
/// fallible operation (including private exact-capture backing allocation)
/// completes before the group owner is stopped.
pub(crate) struct PreparedSamplingModify {
    attr: PerfEventAttr,
    sample: SampleRecordPlan,
    disabled: bool,
    pebs_upgrade: Option<PebsRing>,
}

struct SamplingState {
    enabled: bool,
    closed: bool,
    failed: bool,
    value: u64,
    enabled_total: u64,
    running_total: u64,
    enabled_since: u64,
    running_since: Option<u64>,
    period: u64,
    frequency: Option<u64>,
    sample_type: u64,
    wakeup: thekernel_linux_perf::Wakeup,
    last_frequency_adjust: u64,
    /// Units accumulated since the last emitted source sample. Source units
    /// are occurrences for trace/probe/software events and nanoseconds for
    /// CPU_CLOCK/TASK_CLOCK.
    source_progress: u64,
    /// Units observed in the current source frequency-control window.
    source_frequency_observed: u64,
    /// Hard ATTR_FREQ publication throttle, independent of the slowly
    /// adjusted source period.
    last_source_sample: u64,
    refresh_budget: u64,
    output_paused: bool,
    ring: Option<Ring>,
    aux: Option<AuxRing>,
    pebs: Option<PebsRing>,
    data_charge: alloc::sync::Weak<crate::perf_security::PerfMlockReservation>,
    aux_charge: alloc::sync::Weak<crate::perf_security::PerfMlockReservation>,
    /// Replaced only under this lock after SET_FILTER has completed bounded
    /// usercopy and source-specific parsing. The producer only reads it.
    filter: Option<Arc<SamplingFilter>>,
    scratch: [u8; 8192],
    /// Intel PT address filtering is a separate perf address-filter ABI,
    /// never an interpretation of attr.config1/config2.  This first product
    /// slice programs one hardware `filter start/size` range per event.
    pt_filter: Option<axhal::perf_precise_aux::PtAddressFilter>,
}

fn write_metadata(view: &SharedFixedView, offset: usize, bytes: &[u8]) -> AxResult {
    // The metadata prefix is not part of the producer/consumer data ring.
    // Writers serialize it with the perf mmap seqlock below, exactly as ABI
    // readers retry around an odd `lock` value.
    unsafe { view.write_wrapped(0, PAGE, offset, bytes) }
}

fn initialize_metadata_page(
    view: &SharedFixedView,
    sequence: &crate::mm::SharedAtomicU32,
    data_size: usize,
) -> AxResult {
    sequence.store_release(1);
    write_metadata(view, META_VERSION, &0u32.to_ne_bytes())?;
    write_metadata(view, META_COMPAT_VERSION, &0u32.to_ne_bytes())?;
    // index==0 / capability==0 deliberately force userspace through
    // read(2): no event receives CR4.PCE or RDPMC access.
    write_metadata(view, META_INDEX, &0u32.to_ne_bytes())?;
    write_metadata(view, META_OFFSET, &0i64.to_ne_bytes())?;
    write_metadata(view, META_TIME_ENABLED, &0u64.to_ne_bytes())?;
    write_metadata(view, META_TIME_RUNNING, &0u64.to_ne_bytes())?;
    write_metadata(view, META_CAPABILITIES, &0u64.to_ne_bytes())?;
    write_metadata(view, META_PMC_WIDTH, &0u16.to_ne_bytes())?;
    write_metadata(view, META_TIME_SHIFT, &0u16.to_ne_bytes())?;
    write_metadata(view, META_TIME_MULT, &1u32.to_ne_bytes())?;
    write_metadata(view, META_TIME_OFFSET, &0u64.to_ne_bytes())?;
    write_metadata(view, META_TIME_ZERO, &0u64.to_ne_bytes())?;
    write_metadata(
        view,
        META_SIZE,
        &(core::mem::size_of::<thekernel_linux_perf::PerfEventMmapPage>() as u32).to_ne_bytes(),
    )?;
    view.atomic_u64(DATA_OFFSET)?.store_release(PAGE as u64);
    view.atomic_u64(DATA_SIZE)?.store_release(data_size as u64);
    sequence.store_release(2);
    Ok(())
}

fn sync_metadata(ring: &Ring, state: &SamplingState, now: u64) -> AxResult {
    let begin = ring.sequence.load_acquire().wrapping_add(1) | 1;
    ring.sequence.store_release(begin);
    let enabled = state.enabled_total.saturating_add(if state.enabled {
        now.saturating_sub(state.enabled_since)
    } else {
        0
    });
    let running = state.running_total.saturating_add(
        state
            .running_since
            .map_or(0, |since| now.saturating_sub(since)),
    );
    write_metadata(&ring.view, META_TIME_ENABLED, &enabled.to_ne_bytes())?;
    write_metadata(&ring.view, META_TIME_RUNNING, &running.to_ne_bytes())?;
    ring.sequence.store_release(begin.wrapping_add(1));
    Ok(())
}

/// An OFD-owned sampling event.  The producer state is IRQ-safe and has no
/// allocation path after mmap has installed its fixed backing.
pub(crate) struct PerfSampleBackend {
    config: SamplingConfig,
    state: SpinNoIrq<SamplingState>,
    waiters: PollSet<4>,
    retire_next: AtomicPtr<PerfSampleBackend>,
    retire_queued: SpinNoIrq<bool>,
    /// Optional destination selected by PERF_EVENT_IOC_SET_OUTPUT.  The
    /// registry validates context and rejects cycles before publishing this
    /// weak edge; the destination retains ownership of its mmap pages.
    output: SpinNoIrq<Option<alloc::sync::Weak<PerfSampleBackend>>>,
    /// Weak back-edge to the descriptor that owns PERF_EVENT BPF attachment.
    /// Sampling custody keeps this backend alive, never the descriptor, so a
    /// close cannot form an Arc cycle or be resurrected by a deferred PMI.
    #[cfg(feature = "bpf")]
    owner: SpinNoIrq<alloc::sync::Weak<super::perf::PerfEventFile>>,
}

// `CUSTODY` keeps `Arc<PerfSampleBackend>` values in per-CPU spin-locked
// slots.  The weak descriptor edge makes the automatic Send/Sync solver
// recursive (`PerfEventFile` owns a backend which weakly names its owner).
// Every mutable backend field is protected by `SpinNoIrq` or an atomic, and
// the only raw-pointer paths transfer an owned Arc explicitly, so breaking
// that type-level cycle is sound.
unsafe impl Send for PerfSampleBackend {}
unsafe impl Sync for PerfSampleBackend {}

struct SamplingRetireQueue {
    incoming: AtomicPtr<PerfSampleBackend>,
    pending: AtomicPtr<PerfSampleBackend>,
    draining: AtomicBool,
}

impl SamplingRetireQueue {
    const fn new() -> Self {
        Self {
            incoming: AtomicPtr::new(ptr::null_mut()),
            pending: AtomicPtr::new(ptr::null_mut()),
            draining: AtomicBool::new(false),
        }
    }
}

static RETIRED_CUSTODY: SamplingRetireQueue = SamplingRetireQueue::new();
const RETIRE_BATCH: usize = 16;
/// Source events do not receive PMIs. Keep their feedback controller slow
/// enough to avoid oscillation; the publication-time gate remains the strict
/// samples-per-second bound.
const SOURCE_FREQUENCY_ADJUST_NS: u64 = 10_000_000;
/// Scheduler elapsed time can cross many source periods.  Bound one normal
/// publication batch so a delayed tick never turns into unbounded work.
const MAX_SOURCE_SAMPLE_BATCH: usize = 32;
/// Normal context drains this bounded slice of the CPU-local NMI SPSC queue
/// per deferred boundary. It is deliberately much larger than the number of
/// simultaneously armed PMCs so high-frequency completions do not wait a tick
/// per counter.
const NMI_COMPLETION_DRAIN_BATCH: usize = 64;

/// Fields whose values have no truthful ordinary-PMI fallback.  In
/// particular, never turn an absent or malformed PEBS record into a sample
/// carrying the interrupted RIP and zero address/data-source words.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExactCaptureRequirements {
    pebs: bool,
    lbr: bool,
}

const fn exact_capture_requirements(
    sample_type: u64,
    aux: Option<AuxRequest>,
) -> ExactCaptureRequirements {
    ExactCaptureRequirements {
        pebs: sample_type & (PERF_SAMPLE_ADDR | PERF_SAMPLE_DATA_SRC) != 0
            || matches!(aux, Some(request) if request.precise_ip == 1),
        lbr: sample_type & PERF_SAMPLE_BRANCH_STACK != 0,
    }
}

/// PEBS absence and decode failure are both loss, not permission to publish
/// an inexact PMI record.  Keep this policy pure so the NMI-drain model can
/// exercise both paths without fabricating a hardware descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExactCompletionAction {
    Publish,
    DropAsLost,
}

const fn exact_completion_action(
    requirements: ExactCaptureRequirements,
    pebs_present: bool,
    pebs_decoded: bool,
    lbr_captured: bool,
) -> ExactCompletionAction {
    if (requirements.pebs && (!pebs_present || !pebs_decoded))
        || (requirements.lbr && !lbr_captured)
    {
        ExactCompletionAction::DropAsLost
    } else {
        ExactCompletionAction::Publish
    }
}

const fn exact_drop_lost(completion_lost: u64) -> u64 {
    completion_lost.saturating_add(1)
}

#[derive(Clone, Copy, Default)]
struct SourceSampleDue {
    records: usize,
    lost: u64,
}

struct CpuCustody {
    event: Arc<PerfSampleBackend>,
    // Keep the event alive even after an IRQ has stopped the counter.  In
    // particular, never let an interrupt drop the final Ring/FixedView Arc.
    token: Option<axhal::pmu::SamplingToken>,
    lbr: Option<axhal::perf_precise_aux::LbrToken>,
    pebs_armed: bool,
    cookie: u64,
    group_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReconcileAction {
    Arm,
    Stop,
    Keep,
}

#[derive(Clone, Copy)]
enum StopSettlement {
    Normal,
    RunningOnly,
}

const fn reconcile_action(live: bool, matching_owner: bool, token_armed: bool) -> ReconcileAction {
    match (live, matching_owner, token_armed) {
        (true, _, false) => ReconcileAction::Arm,
        (false, true, true) => ReconcileAction::Stop,
        _ => ReconcileAction::Keep,
    }
}

// PMU sampling may occupy every architectural programmable counter.  Keep
// that ownership in a fixed per-CPU table: scheduler, IPI and NMI-adjacent
// paths never grow a Vec merely because a second sampler is admitted.
const MAX_SAMPLING_CUSTODIES: usize = axhal::pmu::MAX_COUNTING_GROUP;
static CUSTODY: [SpinNoIrq<[Option<CpuCustody>; MAX_SAMPLING_CUSTODIES]>;
    axconfig::plat::MAX_CPU_NUM] = [const { SpinNoIrq::new([const { None }; MAX_SAMPLING_CUSTODIES]) };
    axconfig::plat::MAX_CPU_NUM];

fn defer_custody_retire(event: Arc<PerfSampleBackend>) {
    let node = Arc::into_raw(event) as *mut PerfSampleBackend;
    // SAFETY: `node` owns the strong reference just transferred from `event`.
    // A true queued bit is protected by this same lock and guarantees that a
    // distinct queue-owned strong reference remains live until the consumer
    // acquires the lock to clear it.
    let mut queued = unsafe { (*node).retire_queued.lock() };
    if *queued {
        // Release the duplicate while the queue owner is still protected by
        // `retire_queued`.  This decrement therefore cannot run the final
        // destructor in the scheduler's IRQ-disabled leave path.
        unsafe { Arc::decrement_strong_count(node) };
        drop(queued);
        return;
    }
    *queued = true;
    drop(queued);
    loop {
        let head = RETIRED_CUSTODY.incoming.load(Ordering::Acquire);
        // SAFETY: this raw Arc is uniquely owned by the queue publication.
        unsafe { (*node).retire_next.store(head, Ordering::Relaxed) };
        if RETIRED_CUSTODY
            .incoming
            .compare_exchange_weak(head, node, Ordering::Release, Ordering::Acquire)
            .is_ok()
        {
            crate::deferred_work::kick_perf_retire_worker();
            return;
        }
    }
}

fn reverse_retire_list(mut head: *mut PerfSampleBackend) -> *mut PerfSampleBackend {
    let mut reversed = ptr::null_mut();
    while !head.is_null() {
        // SAFETY: nodes are detached from incoming by the sole consumer.
        let next = unsafe { (*head).retire_next.load(Ordering::Relaxed) };
        unsafe { (*head).retire_next.store(reversed, Ordering::Relaxed) };
        reversed = head;
        head = next;
    }
    reversed
}

pub(crate) fn has_deferred_custody_retire_work() -> bool {
    !RETIRED_CUSTODY.incoming.load(Ordering::Acquire).is_null()
        || !RETIRED_CUSTODY.pending.load(Ordering::Acquire).is_null()
}

pub(crate) fn drain_deferred_custody_retire_work() {
    if RETIRED_CUSTODY
        .draining
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let mut list = RETIRED_CUSTODY
        .pending
        .swap(ptr::null_mut(), Ordering::AcqRel);
    if list.is_null() {
        list = reverse_retire_list(
            RETIRED_CUSTODY
                .incoming
                .swap(ptr::null_mut(), Ordering::AcqRel),
        );
    }
    let mut count = 0;
    while !list.is_null() && count < RETIRE_BATCH {
        let node = list;
        // SAFETY: `node` is exclusively held by this consumer list.
        list = unsafe { (*node).retire_next.load(Ordering::Relaxed) };
        unsafe {
            (*node)
                .retire_next
                .store(ptr::null_mut(), Ordering::Relaxed)
        };
        let event = unsafe { Arc::from_raw(node) };
        {
            let mut queued = event.retire_queued.lock();
            *queued = false;
        }
        drop(event);
        count += 1;
    }
    if !list.is_null() {
        RETIRED_CUSTODY.pending.store(list, Ordering::Release);
    }
    RETIRED_CUSTODY.draining.store(false, Ordering::Release);
}

impl PerfSampleBackend {
    pub(crate) const fn count_domains(&self) -> (bool, bool) {
        (self.config.count_user, self.config.count_kernel)
    }

    const MAX_FILTER_BYTES: usize = 4096;

    fn copy_filter(context: &IoctlContext, arg: usize) -> AxResult<Vec<u8>> {
        if arg == 0 {
            return Err(AxError::BadAddress);
        }
        let mut bytes = Vec::new();
        for offset in 0..Self::MAX_FILTER_BYTES {
            let address = arg.checked_add(offset).ok_or(AxError::BadAddress)?;
            let byte = context
                .user_memory()
                .read_value(address as *const u8)
                .map_err(crate::mm::map_usercopy_error)?;
            if byte == 0 {
                return Ok(bytes);
            }
            bytes.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            bytes.push(byte);
        }
        Err(AxError::ArgumentListTooLong)
    }

    pub(crate) fn prepare_modify_attributes(
        &self,
        context: &IoctlContext,
        arg: usize,
    ) -> AxResult<PreparedSamplingModify> {
        let memory = context.user_memory();
        let size = crate::syscall::read_attr_size(memory, arg as *const _)?;
        let copy_len = crate::syscall::attr_copy_len(size)?;
        let attr = crate::syscall::read_attr(
            memory,
            arg as *const _,
            copy_len.min(thekernel_linux_perf::PERF_ATTR_SIZE_VER9 as usize),
        )?;
        crate::syscall::read_attr_tail(memory, arg as *const _, copy_len)?;

        let identity = self.config.identity;
        // Re-run the same complete v0-v9 planner used at open against the
        // frozen target.  MODIFY_ATTRIBUTES never consults current
        // credentials, target selection, or an FD supplied by the caller.
        let (_, plan) = crate::syscall::perf_plan(attr, size, &[], identity.target)?;
        let (pid, cpu) = match identity.target.target {
            thekernel_linux_perf::PerfTarget::Task { pid, cpu } => (pid, cpu),
            thekernel_linux_perf::PerfTarget::Cpu { cpu } => (-1, cpu),
            thekernel_linux_perf::PerfTarget::Cgroup { cpu, .. } => (-1, cpu),
        };
        // Reapply the opening credential snapshot to replacement attributes.
        // In particular, an unprivileged descriptor must not gain LBR/PEBS
        // capture merely because a different thread issues this ioctl later.
        crate::perf_security::authorize_open(
            identity.authority,
            &attr,
            pid,
            cpu,
            identity.target.open_flags,
        )?;
        // Preserve the open-time frequency ceiling for mutable sampling
        // attributes.  This is deliberately after all usercopy and planner
        // validation, but before stopping a live counter or touching the
        // runtime state/ring metadata, so a rejected request is inert.
        authorize_sampling_rate(&attr)?;
        let old = identity.attr;
        const MUTABLE_FLAGS: u64 = thekernel_linux_perf::ATTR_DISABLED
            | thekernel_linux_perf::ATTR_FREQ
            | thekernel_linux_perf::ATTR_WATERMARK;
        if attr.event_type != old.event_type
            || attr.config != old.config
            || attr.config1 != old.config1
            || attr.config2 != old.config2
            || attr.config3 != old.config3
            || attr.config4 != old.config4
            || attr.read_format != old.read_format
            || (attr.flags & !MUTABLE_FLAGS) != (old.flags & !MUTABLE_FLAGS)
        {
            return Err(AxError::OperationNotPermitted);
        }
        let sample = plan.sample.ok_or(AxError::InvalidInput)?;
        if !crate::syscall::sampling_fields_supported_by_backend(&attr, self.config.event) {
            return Err(AxError::OperationNotSupported);
        }
        let candidate_aux = AuxRequest::from_attr(&attr, size);
        if let Some(candidate) = candidate_aux {
            candidate.admit()?;
        }
        let aux_compatible = match self.config.aux {
            Some(configured) => configured.compatible_with_modify(candidate_aux),
            // A branch-stack-only request has no AUX transport to replace;
            // it is admitted above and receives its own LBR custody.
            None => {
                candidate_aux.is_none_or(|candidate| !candidate.aux && candidate.precise_ip == 0)
            }
        };
        if !aux_compatible {
            return Err(AxError::OperationNotPermitted);
        }
        let needs_pebs = exact_capture_requirements(sample.sample_type, self.config.aux).pebs;
        let pebs_upgrade = if needs_pebs && self.state.lock().pebs.is_none() {
            Some(PebsRing {
                data: Arc::try_new(SharedPages::new_fixed(
                    PAGE,
                    axhal::paging::PageSize::Size4K,
                )?)
                .map_err(|_| AxError::NoMemory)?,
                ds: Arc::try_new(SharedPages::new_fixed(
                    PAGE,
                    axhal::paging::PageSize::Size4K,
                )?)
                .map_err(|_| AxError::NoMemory)?,
            })
        } else {
            None
        };
        Ok(PreparedSamplingModify {
            attr,
            sample,
            disabled: plan.disabled,
            pebs_upgrade,
        })
    }

    pub(crate) fn commit_modify_attributes(
        &self,
        prepared: PreparedSamplingModify,
    ) -> AxResult<()> {
        let PreparedSamplingModify {
            attr,
            sample,
            disabled,
            pebs_upgrade,
        } = prepared;
        if self.live() {
            self.stop_if_current();
        }
        let now = axhal::time::monotonic_time_nanos();
        let mut state = self.state.lock();
        if state.pebs.is_none() {
            state.pebs = pebs_upgrade;
        }
        let frequency =
            (attr.flags & thekernel_linux_perf::ATTR_FREQ != 0).then_some(attr.sample_period);
        // The union carries Hz in frequency mode.  Preserve the last real
        // counter period across a mode switch so the first rearm cannot turn
        // (for example) 99Hz into a 99-event PMI storm.
        if frequency.is_none() {
            state.period = sample.period;
        } else if matches!(self.config.event, SamplingEvent::Source) {
            state.period = 1;
        }
        state.frequency = frequency;
        state.sample_type = sample.sample_type;
        state.wakeup = if attr.flags & thekernel_linux_perf::ATTR_WATERMARK != 0 {
            thekernel_linux_perf::Wakeup::Watermark(attr.wakeup_events)
        } else {
            thekernel_linux_perf::Wakeup::Events(attr.wakeup_events)
        };
        state.last_frequency_adjust = now;
        state.source_frequency_observed = 0;
        state.last_source_sample = now;
        let enabled = !disabled;
        if enabled != state.enabled {
            if state.enabled {
                state.enabled_total = state
                    .enabled_total
                    .saturating_add(now.saturating_sub(state.enabled_since));
            } else {
                state.enabled_since = now;
            }
            state.enabled = enabled;
        }
        if let Some(ring) = state.ring.as_ref() {
            sync_metadata(ring, &state, now)?;
        }
        Ok(())
    }

    fn modify_attributes(&self, context: &IoctlContext, arg: usize) -> AxResult<()> {
        self.commit_modify_attributes(self.prepare_modify_attributes(context, arg)?)
    }

    /// Snapshot the immutable sampling identity used by standard lifecycle
    /// records. The caller supplies the real task identity at the hook; time
    /// and CPU are captured at publication so SAMPLE_ID_ALL is contemporaneous.
    pub(crate) fn metadata_sample_id(&self, pid: u32, tid: u32) -> crate::perf_records::SampleId {
        let sample_type = self.state.lock().sample_type;
        crate::perf_records::SampleId {
            sample_type,
            pid,
            tid,
            time: axhal::time::monotonic_time_nanos(),
            id: self.config.id,
            stream_id: self.config.id,
            cpu: axhal::percpu::this_cpu_id() as u32,
        }
    }
    pub(crate) fn try_new(config: SamplingConfig) -> AxResult<Arc<Self>> {
        let now = axhal::time::monotonic_time_nanos();
        let wakeup = if config.identity.attr.flags & thekernel_linux_perf::ATTR_WATERMARK != 0 {
            thekernel_linux_perf::Wakeup::Watermark(config.identity.attr.wakeup_events)
        } else {
            thekernel_linux_perf::Wakeup::Events(config.identity.attr.wakeup_events)
        };
        Arc::try_new(Self {
            state: SpinNoIrq::new(SamplingState {
                enabled: !config.disabled,
                closed: false,
                failed: false,
                value: 0,
                enabled_total: 0,
                running_total: 0,
                enabled_since: now,
                running_since: None,
                // Event sources have no event-rate estimate at open. Begin
                // ATTR_FREQ at one occurrence and let the strict time gate
                // plus bounded feedback expand it; a hardware-style 100k
                // preload would otherwise starve sparse trace/probe sources.
                period: if config.frequency.is_some()
                    && matches!(config.event, SamplingEvent::Source)
                {
                    1
                } else {
                    config.period
                },
                frequency: config.frequency,
                sample_type: config.sample_type,
                wakeup,
                last_frequency_adjust: now,
                source_progress: 0,
                source_frequency_observed: 0,
                last_source_sample: now,
                refresh_budget: 0,
                output_paused: false,
                ring: None,
                aux: None,
                data_charge: alloc::sync::Weak::new(),
                aux_charge: alloc::sync::Weak::new(),
                pebs: None,
                filter: None,
                scratch: [0; 8192],
                pt_filter: None,
            }),
            config,
            waiters: PollSet::new(),
            retire_next: AtomicPtr::new(ptr::null_mut()),
            retire_queued: SpinNoIrq::new(false),
            output: SpinNoIrq::new(None),
            #[cfg(feature = "bpf")]
            owner: SpinNoIrq::new(alloc::sync::Weak::new()),
        })
        .map_err(|_| AxError::NoMemory)
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn bind_owner(&self, owner: &Arc<super::perf::PerfEventFile>) {
        *self.owner.lock() = Arc::downgrade(owner);
    }

    #[cfg(feature = "bpf")]
    fn run_completion_bpf(&self, detail: u64) {
        if let Some(owner) = self.owner.lock().upgrade() {
            owner.run_attached_bpf(4, detail);
        }
    }

    pub(crate) fn try_new_bpf_output(
        id: u64,
        target_task_id: u64,
        disabled: bool,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(SamplingConfig {
            id,
            target_task_id,
            event: SamplingEvent::BpfOutput,
            period: 0,
            frequency: None,
            sample_type: thekernel_linux_perf::PERF_SAMPLE_RAW,
            count_user: true,
            count_kernel: true,
            disabled,
            read_format: 0,
            aux: None,
            identity: PerfOpenIdentity {
                attr: thekernel_linux_perf::PerfEventAttr::default(),
                target: thekernel_linux_perf::PerfOpenTarget {
                    target: thekernel_linux_perf::PerfTarget::Task { pid: 0, cpu: -1 },
                    group_fd: -1,
                    output_fd: -1,
                    open_flags: 0,
                },
                authority: crate::perf_security::PerfAuthority::Restricted,
            },
        })
    }

    /// Create the sampling half of an inherited descriptor.  A child starts
    /// with the parent's current sampling policy but never aliases AUX pages,
    /// PMU custody, counters, or filter-output ownership.  It does route
    /// data records through a weak parent destination: inherited children do
    /// not own FDs/rings of their own, while parent close naturally makes the
    /// destination disappear rather than retaining a stale mmap.
    pub(crate) fn fork_clone(
        self: &Arc<Self>,
        id: u64,
        target_task_id: u64,
    ) -> AxResult<Arc<Self>> {
        let (enabled, period, frequency, sample_type, wakeup, filter, pt_filter) = {
            let state = self.state.lock();
            (
                state.enabled && !state.closed && !state.failed,
                state.period,
                state.frequency,
                state.sample_type,
                state.wakeup,
                state.filter.clone(),
                state.pt_filter,
            )
        };
        let mut config = self.config;
        config.id = id;
        config.target_task_id = target_task_id;
        config.disabled = !enabled;
        let child = Self::try_new(config)?;
        let mut state = child.state.lock();
        state.period = period;
        state.frequency = frequency;
        state.sample_type = sample_type;
        state.wakeup = wakeup;
        state.filter = filter;
        state.pt_filter = pt_filter;
        drop(state);
        let _routing = super::perf::OUTPUT_ROUTING_LOCK.lock();
        *child.output.lock() = Some(Arc::downgrade(self));
        Ok(child)
    }

    /// Emit a source record with its architectural instruction pointer.  The
    /// caller must pass zero when no trap/register IP exists; trace headers
    /// are payload, never a substitute for a program counter.
    pub(crate) fn emit_raw_record_at(&self, ip: u64, user: bool, data: &[u8], timestamp: u64) -> AxResult<()> {
        if (user && !self.config.count_user) || (!user && !self.config.count_kernel) {
            return Ok(());
        }
        let output = self.output_target();
        let mut state = self.state.lock();
        if !state.enabled || state.closed || state.failed || state.output_paused {
            return Ok(());
        }
        let (sample_type, period) = (state.sample_type, state.period);
        let now = timestamp;
        let cpu = axhal::percpu::this_cpu_id() as u32;
        let pid = current().id().as_u64() as u32;
        let size = encode_sample_record_fields(
            &mut state.scratch,
            sample_type,
            PerfSampleFields {
                identifier: self.config.id,
                ip,
                pid,
                tid: pid,
                time: now,
                cpu,
                period,
                id: self.config.id,
                stream_id: self.config.id,
                user,
                raw: data,
                ..PerfSampleFields::default()
            },
        )
        .ok_or(AxError::InvalidInput)?;
        let (wake, target) = if let Some(output) = output {
            let mut destination = output.state.lock();
            let wakeup = destination.wakeup;
            let ring = destination
                .ring
                .as_mut()
                .ok_or(AxError::OperationNotSupported)?;
            let result = publish_record(ring, &state.scratch[..size], self.config.id);
            let wake = publication_should_wake(wakeup, ring, result.published);
            drop(destination);
            (wake, Some(output))
        } else {
            let wakeup = state.wakeup;
            // The producer needs an immutable record while borrowing the
            // ring mutably. Keep a bounded stack copy rather than aliasing
            // two fields through the spin-lock guard.
            let scratch = state.scratch;
            let ring = state.ring.as_mut().ok_or(AxError::OperationNotSupported)?;
            let result = publish_record(ring, &scratch[..size], self.config.id);
            (
                publication_should_wake(wakeup, ring, result.published),
                None,
            )
        };
        drop(state);
        if wake {
            wake_perf_waiters(target.as_ref().map_or(&self.waiters, |target| &target.waiters));
        }
        Ok(())
    }

    pub(crate) fn emit_raw_record(&self, data: &[u8]) -> AxResult<()> {
        self.emit_raw_record_at(0, true, data, axhal::time::monotonic_time_nanos())
    }

    fn publish_encoded_data_record(&self, record: &[u8], source_id: u64) -> AxResult<()> {
        if let Some(output) = self.output_target() {
            return output.publish_encoded_data_record(record, source_id);
        }
        let mut state = self.state.lock();
        if state.output_paused {
            return Ok(());
        }
        let wakeup = state.wakeup;
        let Some(ring) = state.ring.as_mut() else {
            return Err(AxError::OperationNotSupported);
        };
        let result = publish_record(ring, record, source_id);
        let wake = publication_should_wake(wakeup, ring, result.published);
        drop(state);
        if wake {
            wake_perf_waiters(&self.waiters);
        }
        Ok(())
    }

    fn source_frequency_adjust_locked(state: &mut SamplingState, now: u64) {
        let Some(hz) = state.frequency else {
            return;
        };
        let elapsed = now.saturating_sub(state.last_frequency_adjust);
        if elapsed < SOURCE_FREQUENCY_ADJUST_NS {
            return;
        }
        // `source_frequency_observed` is either events or elapsed ns.  This
        // expression therefore converges to events/sample for occurrence
        // sources and to ns/sample for clock sources.
        let desired = ((u128::from(state.source_frequency_observed) * 1_000_000_000u128)
            / u128::from(hz.max(1))
            / u128::from(elapsed.max(1)))
        .max(1)
        .min(u128::from(u64::MAX)) as u64;
        let lower = state.period.saturating_div(2).max(1);
        let upper = state.period.saturating_mul(2).max(1);
        state.period = desired.clamp(lower, upper);
        state.source_frequency_observed = 0;
        state.last_frequency_adjust = now;
    }

    /// Account source units and decide whether this boundary earns one
    /// sample. The data ring is deliberately not touched while holding this
    /// lock, and all counters advance even when output is paused or unmapped.
    fn source_sample_due_locked(
        state: &mut SamplingState,
        units: u64,
        now: u64,
    ) -> SourceSampleDue {
        if !state.enabled || state.closed || state.failed || units == 0 {
            return SourceSampleDue::default();
        }
        state.value = state.value.saturating_add(units);
        state.source_progress = state.source_progress.saturating_add(units);
        state.source_frequency_observed = state.source_frequency_observed.saturating_add(units);
        Self::source_frequency_adjust_locked(state, now);
        let period = state.period.max(1);
        if state.source_progress < period {
            return SourceSampleDue::default();
        }
        let crossings = state.source_progress / period;
        state.source_progress %= period;
        if let Some(hz) = state.frequency {
            let minimum_gap = 1_000_000_000u64 / hz.max(1);
            if now.saturating_sub(state.last_source_sample) < minimum_gap {
                return SourceSampleDue::default();
            }
            state.last_source_sample = now;
        }
        if state.output_paused {
            return SourceSampleDue::default();
        }
        SourceSampleDue {
            records: (crossings as usize).min(MAX_SOURCE_SAMPLE_BATCH),
            lost: crossings.saturating_sub(MAX_SOURCE_SAMPLE_BATCH as u64),
        }
    }

    /// Source producers pass their task id here so a tracefs `common_pid ==`
    /// filter is checked before accounting or reserving a record.  PMU
    /// completion does not use trace filters.
    pub(crate) fn emit_source_raw_record_at(
        &self,
        pid: u32,
        ip: u64,
        user: bool,
        data: &[u8],
        timestamp: u64,
    ) -> AxResult<()> {
        if (user && !self.config.count_user) || (!user && !self.config.count_kernel) {
            return Ok(());
        }
        let now = timestamp;
        let due = {
            let mut state = self.state.lock();
            if !state
                .filter
                .as_ref()
                .is_none_or(|filter| filter.matches_pid(pid))
            {
                return Ok(());
            }
            Self::source_sample_due_locked(&mut state, 1, now)
        };
        for _ in 0..due.records {
            self.emit_raw_record_at(ip, user, data, timestamp)?;
        }
        self.charge_lost_records(due.lost);
        Ok(())
    }

    pub(crate) fn emit_source_raw_record(&self, pid: u32, data: &[u8]) -> AxResult<()> {
        self.emit_source_raw_record_at(pid, 0, false, data, axhal::time::monotonic_time_nanos())
    }

    /// Scheduler-time source accounting for CPU_CLOCK and TASK_CLOCK.  The
    /// caller passes the elapsed running interval at each scheduler tick, so
    /// a long-running task crosses periods while it runs rather than only on
    /// its eventual switch-out.
    pub(crate) fn account_source_time(&self, pid: u32, elapsed: u64, user: bool) -> AxResult<()> {
        let now = axhal::time::monotonic_time_nanos();
        let due = {
            let mut state = self.state.lock();
            if !state
                .filter
                .as_ref()
                .is_none_or(|filter| filter.matches_pid(pid))
            {
                return Ok(());
            }
            Self::source_sample_due_locked(&mut state, elapsed, now)
        };
        for _ in 0..due.records {
            self.emit_raw_record_at(0, user, &[], now)?;
        }
        self.charge_lost_records(due.lost);
        Ok(())
    }

    /// A normal-context producer that cannot allocate a record charges the
    /// same lost counter used by a full data ring.  This is intentionally
    /// non-fatal: lifecycle delivery must not turn a successful mmap into an
    /// unrelated syscall failure.
    pub(crate) fn charge_lost_record(&self) {
        self.charge_lost_records(1);
    }

    fn charge_lost_records(&self, records: u64) {
        if records == 0 {
            return;
        }
        if let Some(output) = self.output_target() {
            output.charge_lost_records(records);
            return;
        }
        let mut state = self.state.lock();
        if let Some(ring) = state.ring.as_mut() {
            ring.lost = ring.lost.saturating_add(records);
        }
    }

    /// Publish a pre-encoded Linux metadata record (MMAP/MMAP2, COMM,
    /// FORK/EXIT, SWITCH, READ, LOST, THROTTLE).  Lifecycle owners snapshot
    /// their task/MM data before calling this method; the ring layer then
    /// supplies the same all-or-nothing head/lost semantics as samples.
    pub(crate) fn emit_metadata_record(&self, record: &[u8]) -> AxResult<()> {
        if let Some(output) = self.output_target() {
            return output.emit_metadata_record(record);
        }
        if record.len() < thekernel_linux_perf::PERF_RECORD_HEADER_SIZE
            || record.len() > u16::MAX as usize
            || !record.len().is_multiple_of(8)
        {
            return Err(AxError::InvalidInput);
        }
        let header = thekernel_linux_perf::decode_record_header(record)
            .map_err(|_| AxError::InvalidInput)?;
        if usize::from(header.size) != record.len()
            || header.kind == thekernel_linux_perf::PERF_RECORD_SAMPLE
        {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        if state.output_paused {
            return Ok(());
        }
        let wakeup = state.wakeup;
        let ring = state.ring.as_mut().ok_or(AxError::OperationNotSupported)?;
        let published = publish_record(ring, record, self.config.id);
        let wake = publication_should_wake(wakeup, ring, published.published);
        drop(state);
        if wake {
            wake_perf_waiters(&self.waiters);
        }
        Ok(())
    }

    /// Attach this descriptor's data producer to `output`'s mmap data ring.
    /// Callers hold `OUTPUT_ROUTING_LOCK` across graph validation, both edge
    /// publications, and descriptor destination accounting.
    pub(crate) fn share_output_from_transaction(&self, output: &Arc<Self>) -> AxResult<()> {
        if core::ptr::eq(self, Arc::as_ptr(output)) {
            return Err(AxError::InvalidInput);
        }
        *self.output.lock() = Some(Arc::downgrade(output));
        Ok(())
    }

    /// The descriptor transaction owns route/accounting teardown.
    pub(crate) fn clear_output_target_transaction(&self) {
        *self.output.lock() = None;
    }

    fn output_target(&self) -> Option<Arc<Self>> {
        let _routing = super::perf::OUTPUT_ROUTING_LOCK.lock();
        self.output
            .lock()
            .as_ref()
            .and_then(alloc::sync::Weak::upgrade)
    }

    pub(crate) fn enabled(&self) -> bool {
        let state = self.state.lock();
        state.enabled && !state.closed && !state.failed
    }

    pub(crate) fn set_enabled_at(&self, enabled: bool, now: u64) {
        let mut state = self.state.lock();
        if enabled == state.enabled {
            return;
        }
        if enabled {
            state.enabled = true;
            state.enabled_since = now;
        } else {
            state.enabled_total = state
                .enabled_total
                .saturating_add(now.saturating_sub(state.enabled_since));
            state.enabled = false;
        }
        if let Some(ring) = state.ring.as_ref() {
            let _ = sync_metadata(ring, &state, now);
        }
    }

    pub(crate) fn reset_count(&self) {
        let mut state = self.state.lock();
        state.value = 0;
        state.source_progress = 0;
        state.source_frequency_observed = 0;
        state.last_frequency_adjust = axhal::time::monotonic_time_nanos();
        state.last_source_sample = state.last_frequency_adjust;
        state.failed = false;
        if let Some(ring) = state.ring.as_mut() {
            ring.lost = 0;
        }
    }

    pub(crate) const fn uses_hardware(&self) -> bool {
        !matches!(
            self.config.event,
            SamplingEvent::BpfOutput | SamplingEvent::Source
        )
    }

    pub(crate) fn lost_records(&self) -> u64 {
        self.state.lock().ring.as_ref().map_or(0, |ring| ring.lost)
    }

    /// Snapshot the values used by read(2) without performing usercopy.  A
    /// caller which owns the local placement first invokes
    /// `reconcile_local_stop`; keeping settlement separate lets one group
    /// stop all of its sampling members before taking any member snapshot.
    pub(crate) fn count_snapshot(&self) -> AxResult<SamplingCount> {
        let state = self.state.lock();
        if state.failed {
            return Err(AxError::Io);
        }
        let now = axhal::time::monotonic_time_nanos();
        if let Some(ring) = state.ring.as_ref() {
            sync_metadata(ring, &state, now)?;
        }
        Ok(SamplingCount {
            value: state.value,
            enabled: state.enabled_total.saturating_add(if state.enabled {
                now.saturating_sub(state.enabled_since)
            } else {
                0
            }),
            running: state.running_total.saturating_add(
                state
                    .running_since
                    .map_or(0, |since| now.saturating_sub(since)),
            ),
            lost: state.ring.as_ref().map_or(0, |ring| ring.lost),
        })
    }

    /// Locate this descriptor's current CPU-local PMU custody without
    /// touching hardware.  Lifecycle callers use this to send the typed
    /// PerfReconcile IPI instead of waiting for a later scheduler tick.
    #[cfg(target_os = "none")]
    pub(crate) fn active_cpu(&self) -> Option<usize> {
        CUSTODY.iter().enumerate().find_map(|(cpu, slot)| {
            let custody = slot.lock();
            custody
                .iter()
                .flatten()
                .find(|custody| core::ptr::eq(Arc::as_ptr(&custody.event), self))
                .and_then(|custody| custody.token.is_some().then_some(cpu))
        })
    }

    #[cfg(target_os = "none")]
    pub(crate) fn owns_current_cpu(&self) -> bool {
        self.active_cpu() == Some(axhal::percpu::this_cpu_id())
    }

    #[cfg(target_os = "none")]
    pub(crate) fn retained_on_current_cpu(&self) -> bool {
        CUSTODY
            .get(axhal::percpu::this_cpu_id())
            .is_some_and(|slot| {
                slot.lock()
                    .iter()
                    .flatten()
                    .any(|custody| core::ptr::eq(Arc::as_ptr(&custody.event), self))
            })
    }

    #[cfg(not(target_os = "none"))]
    pub(crate) const fn owns_current_cpu(&self) -> bool {
        false
    }

    #[cfg(not(target_os = "none"))]
    pub(crate) const fn retained_on_current_cpu(&self) -> bool {
        false
    }

    #[cfg(target_os = "none")]
    fn custody_reference(&self) -> Option<Arc<Self>> {
        CUSTODY.iter().find_map(|slot| {
            let custody = slot.lock();
            custody
                .iter()
                .flatten()
                .find(|custody| core::ptr::eq(Arc::as_ptr(&custody.event), self))
                .map(|custody| custody.event.clone())
        })
    }

    #[cfg(target_os = "none")]
    pub(crate) fn reconcile_local_stop(&self) {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if let Some(slot) = CUSTODY.get(cpu) {
            let mut custody = slot.lock();
            for custody in custody.iter_mut().flatten() {
                if core::ptr::eq(Arc::as_ptr(&custody.event), self) {
                    Self::stop_custody(custody, StopSettlement::Normal);
                }
            }
        }
    }

    /// IPI half of a group reconcile.  The group generation is carried in
    /// each fixed custody entry at arm time, so this stops precisely the
    /// sampling PMCs admitted with that group and leaves unrelated samplers
    /// alone. Removed entries transfer their Arc to the deferred retire
    /// queue; the IPI never drops mappings directly.
    #[cfg(target_os = "none")]
    pub(crate) fn reconcile_group_generation_local(group_generation: u64) {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        let Some(slot) = CUSTODY.get(cpu) else { return };
        let retired = {
            let mut table = slot.lock();
            let mut retired = [const { None }; MAX_SAMPLING_CUSTODIES];
            for (source, destination) in table.iter_mut().zip(retired.iter_mut()) {
                if source
                    .as_ref()
                    .is_some_and(|entry| entry.group_generation == group_generation)
                {
                    *destination = source.take();
                }
            }
            retired
        };
        for mut custody in retired.into_iter().flatten() {
            Self::stop_custody(&mut custody, StopSettlement::Normal);
            defer_custody_retire(custody.event);
        }
    }

    #[cfg(not(target_os = "none"))]
    pub(crate) fn reconcile_local_stop(&self) {}

    #[cfg(not(target_os = "none"))]
    pub(crate) fn reconcile_group_generation_local(_: u64) {}

    #[cfg(target_os = "none")]
    pub(crate) fn fail_closed(&self) {
        let mut state = self.state.lock();
        state.failed = true;
        if let Some(since) = state.running_since.take() {
            state.running_total = state
                .running_total
                .saturating_add(axhal::time::monotonic_time_nanos().saturating_sub(since));
        }
    }

    /// IPI half of final close/disable.  `event` carries one explicit Arc
    /// strong count from the publisher; move it to the existing deferred
    /// queue because an IPI may not run the final mapping destructor.
    #[cfg(target_os = "none")]
    pub(crate) unsafe fn reconcile_ipi_stop(event: *const Self) {
        let event_ref = unsafe { &*event };
        event_ref.reconcile_local_stop();
        // SAFETY: exactly the raw strong count transferred by the publisher.
        let custody = unsafe { Arc::from_raw(event) };
        defer_custody_retire(custody);
    }

    fn target_current(&self) -> bool {
        // Placement belongs to PerfGroup's task/CPU/cgroup scheduler context.
        // A backend-local current-task comparison would incorrectly suppress
        // system-wide and cgroup samples after the group selected them.
        true
    }

    /// Attempt to acquire one free PMC in this CPU's NMI sampling transport.
    /// A false return is a normal flexible-placement result: the caller must
    /// leave the event's running interval closed rather than pretending that
    /// an unprogrammed counter accumulated time.
    pub(crate) fn enter_current(self: &Arc<Self>, group_generation: u64) -> bool {
        if matches!(
            self.config.event,
            SamplingEvent::BpfOutput | SamplingEvent::Source
        ) {
            return false;
        }
        if !self.target_current() {
            return false;
        }
        if !self.live() {
            return false;
        }
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if cpu >= CUSTODY.len() {
            return false;
        }
        let event = match self.config.event {
            SamplingEvent::Cycles => axhal::pmu::Event::Cycles,
            SamplingEvent::Instructions => axhal::pmu::Event::Instructions,
            SamplingEvent::Raw { config, core_type } => axhal::pmu::Event::Raw {
                event_select: config,
                core_type,
            },
            SamplingEvent::BpfOutput | SamplingEvent::Source => return false,
        };
        let program = axhal::pmu::SamplingProgram {
            event,
            period: self.state.lock().period,
            count_user: self.config.count_user,
            count_kernel: self.config.count_kernel,
            cookie: self.config.id,
        };
        // Reserve a distinct fixed custody entry before programming the PMC.
        // Holding only this CPU's table lock is safe here (the PMU manager is
        // separate), and closes the arm-vs-close window without a transient
        // token that no table entry owns.
        let mut custody = CUSTODY[cpu].lock();
        let Some(entry) = custody.iter_mut().find(|entry| match entry {
            None => true,
            Some(entry) => Arc::ptr_eq(&entry.event, self) && entry.token.is_none(),
        }) else {
            return false;
        };
        let Ok(token) = axhal::pmu::sampling_arm_local(program) else {
            return false;
        };
        match entry {
            Some(entry) => {
                entry.token = Some(token);
                entry.group_generation = group_generation;
            }
            empty @ None => {
                *empty = Some(CpuCustody {
                    event: self.clone(),
                    token: Some(token),
                    lbr: None,
                    pebs_armed: false,
                    cookie: self.config.id,
                    group_generation,
                });
            }
        }
        drop(custody);
        let mut state = self.state.lock();
        if state.enabled && !state.closed && !state.failed && state.ring.is_some() {
            state.running_since = Some(axhal::time::monotonic_time_nanos());
            drop(state);
            if !Self::start_aux_local(self) {
                self.release_if_current();
                return false;
            }
            if !Self::start_exact_local(self) {
                self.release_if_current();
                return false;
            }
            return true;
        }
        drop(state);
        // A remote final_close/disable can win after arm.  The custody Arc
        // makes this stop safe here and defers its final destruction to leave.
        self.release_if_current();
        false
    }

    /// Release only this backend's placement.  The old static CPU-wide
    /// teardown let a sibling group accidentally stop whichever sampler had
    /// won the single hardware transport; exact ownership is essential once
    /// flexible sampling members rotate independently.
    pub(crate) fn stop_if_current(&self) {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if let Some(slot) = CUSTODY.get(cpu) {
            let mut custody = slot.lock();
            for custody in custody.iter_mut().flatten() {
                if core::ptr::eq(Arc::as_ptr(&custody.event), self) {
                    Self::stop_custody(custody, StopSettlement::Normal);
                }
            }
        }
    }

    /// End this backend's scheduler lease and release its retained Arc before
    /// a sibling is armed.  Unlike `stop_if_current`, which intentionally
    /// keeps custody for read/close reconciliation, rotation must vacate the
    /// CPU slot or every later sibling would observe a permanently busy PMU.
    pub(crate) fn release_if_current(&self) {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        let Some(slot) = CUSTODY.get(cpu) else { return };
        let release = {
            let mut slot = slot.lock();
            slot.iter()
                .position(|entry| {
                    entry
                        .as_ref()
                        .is_some_and(|custody| core::ptr::eq(Arc::as_ptr(&custody.event), self))
                })
                .and_then(|index| slot[index].take())
        };
        if let Some(mut custody) = release {
            Self::stop_custody(&mut custody, StopSettlement::Normal);
            defer_custody_retire(custody.event);
        }
    }

    pub(crate) const fn pinned(&self) -> bool {
        self.config.identity.attr.flags & thekernel_linux_perf::ATTR_PINNED != 0
    }

    pub(crate) fn leave_current() {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        let Some(slot) = CUSTODY.get(cpu) else {
            return;
        };
        let retired = {
            let mut entries = slot.lock();
            core::mem::replace(&mut *entries, core::array::from_fn(|_| None))
        };
        for mut custody in retired.into_iter().flatten() {
            Self::stop_custody(&mut custody, StopSettlement::Normal);
            defer_custody_retire(custody.event);
        }
    }

    fn live(&self) -> bool {
        let state = self.state.lock();
        state.enabled
            && !state.closed
            && !state.failed
            && state.ring.is_some()
            && (self.config.aux.map_or(true, |request| !request.aux) || state.aux.is_some())
    }

    fn settle_stop(event: &Self, sample: axhal::pmu::StopSample) {
        let mut state = event.state.lock();
        if let Ok(caps) = axhal::pmu::capabilities() {
            let mask = caps.programmable_mask();
            let preload = mask.wrapping_add(1).wrapping_sub(state.period);
            let partial = sample.residual.wrapping_sub(preload) & mask;
            state.value = state.value.saturating_add(partial);
        } else {
            state.failed = true;
        }
        if let Some(since) = state.running_since.take() {
            state.running_total = state
                .running_total
                .saturating_add(axhal::time::monotonic_time_nanos().saturating_sub(since));
        }
        // An overflow merely means that one pending sample could not be
        // delivered before stop; it is loss, not a hardware fault.
        if sample.overflowed || sample.lost {
            if let Some(ring) = state.ring.as_mut() {
                ring.lost = ring.lost.saturating_add(1);
            }
        }
    }

    fn settle_after_pmi(event: &Self, sample: axhal::pmu::StopSample) {
        let mut state = event.state.lock();
        if let Ok(caps) = axhal::pmu::capabilities() {
            // The completed PMI period was accounted before rearm.  The stop
            // sample can only contribute post-overflow residual progress.
            state.value = state
                .value
                .saturating_add(sample.residual & caps.programmable_mask());
        } else {
            state.failed = true;
        }
        if let Some(since) = state.running_since.take() {
            state.running_total = state
                .running_total
                .saturating_add(axhal::time::monotonic_time_nanos().saturating_sub(since));
        }
        if sample.lost {
            if let Some(ring) = state.ring.as_mut() {
                ring.lost = ring.lost.saturating_add(1);
            }
        }
        if let Some(hz) = state.frequency {
            let now = axhal::time::monotonic_time_nanos();
            let elapsed = now.saturating_sub(state.last_frequency_adjust).max(1);
            state.last_frequency_adjust = now;
            let desired = 1_000_000_000u64 / hz.max(1);
            // Period is proportional to the observed inter-sample time.  Use
            // saturated integer math and a bounded one-octave correction to
            // avoid feedback oscillation after a delayed PMI.
            let scaled = state.period.saturating_mul(desired) / elapsed;
            let lower = state.period.saturating_div(2).max(1);
            let upper = state.period.saturating_mul(2).max(1);
            state.period = scaled.clamp(lower, upper);
        }
    }

    fn stop_custody(custody: &mut CpuCustody, settlement: StopSettlement) {
        if custody.pebs_armed {
            let _ = axhal::perf_precise_aux::disarm_pebs_local(custody.cookie);
            custody.pebs_armed = false;
        }
        if let Some(lbr) = custody.lbr.take() {
            let _ = axhal::perf_precise_aux::release_lbr_local(lbr);
        }
        let Some(token) = custody.token.take() else {
            return;
        };
        match axhal::pmu::sampling_stop_local(token) {
            Ok(sample) => match settlement {
                StopSettlement::Normal => Self::settle_stop(&custody.event, sample),
                StopSettlement::RunningOnly => Self::settle_after_pmi(&custody.event, sample),
            },
            Err(_) => {
                let mut state = custody.event.state.lock();
                state.failed = true;
                if let Some(since) = state.running_since.take() {
                    state.running_total = state
                        .running_total
                        .saturating_add(axhal::time::monotonic_time_nanos().saturating_sub(since));
                }
            }
        }
        Self::finish_aux_local(&custody.event);
    }

    /// Start the already-allocated PT generation at the scheduler entry
    /// boundary.  No allocation, usercopy, or ordinary ring write occurs in
    /// this path.
    fn start_aux_local(&self) -> bool {
        #[cfg(all(feature = "pmu", target_os = "none"))]
        {
            let state = self.state.lock();
            let (Some(aux), Some(request)) = (state.aux.as_ref(), self.config.aux) else {
                return true;
            };
            if !request.aux {
                return true;
            }
            if matches!(aux.backend, super::perf_aux::AuxBackend::Bts) {
                let (Ok(data), Some(ds)) = (aux.pages.paddr_at(0), aux.bts_ds.as_ref()) else {
                    return false;
                };
                let Ok(ds) = ds.paddr_at(0) else { return false };
                return axhal::perf_precise_aux::start_bts_local(
                    axhal::perf_precise_aux::BtsProgram {
                        ds_area_physical: ds.as_usize() as u64,
                        buffer: axhal::perf_precise_aux::BtsBuffer {
                            physical: data.as_usize() as u64,
                            bytes: PAGE - (PAGE % 24),
                        },
                        generation: self.config.id,
                    },
                )
                .is_ok();
            }
            let program = axhal::perf_precise_aux::PtProgram {
                topa_physical: match aux.topa.paddr_at(0) {
                    Ok(physical) => physical.as_usize() as u64,
                    Err(_) => return false,
                },
                layout: axhal::perf_precise_aux::TopaLayout {
                    data_bytes: aux.data_size,
                    page_bytes: PAGE,
                },
                mode: match request.mode {
                    super::perf_aux::AuxMode::Snapshot => {
                        axhal::perf_precise_aux::AuxMode::Snapshot
                    }
                    super::perf_aux::AuxMode::Overwrite => {
                        axhal::perf_precise_aux::AuxMode::Overwrite
                    }
                },
                generation: self.config.id,
                config: axhal::perf_precise_aux::PtConfig {
                    config: request.config,
                    trace_user: request.trace_user,
                    trace_kernel: request.trace_kernel,
                    // Address ranges are installed through the standard
                    // PERF_EVENT_IOC_SET_FILTER path and snapshotted while
                    // the event is disabled, before taking RTIT ownership.
                    address_filter: state.pt_filter,
                },
            };
            drop(state);
            return axhal::perf_precise_aux::start_pt_local(program).is_ok();
        }
        #[cfg(not(all(feature = "pmu", target_os = "none")))]
        true
    }

    /// Arms exact facilities only after the ordinary programmable counter is
    /// published in local custody.  All pages were pinned by mmap; this path
    /// allocates nothing and failure is handled by the same synchronous stop
    /// boundary as an ordinary PMU failure.
    fn start_exact_local(&self) -> bool {
        #[cfg(all(feature = "pmu", target_os = "none"))]
        {
            let exact = {
                let state = self.state.lock();
                exact_capture_requirements(state.sample_type, self.config.aux)
            };
            if !exact.pebs && !exact.lbr {
                return true;
            }
            let pebs = if exact.pebs {
                let state = self.state.lock();
                let Some(pebs) = state.pebs.as_ref() else {
                    return false;
                };
                let (Ok(data), Ok(ds)) = (pebs.data.paddr_at(0), pebs.ds.paddr_at(0)) else {
                    return false;
                };
                Some((data.as_usize() as u64, ds.as_usize() as u64))
            } else {
                None
            };
            let cpu = axhal::percpu::this_cpu_id();
            let Some(slot) = CUSTODY.get(cpu) else {
                return false;
            };
            let mut custody = slot.lock();
            let Some(custody) = custody
                .iter_mut()
                .flatten()
                .find(|custody| core::ptr::eq(Arc::as_ptr(&custody.event), self))
            else {
                return false;
            };
            if exact.lbr {
                match axhal::perf_precise_aux::acquire_lbr_local(
                    self.config.target_task_id,
                    self.config.id,
                ) {
                    Ok(token) => custody.lbr = Some(token),
                    Err(_) => return false,
                }
            }
            if let Some((data, ds)) = pebs {
                let Some(token) = custody.token.as_ref() else {
                    return false;
                };
                let buffer = axhal::perf_precise_aux::PebsBuffer {
                    physical: data,
                    bytes: PAGE,
                    ds_area_physical: ds,
                    format: axhal::perf_precise_aux::PebsFormat::PantherCoveBasic,
                };
                if axhal::perf_precise_aux::arm_pebs_local(
                    buffer,
                    self.config.id,
                    axhal::pmu::sampling_token_counter_bit(token),
                )
                .is_err()
                {
                    if let Some(lbr) = custody.lbr.take() {
                        let _ = axhal::perf_precise_aux::release_lbr_local(lbr);
                    }
                    return false;
                }
                custody.pebs_armed = true;
            }
            true
        }
        #[cfg(not(all(feature = "pmu", target_os = "none")))]
        true
    }

    /// Drain PT in scheduler/task context and publish its independent
    /// descriptor to the ordinary perf data ring.  A stale generation is an
    /// expected close/migration race and never becomes an I/O error.
    fn finish_aux_local(&self) {
        #[cfg(all(feature = "pmu", target_os = "none"))]
        {
            let mut state = self.state.lock();
            let wakeup = state.wakeup;
            let Some(request) = self.config.aux else {
                return;
            };
            let Some(backend) = state.aux.as_ref().map(|aux| aux.backend) else {
                return;
            };
            let Some(data) = state.ring.as_mut() else {
                return;
            };
            if !request.aux {
                return;
            }
            let tail = data
                .view
                .atomic_u64(AUX_TAIL)
                .map_or(0, |tail| tail.load_acquire());
            let metadata = match match backend {
                super::perf_aux::AuxBackend::IntelPt => {
                    axhal::perf_precise_aux::stop_pt_local(self.config.id, tail)
                }
                super::perf_aux::AuxBackend::Bts => {
                    axhal::perf_precise_aux::stop_bts_local(self.config.id)
                }
            } {
                Ok(metadata) => metadata,
                Err(_) => return,
            };
            let mut record = [0u8; 32];
            let flags = super::perf_aux::AuxPublication::from_completion(
                metadata.offset,
                metadata.size,
                request.mode,
                metadata.truncated,
            )
            .flags;
            let Ok(size) = encode_aux_record(
                &mut record,
                AuxRecord {
                    offset: metadata.offset,
                    size: metadata.size as u64,
                    flags,
                },
            ) else {
                return;
            };
            let published = publish_record(data, &record[..size], self.config.id);
            let wake = publication_should_wake(wakeup, data, published.published);
            data.view
                .atomic_u64(AUX_HEAD)
                .map(|head| head.store_release(metadata.offset + metadata.size as u64))
                .ok();
            if wake {
                wake_perf_waiters(&self.waiters);
            }
        }
    }

    /// Stop only this backend's sampler while retaining its custody entry
    /// until scheduler leave. Siblings on other PMCs remain armed.
    fn stop_current_with(&self, settlement: StopSettlement) {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if let Some(slot) = CUSTODY.get(cpu) {
            let mut custody = slot.lock();
            for custody in custody.iter_mut().flatten() {
                if !core::ptr::eq(Arc::as_ptr(&custody.event), self) {
                    continue;
                }
                Self::stop_custody(custody, settlement);
                break;
            }
        }
    }

    /// Reconcile hardware ownership with the target event's current state.
    /// The scheduler tick uses this to bound a remote close/disable to one tick.
    pub(crate) fn reconcile_current(self: &Arc<Self>) {
        if !self.target_current() {
            return;
        }
        let _irq = NoPreemptIrqSave::new();
        // NMI only captures a bounded raw completion. Publish/wake/rearm at
        // this normal scheduler boundary, never from vector 2.
        Self::drain_nmi_local();
        let cpu = axhal::percpu::this_cpu_id();
        let (matching_owner, token_armed, group_generation) = CUSTODY
            .get(cpu)
            .and_then(|slot| {
                let custody = slot.lock();
                custody.iter().flatten().find_map(|c| {
                    Arc::ptr_eq(&c.event, self).then_some((
                        true,
                        c.token.is_some(),
                        c.group_generation,
                    ))
                })
            })
            .unwrap_or((false, false, 0));
        match reconcile_action(self.live(), matching_owner, token_armed) {
            ReconcileAction::Arm => {
                if matching_owner {
                    let _ = self.enter_current(group_generation);
                }
            }
            ReconcileAction::Stop => self.stop_current_with(StopSettlement::Normal),
            ReconcileAction::Keep => {}
        }
    }

    /// Terminal kexec cleanup.  The caller will never resume ordinary task
    /// execution, so retain the Arc rather than allowing the final fixed-view
    /// drop to run in the terminal IPI context.
    pub(crate) fn quiesce_current_cpu() {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        let Some(slot) = CUSTODY.get(cpu) else { return };
        let custodians = {
            let mut table = slot.lock();
            core::mem::replace(&mut *table, core::array::from_fn(|_| None))
        };
        for mut custody in custodians.into_iter().flatten() {
            if let Some(token) = custody.token.take() {
                let _ = axhal::pmu::sampling_stop_local(token);
            }
            core::mem::forget(custody.event);
        }
        let _ = axhal::pmu::sampling_quiesce_local();
    }

    /// Vector-2 NMI capture: only the platform's cacheline-local PMU slot is
    /// touched here.  In particular, do not acquire custody/state locks,
    /// clone/drop an Arc, write a user mapping, or wake waiters from NMI.
    pub(crate) fn handle_nmi(frame: &axcpu::TrapFrame) {
        let mut completions = [axhal::pmu::PmiCompletion {
            sample: axhal::pmu::PmiSample {
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
        }; MAX_SAMPLING_CUSTODIES];
        if let Ok(count) =
            axhal::pmu::sampling_nmi_take_pmis(frame.rip, frame.cs & 3 == 3, &mut completions)
        {
            // One PMI may report multiple owned counters.  Capture each
            // precise record by its own cookie before returning from NMI;
            // never let the first overflow suppress a sibling's PEBS handoff.
            for completion in &completions[..count] {
                let _ = axhal::perf_precise_aux::capture_pebs_nmi(
                    completion.sample.cookie,
                    completion.counter_bit,
                );
            }
        }
    }

    /// Complete one captured NMI in ordinary task context.  This is the
    /// deferred producer half: it may take the normal ownership locks, emit a
    /// data/lost record and wake poll waiters, then rearm only if generation
    /// still names the same custody.
    fn drain_nmi_local() {
        let mut completions = [axhal::pmu::PmiCompletion {
            sample: axhal::pmu::PmiSample {
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
        }; NMI_COMPLETION_DRAIN_BATCH];
        let count = axhal::pmu::sampling_nmi_take_completions_local(&mut completions);
        for completion in &completions[..count] {
            Self::drain_nmi_completion_local(*completion);
        }
    }

    fn drain_nmi_completion_local(completion: axhal::pmu::PmiCompletion) {
        let cpu = axhal::percpu::this_cpu_id();
        let Some(slot) = CUSTODY.get(cpu) else { return };
        let event = {
            let custody_slot = slot.lock();
            let Some(custody) = custody_slot.iter().flatten().find(|custody| {
                custody.token.is_some() && custody.cookie == completion.sample.cookie
            }) else {
                // A previous stop has already terminated hardware; do not
                // turn an empty retained custody into a false running fault.
                return;
            };
            // This is normal context; a temporary clone is safe because the
            // static custody keeps a strong owner through scheduler leave.
            Some(custody.event.clone())
        };
        let Some(event) = event else { return };
        let mut state = event.state.lock();
        if !state.enabled || state.closed || state.failed || state.ring.is_none() {
            drop(state);
            event.stop_current_with(StopSettlement::Normal);
            return;
        }
        let exact = exact_capture_requirements(state.sample_type, event.config.aux);
        let mut branches = [PerfBranchEntry::default(); 32];
        let mut branch_len = 0;
        let mut lbr_captured = !exact.lbr;
        #[cfg(all(feature = "pmu", target_os = "none"))]
        if exact.lbr {
            if let Some(token) = CUSTODY.get(cpu).and_then(|slot| {
                slot.lock()
                    .iter()
                    .flatten()
                    .find_map(|c| Arc::ptr_eq(&c.event, &event).then_some(c.lbr))
                    .flatten()
            }) {
                let mut lbr = [axhal::perf_precise_aux::LbrEntry::default(); 32];
                if let Ok(count) = axhal::perf_precise_aux::read_lbr_local(token, &mut lbr) {
                    for (dst, src) in branches.iter_mut().zip(lbr).take(count) {
                        *dst = PerfBranchEntry {
                            from: src.from,
                            to: src.to,
                            flags: 0,
                        };
                    }
                    branch_len = count;
                    lbr_captured = true;
                }
            }
        }
        let mut ip = completion.ip;
        let mut addr = 0;
        let mut data_src = 0;
        let mut exact_ip = false;
        let mut pebs_present = !exact.pebs;
        let mut pebs_decoded = !exact.pebs;
        #[cfg(all(feature = "pmu", target_os = "none"))]
        if exact.pebs {
            if let Ok(Some(raw)) = axhal::perf_precise_aux::take_pebs_record_local(event.config.id)
            {
                pebs_present = true;
                if let Ok(record) = axhal::perf_precise_aux::decode_pebs_record(
                    axhal::perf_precise_aux::PebsFormat::PantherCoveBasic,
                    &raw,
                ) {
                    ip = record.ip;
                    addr = record.data_linear_address;
                    data_src = record.data_source;
                    exact_ip = true;
                    pebs_decoded = true;
                }
            }
        }
        if exact_completion_action(exact, pebs_present, pebs_decoded, lbr_captured)
            == ExactCompletionAction::DropAsLost
        {
            if let Some(ring) = state.ring.as_mut() {
                // This completion itself is lost in addition to any loss
                // reported by the bounded NMI queue.  Do not account a
                // phantom period or invoke completion BPF for it.
                ring.lost = ring.lost.saturating_add(exact_drop_lost(completion.lost));
            }
            drop(state);
            return;
        }
        let mut bytes = [0_u8; 1024];
        let size = encode_sample_record_fields(
            &mut bytes,
            state.sample_type,
            PerfSampleFields {
                identifier: event.config.id,
                ip,
                user: completion.user,
                exact_ip,
                time: axhal::time::monotonic_time_nanos(),
                cpu: cpu as u32,
                period: completion.sample.period,
                addr,
                pid: axtask::current().id().as_u64() as u32,
                tid: axtask::current().id().as_u64() as u32,
                id: event.config.id,
                stream_id: event.config.id,
                data_src,
                raw: &[],
                branches: &branches[..branch_len],
                // PT/BTS bytes are published through the separate AUX mmap
                // transport and PERF_RECORD_AUX.  The sample field remains
                // ABI-complete with a truthful zero-length payload here.
                aux: &[],
            },
        )
        .unwrap_or(0);
        if size == 0 {
            state.failed = true;
            drop(state);
            #[cfg(feature = "bpf")]
            event.run_completion_bpf(completion.sample.period);
            event.stop_current_with(StopSettlement::RunningOnly);
            return;
        }
        state.value = state.value.saturating_add(completion.sample.period);
        // `SET_OUTPUT` redirects only data-ring publication; accounting and
        // PMU ownership remain with the originating event.  Registry rejects
        // output cycles, so this second lock cannot recurse back into `state`.
        let output = event.output_target();
        let (published, wake_needed) = if let Some(output) = output.as_ref() {
            let mut destination = output.state.lock();
            let wakeup = destination.wakeup;
            if destination.output_paused {
                (PublishResult::default(), false)
            } else if let Some(ring) = destination.ring.as_mut() {
                let published = publish_record(ring, &bytes[..size], event.config.id);
                let wake = publication_should_wake(wakeup, ring, published.published);
                (published, wake)
            } else {
                (PublishResult::default(), false)
            }
        } else {
            let wakeup = state.wakeup;
            let ring = state.ring.as_mut().unwrap();
            let published = publish_record(ring, &bytes[..size], event.config.id);
            let wake = publication_should_wake(wakeup, ring, published.published);
            (published, wake)
        };
        if published.overflow {
            state.failed = true;
        }
        // The state lock spans the rearm decision: close/disable can neither
        // win after the period was accounted nor leave a live counter behind.
        if completion.lost != 0 {
            if let Some(ring) = state.ring.as_mut() {
                ring.lost = ring.lost.saturating_add(completion.lost);
            }
        }
        if state.refresh_budget != 0 {
            state.refresh_budget -= 1;
            if state.refresh_budget == 0 {
                state.enabled = false;
                state.enabled_total = state.enabled_total.saturating_add(
                    axhal::time::monotonic_time_nanos().saturating_sub(state.enabled_since),
                );
                if let Some(ring) = state.ring.as_mut() {
                    let mut throttle = [0u8; 32];
                    encode_throttle_record(
                        &mut throttle,
                        thekernel_linux_perf::PERF_RECORD_THROTTLE,
                        axhal::time::monotonic_time_nanos(),
                        event.config.id,
                    );
                    let _ = publish_record(ring, &throttle, event.config.id);
                }
            }
        }
        let rearm = !state.failed && state.enabled && !state.closed && state.ring.is_some();
        drop(state);
        #[cfg(feature = "bpf")]
        event.run_completion_bpf(completion.sample.period);
        if wake_needed {
            if let Some(output) = output {
                wake_perf_waiters(&output.waiters);
            } else {
                wake_perf_waiters(&event.waiters);
            }
        }
        if rearm {
            // Vector-2 already reloaded and unmasked this exact counter after
            // placing the completion in its fixed reservation.  Normal
            // context is deliberately only the publication/wakeup half.
            return;
        }
        event.stop_current_with(StopSettlement::RunningOnly);
    }

    /// Initializes PMU overflow delivery through architectural NMI vector 2.
    pub(crate) fn init_nmi() -> bool {
        true
    }

    fn install_ring(&self, request: FileMmapRequest, mlock: crate::perf_security::PerfMlockContext) -> AxResult<Arc<crate::perf_security::PerfMlockReservation>> {
        if !self.target_current() {
            return Err(AxError::OperationNotSupported);
        }
        if request.offset() != 0
            || request.sharing() != FileMmapSharing::Shared
            || request.protection().contains(FileMmapProtection::EXECUTE)
            || !request
                .protection()
                .contains(FileMmapProtection::READ | FileMmapProtection::WRITE)
        {
            return Err(AxError::InvalidInput);
        }
        let total = request.length();
        let Some(data_size) = total.checked_sub(PAGE) else {
            return Err(AxError::InvalidInput);
        };
        if request.page_size() != PAGE
            || data_size < MIN_DATA
            || data_size > MAX_DATA
            || !data_size.is_power_of_two()
            || !data_size.is_multiple_of(PAGE)
        {
            return Err(AxError::InvalidInput);
        }
        let charge = {
            let mut state = self.state.lock();
            Self::mapping_charge(&mut state.data_charge, mlock, total)?
        };
        if let Some(ring) = self.state.lock().ring.as_ref() {
            return if ring.data_size == data_size {
                Ok(charge)
            } else {
                Err(AxError::ResourceBusy)
            };
        }
        let pages = Arc::try_new(SharedPages::new_fixed(
            total,
            axhal::paging::PageSize::Size4K,
        )?)
        .map_err(|_| AxError::NoMemory)?;
        let view = pages.fixed_view()?;
        let head = view.atomic_u64(DATA_HEAD)?;
        let tail = view.atomic_u64(DATA_TAIL)?;
        let sequence = view.atomic_u32(META_LOCK)?;
        // These words are immutable geometry, but atomics avoid introducing a
        // second ordinary-byte writer into a mapping shared with userspace.
        view.atomic_u64(DATA_OFFSET)?.store_release(PAGE as u64);
        view.atomic_u64(DATA_SIZE)?.store_release(data_size as u64);
        initialize_metadata_page(&view, &sequence, data_size)?;
        let region = FixedSharedMmapRegion::try_new(
            0,
            pages.clone(),
            FileMmapProtection::READ | FileMmapProtection::WRITE,
        )?;
        // PEBS owns a private output page plus a private DS descriptor page;
        // neither is ever mapped to userspace.  Allocate both before the
        // visible data ring is committed so mmap failure cannot leave a live
        // hardware descriptor behind.
        let needs_pebs = {
            let state = self.state.lock();
            exact_capture_requirements(state.sample_type, self.config.aux).pebs
                && state.pebs.is_none()
        };
        let pebs = if needs_pebs {
            Some(PebsRing {
                data: Arc::try_new(SharedPages::new_fixed(
                    PAGE,
                    axhal::paging::PageSize::Size4K,
                )?)
                .map_err(|_| AxError::NoMemory)?,
                ds: Arc::try_new(SharedPages::new_fixed(
                    PAGE,
                    axhal::paging::PageSize::Size4K,
                )?)
                .map_err(|_| AxError::NoMemory)?,
            })
        } else {
            None
        };
        let mut state = self.state.lock();
        if let Some(ring) = state.ring.as_ref() {
            return if ring.data_size == data_size {
                Ok(charge)
            } else {
                Err(AxError::ResourceBusy)
            };
        }
        state.ring = Some(Ring {
            region,
            pages,
            view,
            head,
            tail,
            sequence,
            data_size,
            producer_head: 0,
            lost: 0,
            records_since_wakeup: 0,
        });
        if state.pebs.is_none() {
            state.pebs = pebs;
        }
        Ok(charge)
    }

    fn install_aux_ring(&self, request: FileMmapRequest, mlock: crate::perf_security::PerfMlockContext) -> AxResult<Arc<crate::perf_security::PerfMlockReservation>> {
        if !self.config.aux.map_or(false, |request| request.aux) {
            return Err(AxError::OperationNotSupported);
        }
        if request.sharing() != FileMmapSharing::Shared
            || request.protection().contains(FileMmapProtection::EXECUTE)
            || !request
                .protection()
                .contains(FileMmapProtection::READ | FileMmapProtection::WRITE)
            || request.page_size() != PAGE
            || request.length() < PAGE
            || request.length() > MAX_DATA
            || !request.length().is_power_of_two()
            || !request.length().is_multiple_of(PAGE)
        {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        let data = state.ring.as_ref().ok_or(AxError::InvalidInput)?;
        let offset = aux_mapping_offset(data.data_size, request.length())?;
        if request.offset() != offset {
            return Err(AxError::InvalidInput);
        }
        let aux_request = self.config.aux.expect("checked above");
        if aux_request.sample_size as usize > request.length()
            || aux_request.watermark as usize > request.length()
        {
            return Err(AxError::InvalidInput);
        }
        let charge = Self::mapping_charge(&mut state.aux_charge, mlock, request.length())?;
        if let Some(aux) = state.aux.as_ref() {
            return if aux.data_size == request.length() {
                Ok(charge)
            } else {
                Err(AxError::ResourceBusy)
            };
        }
        let pages = Arc::try_new(SharedPages::new_fixed(
            request.length(),
            axhal::paging::PageSize::Size4K,
        )?)
        .map_err(|_| AxError::NoMemory)?;
        let topa = Arc::try_new(SharedPages::new_fixed(
            PAGE,
            axhal::paging::PageSize::Size4K,
        )?)
        .map_err(|_| AxError::NoMemory)?;
        let region = FixedSharedMmapRegion::try_new(
            offset,
            pages.clone(),
            FileMmapProtection::READ | FileMmapProtection::WRITE,
        )?;
        let backend = aux_request.admit()?.ok_or(AxError::OperationNotSupported)?;
        let bts_ds = match backend {
            super::perf_aux::AuxBackend::IntelPt => {
                program_topa(&topa, &pages)?;
                None
            }
            super::perf_aux::AuxBackend::Bts => {
                // BTS owns one dedicated 4KiB physical page: its usable
                // 4080-byte prefix is exactly 170 contiguous 24-byte records.
                if request.length() != PAGE {
                    return Err(AxError::OperationNotSupported);
                }
                Some(
                    Arc::try_new(SharedPages::new_fixed(
                        PAGE,
                        axhal::paging::PageSize::Size4K,
                    )?)
                    .map_err(|_| AxError::NoMemory)?,
                )
            }
        };
        // Metadata and AUX storage are committed together, after every
        // allocation above has succeeded.  The hardware program is performed
        // by the task-context reconciler, never by mmap while holding this
        // state lock.
        let data = state.ring.as_ref().expect("validated data ring remains installed");
        data.view.atomic_u64(AUX_HEAD)?.store_release(0);
        data.view.atomic_u64(AUX_TAIL)?.store_release(0);
        data.view.atomic_u64(AUX_OFFSET)?.store_release(offset);
        data.view
            .atomic_u64(AUX_SIZE)?
            .store_release(request.length() as u64);
        state.aux = Some(AuxRing {
            region,
            pages,
            topa,
            bts_ds,
            backend,
            data_size: request.length(),
        });
        Ok(charge)
    }

    fn read_count(&self, dst: &mut IoDst) -> AxResult<usize> {
        if !self.target_current() {
            return Err(AxError::OperationNotSupported);
        }
        // Reading an active event settles its residual counter before taking
        // the snapshot.  Custody keeps the backing alive while IRQs are off.
        let active = self.live();
        if active {
            self.stop_if_current();
        }
        let state = self.state.lock();
        if state.failed {
            // stop_current above consumed the token.  Do not rearm a sticky
            // hardware failure; leave later releases the retained custody Arc.
            return Err(AxError::Io);
        }
        let now = axhal::time::monotonic_time_nanos();
        if let Some(ring) = state.ring.as_ref() {
            sync_metadata(ring, &state, now)?;
        }
        let value = state.value;
        let lost = state.ring.as_ref().map_or(0, |ring| ring.lost);
        let enabled = state.enabled_total.saturating_add(if state.enabled {
            now.saturating_sub(state.enabled_since)
        } else {
            0
        });
        let running = state.running_total.saturating_add(
            state
                .running_since
                .map_or(0, |since| now.saturating_sub(since)),
        );
        drop(state);
        let words = 1
            + usize::from(self.config.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0)
            + usize::from(self.config.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0)
            + usize::from(self.config.read_format & PERF_FORMAT_ID != 0)
            + usize::from(self.config.read_format & PERF_FORMAT_LOST != 0);
        if dst.remaining_mut() < words * 8 {
            return Err(AxError::InvalidInput);
        }
        dst.write(&value.to_ne_bytes())?;
        if self.config.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 {
            dst.write(&enabled.to_ne_bytes())?;
        }
        if self.config.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 {
            dst.write(&running.to_ne_bytes())?;
        }
        if self.config.read_format & PERF_FORMAT_ID != 0 {
            dst.write(&self.config.id.to_ne_bytes())?;
        }
        if self.config.read_format & PERF_FORMAT_LOST != 0 {
            dst.write(&lost.to_ne_bytes())?;
        }
        Ok(words * 8)
    }
}

/// Construct the caller-owned ToPA table before enabling PT.  Each output
/// page gets one entry and the final END entry loops back to the table, so the
/// platform can use its documented wrap arithmetic without a contiguous AUX
/// allocation assumption.
fn program_topa(topa: &Arc<SharedPages>, output: &Arc<SharedPages>) -> AxResult {
    let entries = output.len().checked_add(1).ok_or(AxError::InvalidInput)?;
    if entries.saturating_mul(8) > PAGE {
        return Err(AxError::InvalidInput);
    }
    let view = topa.fixed_view()?;
    for index in 0..output.len() {
        let entry = (output.paddr_at(index)?.as_usize() as u64).to_le_bytes();
        // SAFETY: this is the one-time pre-enable construction of a private,
        // pinned page; no CPU can consume the table until start_pt_local.
        unsafe { view.write_wrapped(0, PAGE, index * 8, &entry)? };
    }
    let end = ((topa.paddr_at(0)?.as_usize() as u64) | 1).to_le_bytes();
    // SAFETY: same private, pre-enable table construction as above.
    unsafe { view.write_wrapped(0, PAGE, output.len() * 8, &end)? };
    Ok(())
}

#[register_trap_handler(NMI)]
fn perf_sampling_nmi(frame: &axcpu::TrapFrame) -> bool {
    PerfSampleBackend::handle_nmi(frame);
    true
}

impl PerfSampleBackend {
    pub(crate) fn final_close(&self) {
        let now = axhal::time::monotonic_time_nanos();
        {
            let mut state = self.state.lock();
            if state.enabled {
                state.enabled_total = state
                    .enabled_total
                    .saturating_add(now.saturating_sub(state.enabled_since));
                state.enabled = false;
            }
            state.closed = true;
            if let Some(ring) = state.ring.as_ref() {
                let _ = sync_metadata(ring, &state, now);
            }
            // Last-fd close stops production, but existing VMA leases still
            // retain their mapping charge. Only the last VMA/plan lease
            // refunds it; the backend retains a weak reference.
        }
        #[cfg(target_os = "none")]
        // `final_close` still runs while the descriptor owns a strong Arc;
        // reconcile synchronously so close/CLOEXEC never rely on a tick.
        if let Some(event) = self.custody_reference() {
            crate::file::perf::reconcile_sampling_last(&event);
        }
        wake_perf_waiters(&self.waiters);
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.read_count(dst)
    }
    fn write(&self, _: &mut IoSrc) -> AxResult<usize> {
        Err(AxError::BadFileDescriptor)
    }
    fn mapping_charge(
        weak: &mut alloc::sync::Weak<crate::perf_security::PerfMlockReservation>,
        mlock: crate::perf_security::PerfMlockContext, bytes: usize,
    ) -> AxResult<Arc<crate::perf_security::PerfMlockReservation>> {
        if let Some(charge) = weak.upgrade() { return Ok(charge); }
        let charge = Arc::try_new(mlock.reserve(bytes)?).map_err(|_| AxError::NoMemory)?;
        *weak = Arc::downgrade(&charge);
        Ok(charge)
    }

    pub(crate) fn prepare_mmap(
        &self, request: FileMmapRequest,
    ) -> AxResult<Option<PreparedFileMmap>> {
        let mlock = crate::perf_security::perf_mlock_context()?;
        let charge = if request.offset() == 0 { self.install_ring(request, mlock)? }
            else { self.install_aux_ring(request, mlock)? };
        let state = self.state.lock();
        let region = if request.offset() == 0 {
            state.ring.as_ref().map(|ring| &ring.region)
        } else { state.aux.as_ref().map(|ring| &ring.region) };
        let Some(plan) = region.map(|region| region.prepare(request)).transpose()?.flatten() else {
            return Ok(None);
        };
        Ok(Some(plan.with_mapping_lifetime(charge).with_excluded_fork_and_dump()))
    }
    pub(crate) fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        if cmd == PERF_EVENT_IOC_SET_FILTER {
            let bytes = Self::copy_filter(context, arg)?;
            let mut state = self.state.lock();
            match self.config.event {
                SamplingEvent::Source => {
                    state.filter = Some(Arc::new(SamplingFilter::parse_source(&bytes)?));
                }
                _ if matches!(
                    state.aux.as_ref().map(|aux| aux.backend),
                    Some(super::perf_aux::AuxBackend::IntelPt)
                ) =>
                {
                    // Address filters are synchronized at the same local
                    // start boundary as Linux's `addr_filters_sync`.  Do not
                    // mutate RTIT MSRs below an already-running generation:
                    // userspace must disable it first, making the stop and
                    // reprogramming boundary explicit instead of racing AUX
                    // ownership or silently taking effect later.
                    if state.running_since.is_some() {
                        return Err(AxError::ResourceBusy);
                    }
                    state.pt_filter = parse_pt_address_filter(&bytes)?;
                }
                _ => return Err(AxError::OperationNotSupported),
            }
            return Ok(0);
        }
        if cmd == PERF_EVENT_IOC_MODIFY_ATTRIBUTES {
            self.modify_attributes(context, arg)?;
            return Ok(0);
        }
        if cmd == PERF_EVENT_IOC_ID {
            context
                .user_memory()
                .write_value(arg as *mut u64, self.config.id)
                .map_err(crate::mm::map_usercopy_error)?;
            return Ok(0);
        }
        if cmd == PERF_EVENT_IOC_REFRESH {
            if arg == 0 {
                return Err(AxError::InvalidInput);
            }
            let now = axhal::time::monotonic_time_nanos();
            let mut state = self.state.lock();
            let was_disabled = !state.enabled;
            state.refresh_budget = state.refresh_budget.saturating_add(arg as u64);
            if !state.enabled {
                state.enabled = true;
                state.enabled_since = now;
            }
            if was_disabled && !state.output_paused {
                if let Some(ring) = state.ring.as_mut() {
                    let mut unthrottle = [0u8; 32];
                    encode_throttle_record(
                        &mut unthrottle,
                        thekernel_linux_perf::PERF_RECORD_UNTHROTTLE,
                        now,
                        self.config.id,
                    );
                    let _ = publish_record(ring, &unthrottle, self.config.id);
                }
            }
            if let Some(ring) = state.ring.as_ref() {
                sync_metadata(ring, &state, now)?;
            }
            return Ok(0);
        }
        if cmd == PERF_EVENT_IOC_PERIOD {
            let period = context
                .user_memory()
                .read_value(arg as *const u64)
                .map_err(crate::mm::map_usercopy_error)?;
            if period == 0 {
                return Err(AxError::InvalidInput);
            }
            if self.live() {
                self.stop_if_current();
            }
            let mut state = self.state.lock();
            state.period = period;
            state.source_progress = 0;
            state.source_frequency_observed = 0;
            state.last_frequency_adjust = axhal::time::monotonic_time_nanos();
            state.last_source_sample = state.last_frequency_adjust;
            if let Some(ring) = state.ring.as_ref() {
                sync_metadata(ring, &state, axhal::time::monotonic_time_nanos())?;
            }
            return Ok(0);
        }
        if cmd == PERF_EVENT_IOC_PAUSE_OUTPUT {
            if arg > 1 {
                return Err(AxError::InvalidInput);
            }
            let mut state = self.state.lock();
            state.output_paused = arg != 0;
            return Ok(0);
        }
        if !matches!(
            cmd,
            PERF_EVENT_IOC_ENABLE | PERF_EVENT_IOC_DISABLE | PERF_EVENT_IOC_RESET
        ) || arg != 0
        {
            return Err(AxError::InvalidInput);
        }
        let now = axhal::time::monotonic_time_nanos();
        let mut state = self.state.lock();
        match cmd {
            PERF_EVENT_IOC_ENABLE => {
                if !state.enabled {
                    state.enabled = true;
                    state.enabled_since = now;
                }
            }
            PERF_EVENT_IOC_DISABLE => {
                if state.enabled {
                    state.enabled_total = state
                        .enabled_total
                        .saturating_add(now.saturating_sub(state.enabled_since));
                    state.enabled = false;
                }
            }
            PERF_EVENT_IOC_RESET => {
                state.value = 0;
                state.source_progress = 0;
                state.source_frequency_observed = 0;
                state.last_frequency_adjust = now;
                state.last_source_sample = now;
                state.failed = false;
                if let Some(ring) = state.ring.as_mut() {
                    ring.lost = 0;
                }
            }
            _ => unreachable!("validated perf sampling ioctl"),
        }
        if let Some(ring) = state.ring.as_ref() {
            sync_metadata(ring, &state, now)?;
        }
        drop(state);
        if cmd == PERF_EVENT_IOC_DISABLE {
            #[cfg(target_os = "none")]
            if let Some(event) = self.custody_reference() {
                crate::file::perf::reconcile_sampling_last(&event);
            }
        }
        Ok(0)
    }
    fn nonblocking(&self) -> bool {
        false
    }
    fn set_nonblocking(&self, _: bool) -> AxResult {
        Ok(())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[perf_event]",
        )))
    }
}

impl Pollable for PerfSampleBackend {
    fn poll(&self) -> IoEvents {
        let state = self.state.lock();
        let mut events = IoEvents::empty();
        if state.closed {
            events |= IoEvents::HANGUP;
        }
        if state.failed {
            events |= IoEvents::ERROR;
        }
        if state.ring.as_ref().is_some_and(|ring| {
            producer_window(ring.producer_head, ring.tail.load_acquire(), ring.data_size)
                .is_some_and(|used| used != 0)
        }) {
            events |= IoEvents::READABLE;
        }
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.intersects(IoEvents::READABLE | IoEvents::HANGUP | IoEvents::ERROR) {
            PollRegistration::single(&self.waiters, context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}

#[derive(Clone, Copy, Default)]
struct PublishResult {
    published: bool,
    overflow: bool,
}

fn producer_window(head: u64, tail: u64, size: usize) -> Option<u64> {
    let used = head.checked_sub(tail)?;
    (used <= size as u64).then_some(used)
}

fn has_space(size: usize, used: u64, record: usize) -> bool {
    (size as u64).saturating_sub(used) >= record as u64
}

/// Linux's THROTTLE/UNTHROTTLE payload is `time,id,stream_id` after the
/// common header.  Sampling descriptors use their stable event ID for both
/// identifiers because they have one stream per FD.
fn encode_throttle_record(out: &mut [u8; 32], kind: u32, time: u64, id: u64) {
    let mut header = [0u8; 8];
    thekernel_linux_perf::PerfRecordHeader::new(kind, 0, 32).encode(&mut header);
    out[..8].copy_from_slice(&header);
    out[8..16].copy_from_slice(&time.to_ne_bytes());
    out[16..24].copy_from_slice(&id.to_ne_bytes());
    out[24..32].copy_from_slice(&id.to_ne_bytes());
}

fn publish_record(ring: &mut Ring, sample: &[u8], id: u64) -> PublishResult {
    let tail = ring.tail.load_acquire();
    let Some(mut used) = producer_window(ring.producer_head, tail, ring.data_size) else {
        ring.lost = ring.lost.saturating_add(1);
        return PublishResult::default();
    };
    let mut lost = [0_u8; 24];
    let pending = ring.lost;
    let mut published_any = false;
    encode_lost_record(&mut lost, id, pending);
    let mut head = ring.producer_head;
    // LOST is independently useful: publish it first whenever it fits, even
    // when this SAMPLE cannot.  Each record has its own all-or-nothing head.
    if pending != 0 && has_space(ring.data_size, used, lost.len()) {
        let Some(next) = head.checked_add(lost.len() as u64) else {
            return PublishResult {
                published: false,
                overflow: true,
            };
        };
        if write_record(ring, head, &lost).is_err() {
            return PublishResult::default();
        }
        head = next;
        used += lost.len() as u64;
        ring.lost = 0;
        ring.producer_head = head;
        ring.head.store_release(head);
        published_any = true;
    }
    if !has_space(ring.data_size, used, sample.len()) {
        ring.lost = ring.lost.saturating_add(1);
        return PublishResult {
            published: published_any,
            overflow: false,
        };
    }
    let Some(next) = head.checked_add(sample.len() as u64) else {
        return PublishResult {
            published: published_any,
            overflow: true,
        };
    };
    if write_record(ring, head, sample).is_err() {
        return PublishResult {
            published: published_any,
            overflow: false,
        };
    }
    ring.producer_head = next;
    ring.head.store_release(next);
    PublishResult {
        published: true,
        overflow: false,
    }
}
fn publish_raw_record(ring: &mut Ring, header: &[u8; 12], raw: &[u8], id: u64) -> PublishResult {
    let total = header.len() + raw.len();
    let tail = ring.tail.load_acquire();
    let Some(mut used) = producer_window(ring.producer_head, tail, ring.data_size) else {
        ring.lost = ring.lost.saturating_add(1);
        return PublishResult::default();
    };
    let mut head = ring.producer_head;
    let mut published = false;
    if ring.lost != 0 {
        let mut lost = [0u8; 24];
        encode_lost_record(&mut lost, id, ring.lost);
        if ring.data_size.saturating_sub(used as usize) >= lost.len() {
            let Some(next) = head.checked_add(lost.len() as u64) else {
                return PublishResult {
                    published: false,
                    overflow: true,
                };
            };
            if write_record(ring, head, &lost).is_err() {
                return PublishResult::default();
            }
            head = next;
            used += lost.len() as u64;
            ring.lost = 0;
            published = true;
        }
    }
    if ring.data_size.saturating_sub(used as usize) < total {
        ring.lost = ring.lost.saturating_add(1);
        ring.producer_head = head;
        ring.head.store_release(head);
        return PublishResult {
            published,
            overflow: false,
        };
    }
    let Some(next) = head.checked_add(total as u64) else {
        return PublishResult {
            published,
            overflow: true,
        };
    };
    if write_record(ring, head, header).is_err()
        || write_record(ring, head + header.len() as u64, raw).is_err()
    {
        ring.lost = ring.lost.saturating_add(1);
        return PublishResult {
            published,
            overflow: false,
        };
    }
    ring.producer_head = next;
    ring.head.store_release(next);
    PublishResult {
        published: true,
        overflow: false,
    }
}
fn write_record(ring: &Ring, head: u64, bytes: &[u8]) -> AxResult {
    let offset = (head as usize) & (ring.data_size - 1);
    // SAFETY: `PerfSampleBackend::state` serializes this sole producer; the
    // acquire tail / release head protocol above proves these bytes are not
    // consumer-owned, and `bytes.len() <= data_size` is fixed by record ABI.
    unsafe { ring.view.write_wrapped(PAGE, ring.data_size, offset, bytes) }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    use core::sync::atomic::{AtomicUsize, Ordering};

    use spin::{Mutex, MutexGuard};
    use thekernel_linux_perf::{
        PERF_SAMPLE_CPU, PERF_SAMPLE_IP, PERF_SAMPLE_PERIOD, PERF_SAMPLE_TIME,
    };

    // These tests exercise the production global retire queue.  Keep their
    // publication and draining isolated from each other so no test can
    // consume another test's custody reference.
    static RETIRE_QUEUE_TEST_SERIAL: Mutex<()> = Mutex::new(());

    struct RetireQueueTestContext {
        _serial: MutexGuard<'static, ()>,
    }

    impl RetireQueueTestContext {
        fn new() -> Self {
            let serial = RETIRE_QUEUE_TEST_SERIAL.lock();
            drain_retire_queue_fully();
            Self { _serial: serial }
        }
    }

    impl Drop for RetireQueueTestContext {
        fn drop(&mut self) {
            drain_retire_queue_fully();
        }
    }

    fn drain_retire_queue_fully() {
        while has_deferred_custody_retire_work() {
            drain_deferred_custody_retire_work();
        }
        assert!(!has_deferred_custody_retire_work());
    }

    fn test_sampling_event() -> Arc<PerfSampleBackend> {
        PerfSampleBackend::try_new(SamplingConfig {
            id: 1,
            target_task_id: 0,
            event: SamplingEvent::Cycles,
            period: 1,
            frequency: None,
            sample_type: PERF_SAMPLE_IP,
            count_user: true,
            count_kernel: false,
            disabled: true,
            read_format: 0,
            aux: None,
            identity: PerfOpenIdentity {
                attr: thekernel_linux_perf::PerfEventAttr::default(),
                target: thekernel_linux_perf::PerfOpenTarget {
                    target: thekernel_linux_perf::PerfTarget::Task { pid: 0, cpu: -1 },
                    group_fd: -1,
                    output_fd: -1,
                    open_flags: 0,
                },
                authority: crate::perf_security::PerfAuthority::Restricted,
            },
        })
        .unwrap()
    }

    #[test]
    fn stopping_a_descriptor_keeps_its_live_mapping_charge() {
        let _context = crate::test_support::scheduler_test_context();
        let event = test_sampling_event();
        let mapping = Arc::new(crate::perf_security::PerfMlockReservation::for_test());
        event.state.lock().data_charge = Arc::downgrade(&mapping);
        event.final_close();
        assert!(event.state.lock().data_charge.upgrade().is_some());
        drop(mapping);
        assert!(event.state.lock().data_charge.upgrade().is_none());
    }

    #[test]
    fn source_record_keeps_the_captured_timestamp_until_encoding() {
        let _context = crate::test_support::scheduler_test_context();
        let event = test_sampling_event();
        {
            let mut state = event.state.lock();
            state.enabled = true;
            state.sample_type = PERF_SAMPLE_TIME | thekernel_linux_perf::PERF_SAMPLE_RAW;
        }
        let captured = 123_456_789;
        // No ring was mapped: encoding runs, then the provider reports the
        // missing destination. Inspect that real encoded source record,
        // rather than testing only the generic record encoder in isolation.
        assert_eq!(
            event.emit_source_raw_record_at(0, 0, true, &[7, 8], captured),
            Err(AxError::OperationNotSupported),
        );
        let state = event.state.lock();
        assert_eq!(
            u64::from_ne_bytes(state.scratch[8..16].try_into().unwrap()),
            captured,
        );
        assert_eq!(
            u32::from_ne_bytes(state.scratch[16..20].try_into().unwrap()),
            2,
        );
        assert_eq!(&state.scratch[20..22], &[7, 8]);
    }

    #[test]
    fn nested_perf_notifications_restore_the_outer_recursion_state() {
        let _context = crate::test_support::scheduler_test_context();
        assert!(!notifying_perf_waiters());
        {
            let _outer = PerfWaitNotificationGuard::new();
            assert!(notifying_perf_waiters());
            {
                let _inner = PerfWaitNotificationGuard::new();
                assert!(notifying_perf_waiters());
            }
            assert!(notifying_perf_waiters());
        }
        assert!(!notifying_perf_waiters());
    }

    #[test]
    fn sample_payload_order_and_misc() {
        let mut out = [0; 40];
        let n = encode_sample_record(
            &mut out,
            PERF_SAMPLE_IP | PERF_SAMPLE_TIME | PERF_SAMPLE_CPU | PERF_SAMPLE_PERIOD,
            1,
            true,
            2,
            4,
            5,
        );
        assert_eq!(n, 40);
        assert_eq!(
            u32::from_ne_bytes(out[..4].try_into().unwrap()),
            thekernel_linux_perf::PERF_RECORD_SAMPLE
        );
        assert_eq!(
            u16::from_ne_bytes(out[4..6].try_into().unwrap()),
            thekernel_linux_perf::PERF_RECORD_MISC_USER
        );
        assert_eq!(u64::from_ne_bytes(out[8..16].try_into().unwrap()), 1);
        assert_eq!(u64::from_ne_bytes(out[16..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_ne_bytes(out[24..28].try_into().unwrap()), 4);
        assert_eq!(u64::from_ne_bytes(out[32..40].try_into().unwrap()), 5);
    }
    #[test]
    fn kernel_sample_uses_kernel_misc() {
        let mut out = [0; 40];
        encode_sample_record(&mut out, PERF_SAMPLE_IP, 1, false, 0, 0, 0);
        assert_eq!(
            u16::from_ne_bytes(out[4..6].try_into().unwrap()),
            thekernel_linux_perf::PERF_RECORD_MISC_KERNEL
        );
    }

    #[test]
    fn producer_window_rejects_invalid_tails_and_accepts_exact_space() {
        assert_eq!(producer_window(80, 16, 64), Some(64));
        assert_eq!(producer_window(80, 81, 64), None);
        assert_eq!(producer_window(80, 0, 64), None);
        assert!(has_space(64, 40, 24));
        assert!(!has_space(64, 41, 24));
    }

    #[test]
    fn lost_is_publishable_before_a_sample_that_does_not_fit() {
        assert!(has_space(64, 24, 24));
        assert!(!has_space(64, 24 + 24, 24));
        assert!(!has_space(64, 41, 24));
    }

    #[test]
    fn missing_pebs_record_is_lost_not_an_inexact_pmi_sample() {
        let requirements = exact_capture_requirements(PERF_SAMPLE_ADDR, None);
        assert!(requirements.pebs);
        assert_eq!(
            exact_completion_action(requirements, false, false, true),
            ExactCompletionAction::DropAsLost
        );
        assert_eq!(exact_drop_lost(0), 1);
    }

    #[test]
    fn malformed_pebs_record_is_lost_not_an_inexact_pmi_sample() {
        let requirements = exact_capture_requirements(
            PERF_SAMPLE_DATA_SRC,
            Some(
                AuxRequest::from_v0(&thekernel_linux_perf::PerfEventAttrV0 {
                    flags: thekernel_linux_perf::ATTR_PRECISE_IP,
                    ..thekernel_linux_perf::PerfEventAttrV0::default()
                })
                .unwrap(),
            ),
        );
        assert!(requirements.pebs);
        assert_eq!(
            exact_completion_action(requirements, true, false, true),
            ExactCompletionAction::DropAsLost
        );
        assert_eq!(exact_drop_lost(7), 8);
    }

    #[test]
    fn valid_pebs_and_lbr_capture_publishes_exact_sample() {
        let requirements = exact_capture_requirements(
            PERF_SAMPLE_ADDR | PERF_SAMPLE_DATA_SRC | PERF_SAMPLE_BRANCH_STACK,
            Some(
                AuxRequest::from_v0(&thekernel_linux_perf::PerfEventAttrV0 {
                    flags: thekernel_linux_perf::ATTR_PRECISE_IP,
                    sample_type: PERF_SAMPLE_BRANCH_STACK,
                    ..thekernel_linux_perf::PerfEventAttrV0::default()
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            exact_completion_action(requirements, true, true, true),
            ExactCompletionAction::Publish
        );
    }

    #[test]
    fn counter_head_overflow_is_not_a_resettable_window() {
        assert!(u64::MAX.checked_add(1).is_none());
        assert_eq!(producer_window(96, 64, 64), Some(32));
    }

    #[test]
    fn stopped_custody_rearms_for_read_reset_and_reenable() {
        // A read stop, RESET stop, and DISABLE -> ENABLE all retain custody
        // but clear its token; each live transition must arm it again.
        for _ in 0..3 {
            assert_eq!(reconcile_action(true, true, false), ReconcileAction::Arm);
        }
        assert_eq!(reconcile_action(true, true, true), ReconcileAction::Keep);
        assert_eq!(reconcile_action(false, true, true), ReconcileAction::Stop);
        assert_eq!(reconcile_action(false, true, false), ReconcileAction::Keep);
    }

    #[test]
    fn sample_type_sizes_cover_each_field() {
        for bit in [
            PERF_SAMPLE_IP,
            PERF_SAMPLE_TIME,
            PERF_SAMPLE_CPU,
            PERF_SAMPLE_PERIOD,
        ] {
            let mut out = [0; 40];
            assert_eq!(encode_sample_record(&mut out, bit, 0, false, 0, 0, 0), 16);
        }
    }

    #[test]
    fn repeated_retire_publication_keeps_one_queue_owner() {
        let _context = RetireQueueTestContext::new();
        let event = test_sampling_event();
        let weak = Arc::downgrade(&event);

        for _ in 0..1024 {
            defer_custody_retire(event.clone());
        }
        drop(event);

        assert!(has_deferred_custody_retire_work());
        drain_deferred_custody_retire_work();
        assert!(!has_deferred_custody_retire_work());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn retire_batch_leaves_the_seventeenth_event_pending() {
        let _context = RetireQueueTestContext::new();
        let events: alloc::vec::Vec<_> = (0..RETIRE_BATCH + 1)
            .map(|_| test_sampling_event())
            .collect();
        let weak: alloc::vec::Vec<_> = events.iter().map(Arc::downgrade).collect();

        for event in &events {
            defer_custody_retire(event.clone());
        }
        drop(events);

        drain_deferred_custody_retire_work();
        assert!(has_deferred_custody_retire_work());
        assert!(weak.iter().any(|event| event.upgrade().is_some()));

        drain_deferred_custody_retire_work();
        assert!(!has_deferred_custody_retire_work());
        assert!(weak.iter().all(|event| event.upgrade().is_none()));
    }

    #[test]
    fn consumed_event_with_external_owner_can_be_republished() {
        let _context = RetireQueueTestContext::new();
        let event = test_sampling_event();
        let weak = Arc::downgrade(&event);

        defer_custody_retire(event.clone());
        drain_deferred_custody_retire_work();
        assert!(!has_deferred_custody_retire_work());
        assert!(weak.upgrade().is_some());

        defer_custody_retire(event.clone());
        drain_deferred_custody_retire_work();
        assert!(!has_deferred_custody_retire_work());
        assert!(weak.upgrade().is_some());

        drop(event);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn concurrent_publishers_and_drainer_release_every_event() {
        let _context = RetireQueueTestContext::new();
        let event = test_sampling_event();
        let weak = Arc::downgrade(&event);
        let publishers = Arc::new(AtomicUsize::new(8));

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let event = event.clone();
                let publishers = publishers.clone();
                scope.spawn(move || {
                    for _ in 0..512 {
                        defer_custody_retire(event.clone());
                    }
                    publishers.fetch_sub(1, Ordering::Release);
                });
            }
            while publishers.load(Ordering::Acquire) != 0 || has_deferred_custody_retire_work() {
                drain_deferred_custody_retire_work();
                std::thread::yield_now();
            }
        });

        drop(event);
        drain_retire_queue_fully();
        assert!(weak.upgrade().is_none());
    }
}
