//! The deliberately small, hardware-only perf sampling ABI.
//!
//! Sampling is not a `PerfGroup`: it has one programmable counter and one
//! producer-owned mmap ring per task.  Keeping it separate prevents a PMU
//! overflow interrupt from acquiring the counting group's lifecycle lock.

use alloc::{borrow::Cow, sync::Arc};
use core::{
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axsync::spin::SpinNoIrq;
use axtask::current;
use kernel_guard::NoPreemptIrqSave;
use memory_addr::PAGE_SIZE_4K;

use crate::{
    file::{
        FileLike, FileMmapProtection, FileMmapRequest, FileMmapSharing, FixedSharedMmapRegion,
        IoDst, IoSrc, IoctlContext, Kstat, PreparedFileMmap, anon_inode_stat,
    },
    mm::{SharedAtomicU64, SharedFixedView, SharedPages},
    task::AsThread,
};

pub(crate) const PERF_SAMPLE_IP: u64 = 1;
pub(crate) const PERF_SAMPLE_TIME: u64 = 1 << 2;
pub(crate) const PERF_SAMPLE_CPU: u64 = 1 << 7;
pub(crate) const PERF_SAMPLE_PERIOD: u64 = 1 << 8;
pub(crate) const PERF_SAMPLE_SUPPORTED: u64 =
    PERF_SAMPLE_IP | PERF_SAMPLE_TIME | PERF_SAMPLE_CPU | PERF_SAMPLE_PERIOD;
const PERF_RECORD_LOST: u32 = 2;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_MISC_KERNEL: u16 = 1;
const PERF_RECORD_MISC_USER: u16 = 2;
const PERF_EVENT_IOC_ENABLE: u32 = 0x2400;
const PERF_EVENT_IOC_DISABLE: u32 = 0x2401;
const PERF_EVENT_IOC_RESET: u32 = 0x2403;
const PERF_EVENT_IOC_ID: u32 = 0x8008_2407;
const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1;
const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 2;
const PERF_FORMAT_ID: u64 = 4;
const PAGE: usize = PAGE_SIZE_4K;
const MIN_DATA: usize = PAGE;
const MAX_DATA: usize = 1024 * 1024;
const DATA_HEAD: usize = 1024;
const DATA_TAIL: usize = 1032;
const DATA_OFFSET: usize = 1040;
const DATA_SIZE: usize = 1048;

#[derive(Clone, Copy)]
pub(crate) enum SamplingEvent {
    Cycles,
    Instructions,
}

#[derive(Clone, Copy)]
pub(crate) struct SamplingConfig {
    pub id: u64,
    pub target_task_id: u64,
    pub event: SamplingEvent,
    pub period: u64,
    pub sample_type: u64,
    pub count_user: bool,
    pub count_kernel: bool,
    pub disabled: bool,
    pub read_format: u64,
}

struct Ring {
    region: FixedSharedMmapRegion,
    view: SharedFixedView,
    head: SharedAtomicU64,
    tail: SharedAtomicU64,
    data_size: usize,
    producer_head: u64,
    lost: u64,
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
    ring: Option<Ring>,
}

/// An OFD-owned sampling event.  The producer state is IRQ-safe and has no
/// allocation path after mmap has installed its fixed backing.
pub(crate) struct PerfSamplingFile {
    config: SamplingConfig,
    state: SpinNoIrq<SamplingState>,
    waiters: PollSet<4>,
    retire_next: AtomicPtr<PerfSamplingFile>,
    retire_raw_refs: SpinNoIrq<usize>,
}

struct SamplingRetireQueue {
    incoming: AtomicPtr<PerfSamplingFile>,
    pending: AtomicPtr<PerfSamplingFile>,
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

struct CpuCustody {
    event: Arc<PerfSamplingFile>,
    // Keep the event alive even after an IRQ has stopped the counter.  In
    // particular, never let an interrupt drop the final Ring/FixedView Arc.
    token: Option<axhal::pmu::SamplingToken>,
    cookie: u64,
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

static CUSTODY: [SpinNoIrq<Option<CpuCustody>>; axconfig::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(None) }; axconfig::plat::MAX_CPU_NUM];

fn defer_custody_retire(event: Arc<PerfSamplingFile>) {
    let node = Arc::into_raw(event) as *mut PerfSamplingFile;
    let publish_node = unsafe {
        let mut refs = (*node).retire_raw_refs.lock();
        let first = *refs == 0;
        *refs = refs.checked_add(1).expect("bounded perf retire references");
        first
    };
    if !publish_node {
        return;
    }
    loop {
        let head = RETIRED_CUSTODY.incoming.load(Ordering::Acquire);
        // SAFETY: this raw Arc is uniquely owned by the queue publication.
        unsafe { (*node).retire_next.store(head, Ordering::Relaxed) };
        if RETIRED_CUSTODY
            .incoming
            .compare_exchange_weak(head, node, Ordering::Release, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
    }
}

fn reverse_retire_list(mut head: *mut PerfSamplingFile) -> *mut PerfSamplingFile {
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
        let refs = unsafe {
            let mut refs = (*node).retire_raw_refs.lock();
            let count = *refs;
            *refs = 0;
            count
        };
        // Once refs is zero, a new publisher can requeue the node.  Do not
        // touch intrusive fields after this point.
        for _ in 0..refs {
            // SAFETY: every count originated at Arc::into_raw above.
            drop(unsafe { Arc::from_raw(node) });
        }
        count += 1;
    }
    if !list.is_null() {
        RETIRED_CUSTODY.pending.store(list, Ordering::Release);
    }
    RETIRED_CUSTODY.draining.store(false, Ordering::Release);
}

impl PerfSamplingFile {
    pub(crate) fn try_new(config: SamplingConfig) -> AxResult<Arc<Self>> {
        let now = axhal::time::monotonic_time_nanos();
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
                ring: None,
            }),
            config,
            waiters: PollSet::new(),
            retire_next: AtomicPtr::new(ptr::null_mut()),
            retire_raw_refs: SpinNoIrq::new(0),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn enabled(&self) -> bool {
        let state = self.state.lock();
        state.enabled && !state.closed && !state.failed
    }

    fn target_current(&self) -> bool {
        axtask::current().id().as_u64() == self.config.target_task_id
    }

    pub(crate) fn enter_current(self: &Arc<Self>) {
        if !self.target_current() {
            return;
        }
        if !self.live() {
            return;
        }
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if cpu >= CUSTODY.len() {
            return;
        }
        let reusable = {
            let custody = CUSTODY[cpu].lock();
            match custody.as_ref() {
                None => true,
                Some(custody) => Arc::ptr_eq(&custody.event, self) && custody.token.is_none(),
            }
        };
        if !reusable {
            return;
        }
        let event = match self.config.event {
            SamplingEvent::Cycles => axhal::pmu::Event::Cycles,
            SamplingEvent::Instructions => axhal::pmu::Event::Instructions,
        };
        let program = axhal::pmu::SamplingProgram {
            event,
            period: self.config.period,
            count_user: self.config.count_user,
            count_kernel: self.config.count_kernel,
            cookie: self.config.id,
        };
        let Ok(token) = axhal::pmu::sampling_arm_local(program) else {
            return;
        };
        let mut token = Some(token);
        let installed = {
            let mut custody = CUSTODY[cpu].lock();
            match custody.as_mut() {
                None => {
                    *custody = Some(CpuCustody {
                        event: self.clone(),
                        token: token.take(),
                        cookie: self.config.id,
                    });
                    true
                }
                Some(custody) if Arc::ptr_eq(&custody.event, self) && custody.token.is_none() => {
                    custody.token = token.take();
                    true
                }
                Some(_) => false,
            }
        };
        if !installed {
            let _ = axhal::pmu::sampling_stop_local(token.expect("unpublished sampling token"));
            return;
        }
        let mut state = self.state.lock();
        if state.enabled && !state.closed && !state.failed && state.ring.is_some() {
            state.running_since = Some(axhal::time::monotonic_time_nanos());
            return;
        }
        drop(state);
        // A remote final_close/disable can win after arm.  The custody Arc
        // makes this stop safe here and defers its final destruction to leave.
        Self::stop_current();
    }

    pub(crate) fn leave_current() {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        let Some(mut custody) = CUSTODY.get(cpu).and_then(|slot| slot.lock().take()) else {
            return;
        };
        Self::stop_custody(&mut custody, StopSettlement::Normal);
        defer_custody_retire(custody.event);
    }

    fn live(&self) -> bool {
        let state = self.state.lock();
        state.enabled && !state.closed && !state.failed && state.ring.is_some()
    }

    fn settle_stop(event: &Self, sample: axhal::pmu::StopSample) {
        let mut state = event.state.lock();
        if let Ok(caps) = axhal::pmu::capabilities() {
            let mask = caps.programmable_mask();
            let preload = mask.wrapping_add(1).wrapping_sub(event.config.period);
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
    }

    fn stop_custody(custody: &mut CpuCustody, settlement: StopSettlement) {
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
    }

    /// Stop this CPU's sampler while retaining custody until scheduler leave.
    /// This is safe from IRQ/tick/final-close contexts and never drops Arc.
    pub(crate) fn stop_current() {
        Self::stop_current_with(StopSettlement::Normal);
    }

    fn stop_current_after_pmi() {
        Self::stop_current_with(StopSettlement::RunningOnly);
    }

    fn stop_current_with(settlement: StopSettlement) {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if let Some(slot) = CUSTODY.get(cpu) {
            let mut custody = slot.lock();
            if let Some(custody) = custody.as_mut() {
                Self::stop_custody(custody, settlement);
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
        let cpu = axhal::percpu::this_cpu_id();
        let (matching_owner, token_armed) = CUSTODY
            .get(cpu)
            .and_then(|slot| {
                let custody = slot.lock();
                custody
                    .as_ref()
                    .and_then(|c| Arc::ptr_eq(&c.event, self).then_some((true, c.token.is_some())))
            })
            .unwrap_or((false, false));
        match reconcile_action(self.live(), matching_owner, token_armed) {
            ReconcileAction::Arm => self.enter_current(),
            ReconcileAction::Stop => Self::stop_current(),
            ReconcileAction::Keep => {}
        }
    }

    /// Terminal kexec cleanup.  The caller will never resume ordinary task
    /// execution, so retain the Arc rather than allowing the final fixed-view
    /// drop to run in the terminal IPI context.
    pub(crate) fn quiesce_current_cpu() {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if let Some(mut custody) = CUSTODY.get(cpu).and_then(|slot| slot.lock().take()) {
            if let Some(token) = custody.token.take() {
                let _ = axhal::pmu::sampling_stop_local(token);
            }
            core::mem::forget(custody.event);
        }
        let _ = axhal::pmu::sampling_quiesce_local();
    }

    pub(crate) fn handle_pmi(frame: &axcpu::TrapFrame) {
        let (sample, generation) = match axhal::pmu::sampling_take_pmi() {
            Ok(Some(sample)) => sample,
            Ok(None) => return,
            Err(_) => {
                Self::stop_current();
                return;
            }
        };
        let cpu = axhal::percpu::this_cpu_id();
        let Some(slot) = CUSTODY.get(cpu) else { return };
        let event = {
            let custody_slot = slot.lock();
            let Some(custody) = custody_slot.as_ref() else {
                return;
            };
            if custody.token.is_none() {
                // A previous stop has already terminated hardware; do not
                // turn an empty retained custody into a false running fault.
                return;
            }
            if custody.cookie != sample.cookie {
                custody.event.state.lock().failed = true;
                None
            } else {
                // A temporary clone is safe in IRQ: the static custody keeps
                // a strong owner until scheduler leave performs the final drop.
                Some(custody.event.clone())
            }
        };
        let Some(event) = event else {
            Self::stop_current();
            return;
        };
        let mut state = event.state.lock();
        if !state.enabled || state.closed || state.failed || state.ring.is_none() {
            drop(state);
            Self::stop_current();
            return;
        }
        let mut bytes = [0_u8; 40];
        let size = encode_sample(
            &mut bytes,
            event.config.sample_type,
            frame.rip,
            frame.cs,
            axhal::time::monotonic_time_nanos(),
            cpu as u32,
            event.config.period,
        );
        state.value = state.value.saturating_add(event.config.period);
        let published = publish_record(
            state.ring.as_mut().unwrap(),
            &bytes[..size],
            event.config.id,
        );
        if published.overflow {
            state.failed = true;
        }
        // The state lock spans the rearm decision: close/disable can neither
        // win after the period was accounted nor leave a live counter behind.
        let rearm = !state.failed && state.enabled && !state.closed && state.ring.is_some();
        let wake_needed = published.published;
        drop(state);
        if wake_needed {
            event.waiters.wake();
        }
        if rearm {
            if axhal::pmu::sampling_rearm_local(sample.cookie, generation).is_ok() {
                return;
            }
            event.state.lock().failed = true;
        }
        Self::stop_current_after_pmi();
    }

    pub(crate) fn init_irq() -> bool {
        axhal::irq::register_context(axhal::pmu::SAMPLING_IRQ_VECTOR, perf_sampling_pmi)
    }

    fn install_ring(&self, request: FileMmapRequest) -> AxResult {
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
        if let Some(ring) = self.state.lock().ring.as_ref() {
            return if ring.data_size == data_size {
                Ok(())
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
        // These words are immutable geometry, but atomics avoid introducing a
        // second ordinary-byte writer into a mapping shared with userspace.
        view.atomic_u64(DATA_OFFSET)?.store_release(PAGE as u64);
        view.atomic_u64(DATA_SIZE)?.store_release(data_size as u64);
        let region = FixedSharedMmapRegion::try_new(
            0,
            pages,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
        )?;
        let mut state = self.state.lock();
        if let Some(ring) = state.ring.as_ref() {
            return if ring.data_size == data_size {
                Ok(())
            } else {
                Err(AxError::ResourceBusy)
            };
        }
        state.ring = Some(Ring {
            region,
            view,
            head,
            tail,
            data_size,
            producer_head: 0,
            lost: 0,
        });
        Ok(())
    }

    fn read_count(&self, dst: &mut IoDst) -> AxResult<usize> {
        if !self.target_current() {
            return Err(AxError::OperationNotSupported);
        }
        // Reading an active event settles its residual counter before taking
        // the snapshot.  Custody keeps the backing alive while IRQs are off.
        let active = self.live();
        if active {
            Self::stop_current();
        }
        let state = self.state.lock();
        if state.failed {
            // stop_current above consumed the token.  Do not rearm a sticky
            // hardware failure; leave later releases the retained custody Arc.
            return Err(AxError::Io);
        }
        let now = axhal::time::monotonic_time_nanos();
        let value = state.value;
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
        if active {
            current().as_thread().reconcile_perf_sampling();
        }
        let words = 1
            + usize::from(self.config.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0)
            + usize::from(self.config.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0)
            + usize::from(self.config.read_format & PERF_FORMAT_ID != 0);
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
        Ok(words * 8)
    }
}

fn perf_sampling_pmi(_: usize, frame: &axcpu::TrapFrame) {
    PerfSamplingFile::handle_pmi(frame);
}

impl FileLike for PerfSamplingFile {
    fn final_close(&self) {
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
        }
        self.waiters.wake();
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
    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        self.install_ring(request)?;
        // mmap is the first point at which this event has a producer backing.
        current().as_thread().reconcile_perf_sampling();
        Ok(self
            .state
            .lock()
            .ring
            .as_ref()
            .map(|ring| ring.region.prepare(request))
            .transpose()?
            .flatten())
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        if !self.target_current() {
            return Err(AxError::OperationNotSupported);
        }
        if cmd == PERF_EVENT_IOC_ID {
            context
                .user_memory()
                .write_value(arg as *mut u64, self.config.id)
                .map_err(crate::mm::map_usercopy_error)?;
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
        if matches!(cmd, PERF_EVENT_IOC_DISABLE | PERF_EVENT_IOC_RESET) {
            Self::stop_current();
        }
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
                state.failed = false;
                if let Some(ring) = state.ring.as_mut() {
                    ring.lost = 0;
                }
            }
            _ => unreachable!("validated perf sampling ioctl"),
        }
        drop(state);
        if cmd != PERF_EVENT_IOC_DISABLE {
            current().as_thread().reconcile_perf_sampling();
        }
        Ok(0)
    }
    fn nonblocking(&self) -> bool {
        false
    }
    fn set_nonblocking(&self, _: bool) -> AxResult {
        Ok(())
    }
    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[perf_event]".into())
    }
}

impl Pollable for PerfSamplingFile {
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

fn header(out: &mut [u8], kind: u32, misc: u16, size: usize) {
    out[..4].copy_from_slice(&kind.to_ne_bytes());
    out[4..6].copy_from_slice(&misc.to_ne_bytes());
    out[6..8].copy_from_slice(&(size as u16).to_ne_bytes());
}
fn push_u64(out: &mut [u8], cursor: &mut usize, value: u64) {
    out[*cursor..*cursor + 8].copy_from_slice(&value.to_ne_bytes());
    *cursor += 8;
}
fn encode_sample(
    out: &mut [u8; 40],
    types: u64,
    ip: u64,
    cs: u64,
    time: u64,
    cpu: u32,
    period: u64,
) -> usize {
    let mut cursor = 8;
    if types & PERF_SAMPLE_IP != 0 {
        push_u64(out, &mut cursor, ip);
    }
    if types & PERF_SAMPLE_TIME != 0 {
        push_u64(out, &mut cursor, time);
    }
    if types & PERF_SAMPLE_CPU != 0 {
        out[cursor..cursor + 4].copy_from_slice(&cpu.to_ne_bytes());
        cursor += 8;
    }
    if types & PERF_SAMPLE_PERIOD != 0 {
        push_u64(out, &mut cursor, period);
    }
    header(
        out,
        PERF_RECORD_SAMPLE,
        if cs & 3 == 3 {
            PERF_RECORD_MISC_USER
        } else {
            PERF_RECORD_MISC_KERNEL
        },
        cursor,
    );
    cursor
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

fn publish_record(ring: &mut Ring, sample: &[u8], id: u64) -> PublishResult {
    let tail = ring.tail.load_acquire();
    let Some(mut used) = producer_window(ring.producer_head, tail, ring.data_size) else {
        ring.lost = ring.lost.saturating_add(1);
        return PublishResult::default();
    };
    let mut lost = [0_u8; 24];
    header(&mut lost, PERF_RECORD_LOST, 0, 24);
    lost[8..16].copy_from_slice(&id.to_ne_bytes());
    let pending = ring.lost;
    let mut published_any = false;
    lost[16..24].copy_from_slice(&pending.to_ne_bytes());
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
fn write_record(ring: &Ring, head: u64, bytes: &[u8]) -> AxResult {
    let offset = (head as usize) & (ring.data_size - 1);
    // SAFETY: `PerfSamplingFile::state` serializes this sole producer; the
    // acquire tail / release head protocol above proves these bytes are not
    // consumer-owned, and `bytes.len() <= data_size` is fixed by record ABI.
    unsafe { ring.view.write_wrapped(PAGE, ring.data_size, offset, bytes) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sample_payload_order_and_misc() {
        let mut out = [0; 40];
        let n = encode_sample(
            &mut out,
            PERF_SAMPLE_IP | PERF_SAMPLE_TIME | PERF_SAMPLE_CPU | PERF_SAMPLE_PERIOD,
            1,
            3,
            2,
            4,
            5,
        );
        assert_eq!(n, 40);
        assert_eq!(
            u32::from_ne_bytes(out[..4].try_into().unwrap()),
            PERF_RECORD_SAMPLE
        );
        assert_eq!(
            u16::from_ne_bytes(out[4..6].try_into().unwrap()),
            PERF_RECORD_MISC_USER
        );
        assert_eq!(u64::from_ne_bytes(out[8..16].try_into().unwrap()), 1);
        assert_eq!(u64::from_ne_bytes(out[16..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_ne_bytes(out[24..28].try_into().unwrap()), 4);
        assert_eq!(u64::from_ne_bytes(out[32..40].try_into().unwrap()), 5);
    }
    #[test]
    fn kernel_sample_uses_kernel_misc() {
        let mut out = [0; 40];
        encode_sample(&mut out, PERF_SAMPLE_IP, 1, 0, 0, 0, 0);
        assert_eq!(
            u16::from_ne_bytes(out[4..6].try_into().unwrap()),
            PERF_RECORD_MISC_KERNEL
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
            assert_eq!(encode_sample(&mut out, bit, 0, 0, 0, 0, 0), 16);
        }
    }
}
