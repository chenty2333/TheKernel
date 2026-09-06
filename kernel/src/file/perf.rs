//! perf-event descriptors and their IRQ-safe task-group runtime.
//!
//! A group, rather than an individual descriptor, is the scheduler unit. The
//! switch callbacks only take `SpinNoIrq` locks and never allocate or copy to
//! userspace.

use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::{MaybeUninit, size_of},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axhal::time::monotonic_time_nanos;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::spin::SpinNoIrq;
use axtask::current;
use thekernel_linux_perf::{
    PERF_EVENT_IOC_DISABLE, PERF_EVENT_IOC_ENABLE, PERF_EVENT_IOC_ID,
    PERF_EVENT_IOC_MODIFY_ATTRIBUTES, PERF_EVENT_IOC_QUERY_BPF, PERF_EVENT_IOC_REFRESH,
    PERF_EVENT_IOC_RESET, PERF_EVENT_IOC_SET_BPF, PERF_EVENT_IOC_SET_OUTPUT, PERF_IOC_FLAG_GROUP,
    ReadPlan,
};

use crate::{
    file::{FileLike, IoDst, IoSrc, IoctlContext, Kstat, anon_inode_stat},
    mm::map_usercopy_error,
    task::AsThread,
};

pub(crate) const MAX_GROUP_MEMBERS: usize = 64;
pub(crate) const MAX_GROUPS_PER_THREAD: usize = 64;
const RECONCILE_ACK_SPINS: usize = 100_000;
static NEXT_RECONCILE_GENERATION: AtomicU64 = AtomicU64::new(1);

/// A fixed CPU-local handoff for the typed PerfReconcile IPI lane.  The raw
/// group pointer is borrowed only while the initiating task holds an Arc to
/// that group; the destination never upgrades or drops an Arc.
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
#[repr(align(64))]
struct PerfReconcileMailbox {
    group: AtomicUsize,
    kind: core::sync::atomic::AtomicU8,
    task_id: AtomicU64,
    member_id: AtomicU64,
    control: core::sync::atomic::AtomicU8,
    cancelled: AtomicBool,
    result: AtomicUsize,
    desired: AtomicU64,
    acknowledged: AtomicU64,
    completion_len: AtomicUsize,
    completion_cookie: [AtomicU64; axhal::pmu::MAX_COUNTING_GROUP],
    completion_delta: [AtomicU64; axhal::pmu::MAX_COUNTING_GROUP],
    completion_overflow: [AtomicU64; axhal::pmu::MAX_COUNTING_GROUP],
}

#[cfg(all(feature = "perf-sampling", target_os = "none"))]
impl PerfReconcileMailbox {
    const fn new() -> Self {
        Self {
            group: AtomicUsize::new(0),
            kind: core::sync::atomic::AtomicU8::new(RECONCILE_GROUP),
            task_id: AtomicU64::new(0),
            member_id: AtomicU64::new(0),
            control: core::sync::atomic::AtomicU8::new(RECONCILE_CONTROL_READ),
            cancelled: AtomicBool::new(false),
            result: AtomicUsize::new(RECONCILE_RESULT_PENDING),
            desired: AtomicU64::new(0),
            acknowledged: AtomicU64::new(0),
            completion_len: AtomicUsize::new(0),
            completion_cookie: [const { AtomicU64::new(0) }; axhal::pmu::MAX_COUNTING_GROUP],
            completion_delta: [const { AtomicU64::new(0) }; axhal::pmu::MAX_COUNTING_GROUP],
            completion_overflow: [const { AtomicU64::new(0) }; axhal::pmu::MAX_COUNTING_GROUP],
        }
    }

    fn try_publish(&self, group: *const PerfGroup, generation: u64) -> bool {
        if self
            .group
            .compare_exchange(
                0,
                RECONCILE_HANDLER_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.desired.store(generation, Ordering::Release);
        self.kind.store(RECONCILE_GROUP, Ordering::Release);
        self.completion_len.store(0, Ordering::Relaxed);
        self.group.store(group as usize, Ordering::Release);
        true
    }

    fn try_publish_sampling(
        &self,
        event: *const crate::file::PerfSampleBackend,
        generation: u64,
    ) -> bool {
        if self
            .group
            .compare_exchange(
                0,
                RECONCILE_HANDLER_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.desired.store(generation, Ordering::Release);
        self.kind.store(RECONCILE_SAMPLING, Ordering::Release);
        self.completion_len.store(0, Ordering::Relaxed);
        self.group.store(event as usize, Ordering::Release);
        true
    }

    fn try_publish_cgroup_membership(&self, task_id: u64, generation: u64) -> bool {
        if self
            .group
            .compare_exchange(
                0,
                RECONCILE_HANDLER_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.desired.store(generation, Ordering::Release);
        self.task_id.store(task_id, Ordering::Release);
        self.kind.store(RECONCILE_CGROUP, Ordering::Release);
        self.group.store(RECONCILE_CGROUP_MARKER, Ordering::Release);
        true
    }

    /// Transfer one explicit group Arc to the owner CPU for a synchronous
    /// read/control boundary.  All operands are POD so the IPI path neither
    /// allocates nor follows user memory.
    fn try_publish_control(
        &self,
        group: *const PerfGroup,
        generation: u64,
        member_id: u64,
        group_control: bool,
        control: u8,
    ) -> bool {
        if self
            .group
            .compare_exchange(
                0,
                RECONCILE_HANDLER_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.desired.store(generation, Ordering::Release);
        self.member_id.store(member_id, Ordering::Release);
        self.control.store(
            control
                | if group_control {
                    RECONCILE_CONTROL_GROUP
                } else {
                    0
                },
            Ordering::Release,
        );
        self.cancelled.store(false, Ordering::Release);
        self.result
            .store(RECONCILE_RESULT_PENDING, Ordering::Release);
        self.kind.store(RECONCILE_CONTROL, Ordering::Release);
        self.completion_len.store(0, Ordering::Relaxed);
        self.group.store(group as usize, Ordering::Release);
        true
    }

    fn publish_completions(&self, completions: &[axhal::pmu::CountingCompletion]) {
        for (index, completion) in completions.iter().enumerate() {
            self.completion_cookie[index].store(completion.cookie, Ordering::Relaxed);
            self.completion_delta[index].store(completion.delta, Ordering::Relaxed);
            self.completion_overflow[index].store(completion.overflowed as u64, Ordering::Relaxed);
        }
        self.completion_len
            .store(completions.len(), Ordering::Release);
    }

    fn acknowledge_if_current(&self, generation: u64) -> bool {
        if self.desired.load(Ordering::Acquire) != generation {
            return false;
        }
        self.acknowledged.store(generation, Ordering::Release);
        true
    }

    fn acknowledge_result_if_current(&self, generation: u64, result: usize) -> bool {
        if self.desired.load(Ordering::Acquire) != generation {
            return false;
        }
        self.result.store(result, Ordering::Release);
        self.acknowledged.store(generation, Ordering::Release);
        true
    }
}

#[cfg(all(feature = "perf-sampling", target_os = "none"))]
static PERF_RECONCILE_MAILBOXES: [PerfReconcileMailbox; axconfig::plat::MAX_CPU_NUM] =
    [const { PerfReconcileMailbox::new() }; axconfig::plat::MAX_CPU_NUM];
static PERF_RECONCILE_RETIRED: AtomicUsize = AtomicUsize::new(0);
const RECONCILE_HANDLER_BUSY: usize = 1;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_GROUP: u8 = 1;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_SAMPLING: u8 = 2;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_CGROUP: u8 = 3;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_CONTROL: u8 = 4;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_CGROUP_MARKER: usize = 2;
const RECONCILE_CONTROL_READ: u8 = 0;
const RECONCILE_CONTROL_ENABLE: u8 = 1;
const RECONCILE_CONTROL_DISABLE: u8 = 2;
const RECONCILE_CONTROL_RESET: u8 = 3;
const RECONCILE_CONTROL_RETIRE: u8 = 4;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_CONTROL_GROUP: u8 = 0x80;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_CONTROL_MASK: u8 = !RECONCILE_CONTROL_GROUP;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_RESULT_PENDING: usize = 0;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_RESULT_OK: usize = 1;
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
const RECONCILE_RESULT_FAILED: usize = 2;

/// Installs the allocation-free remote perf reconciliation IPI consumer.
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
pub(crate) fn init_reconcile_ipi() -> bool {
    axhal::irq::register_ipi_reason(
        axhal::irq::IpiReason::PerfReconcile,
        perf_reconcile_ipi_handler,
    )
}

/// Synchronously stop a sampling placement from descriptor lifecycle paths.
/// The caller retains `event` until the exact generation is acknowledged; on
/// timeout the extra strong reference intentionally remains with the mailbox
/// until the late IPI retires it, avoiding a use-after-free.
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
pub(crate) fn reconcile_sampling_last(event: &Arc<crate::file::PerfSampleBackend>) {
    let Some(cpu) = event.active_cpu() else {
        return;
    };
    if cpu == axhal::percpu::this_cpu_id() {
        event.reconcile_local_stop();
        return;
    }
    let generation = NEXT_RECONCILE_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .max(1);
    let Some(mailbox) = PERF_RECONCILE_MAILBOXES.get(cpu) else {
        event.fail_closed();
        return;
    };
    unsafe { Arc::increment_strong_count(Arc::as_ptr(event)) };
    if !mailbox.try_publish_sampling(Arc::as_ptr(event), generation) {
        unsafe { drop(Arc::from_raw(Arc::as_ptr(event))) };
        event.fail_closed();
        return;
    }
    if axhal::irq::send_ipi_reason(
        axhal::irq::IpiReason::PerfReconcile,
        axhal::irq::IpiTarget::Other { cpu_id: cpu },
    )
    .is_ok()
    {
        for _ in 0..RECONCILE_ACK_SPINS {
            if mailbox.acknowledged.load(Ordering::Acquire) == generation {
                return;
            }
            core::hint::spin_loop();
        }
    } else if mailbox
        .group
        .compare_exchange(
            Arc::as_ptr(event) as usize,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        unsafe { drop(Arc::from_raw(Arc::as_ptr(event))) };
    }
    event.fail_closed();
}

/// Stops only the exact generation published for this CPU.  This path never
/// allocates, usercopies, upgrades/drops Arc ownership, or waits.  The group
/// lock is retained for now because the existing counter leases are still
/// represented by the pre-unified HAL object; it serializes hardware finish
/// with scheduler leave and is never acquired across CPUs.
#[cfg(all(feature = "perf-sampling", target_os = "none"))]
fn perf_reconcile_ipi_handler() {
    let cpu = axhal::percpu::this_cpu_id();
    // Package uncore requests use the same allocation-free IPI reason but a
    // distinct platform mailbox.  A missing/stale request is harmless.
    let uncore_generation = axhal::perf_uncore::reconcile_generation(cpu);
    if uncore_generation != 0 {
        let _ = axhal::perf_uncore::reconcile_owner_current(uncore_generation);
    }
    let Some(mailbox) = PERF_RECONCILE_MAILBOXES.get(cpu) else {
        return;
    };
    let generation = mailbox.desired.load(Ordering::Acquire);
    let kind = mailbox.kind.load(Ordering::Acquire);
    let address = mailbox.group.load(Ordering::Acquire);
    if generation == 0 || address <= RECONCILE_HANDLER_BUSY {
        return;
    }
    if mailbox
        .group
        .compare_exchange(
            address,
            RECONCILE_HANDLER_BUSY,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    if kind == RECONCILE_SAMPLING {
        // The publisher transfers one explicit Arc count.  The NMI/IPI path
        // stops exactly this event and moves that count to the existing
        // task-context retire queue; it never drops user mappings here.
        unsafe {
            crate::file::PerfSampleBackend::reconcile_ipi_stop(address as *const _);
        }
        let _ = mailbox.acknowledge_if_current(generation);
        mailbox.group.store(0, Ordering::Release);
        crate::deferred_work::wake_policy_worker();
        return;
    }
    if kind == RECONCILE_CGROUP {
        let task_id = mailbox.task_id.load(Ordering::Acquire);
        // A cgroup move can race a task switch.  Only reconcile if this CPU
        // still executes the exact task whose membership was committed; the
        // switch path has already accounted and re-filtered every other case.
        if current().id().as_u64() == task_id {
            PerfGroup::cpu_context_membership_changed_local(cpu, task_id);
        }
        let _ = mailbox.acknowledge_if_current(generation);
        mailbox.group.store(0, Ordering::Release);
        return;
    }
    if kind == RECONCILE_CONTROL {
        let group = address as *const PerfGroup;
        let member_id = mailbox.member_id.load(Ordering::Acquire);
        let control = mailbox.control.load(Ordering::Acquire);
        // SAFETY: publication transferred one strong group reference and the
        // publisher retains its own reference through the exact-generation
        // acknowledgement.  This routine only touches CPU-local hardware,
        // performs no usercopy/allocation, and defers the raw Arc custody.
        let succeeded = unsafe {
            if mailbox.cancelled.load(Ordering::Acquire) {
                (&*group).reconcile_cancel_local(generation);
                false
            } else {
                (&*group).reconcile_control_local(
                    generation,
                    member_id,
                    control & RECONCILE_CONTROL_GROUP != 0,
                    control & RECONCILE_CONTROL_MASK,
                    Some(&mailbox.cancelled),
                )
            }
        };
        unsafe { (&*group).defer_reconcile_custody() };
        let _ = mailbox.acknowledge_result_if_current(
            generation,
            if succeeded {
                RECONCILE_RESULT_OK
            } else {
                RECONCILE_RESULT_FAILED
            },
        );
        mailbox.group.store(0, Ordering::Release);
        crate::deferred_work::wake_policy_worker();
        return;
    }
    if kind != RECONCILE_GROUP {
        mailbox.group.store(0, Ordering::Release);
        return;
    }
    let group = address as *const PerfGroup;
    // Sampling tokens are not represented by the counting completion array.
    // Stop every custody carrying this exact group generation before the
    // publisher clears its active file list; this is the remote, immediate
    // lifecycle boundary for NMI PMCs.
    crate::file::PerfSampleBackend::reconcile_group_generation_local(generation);
    let mut completions = [axhal::pmu::CountingCompletion {
        cookie: 0,
        generation: 0,
        delta: 0,
        overflowed: false,
    }; axhal::pmu::MAX_COUNTING_GROUP];
    let count = axhal::pmu::counting_stop_settle_current(generation)
        .and_then(|_| axhal::pmu::counting_copy_completion_current(generation, &mut completions))
        .and_then(|count| axhal::pmu::counting_release_completed_current(generation).map(|_| count))
        .unwrap_or(0);
    mailbox.publish_completions(&completions[..count]);
    // SAFETY: the publisher retains a strong group reference until it either
    // observes this acknowledgement or takes the fail-closed timeout path.
    // The mailbox is CPU-private for consumption and generation rejects an
    // older kick after migration/replacement.
    // The publisher installed one explicit Arc strong-count before publishing
    // this raw pointer.  An interrupt may not perform that Arc drop, so move
    // the raw custody onto a lock-free retire list for a later task context.
    unsafe { (&*group).defer_reconcile_custody() };
    let _ = mailbox.acknowledge_if_current(generation);
    mailbox.group.store(0, Ordering::Release);
    crate::deferred_work::wake_policy_worker();
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub(crate) enum SoftwareEvent {
    CpuClock,
    TaskClock,
    PageFaults,
    PageFaultsMin,
    PageFaultsMaj,
    ContextSwitches,
    CpuMigrations,
    AlignmentFaults,
    EmulationFaults,
    Dummy,
    CgroupSwitches,
    BpfOutput,
}
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub(crate) enum HardwareEvent {
    Cycles,
    Instructions,
    Architectural {
        event_select: u64,
        availability_bit: u8,
    },
}
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub(crate) enum PerfEvent {
    Software(SoftwareEvent),
    Hardware(HardwareEvent),
    /// A tracepoint has a stable tracefs ID.  The source registry owns the
    /// name/format mapping; a group only matches this numeric identity on its
    /// preallocated active snapshot.
    Tracepoint(u64),
    /// A raw PMU encoding. Admission is tied to an actual PMU registry entry;
    /// it is never translated between hybrid core types.
    Raw {
        config: u64,
        core_type: u8,
        precise: bool,
        branch_stack: bool,
    },
    /// PerfMon-Discovery package counter. It is never passed to the core
    /// counter solver; the owner CPU performs its bounded transport access.
    Uncore {
        box_type: u16,
        box_id: u16,
        config: u64,
    },
    /// Whitelisted read-only MSR/RAPL/residency source.
    ReadOnly {
        pmu: axhal::perf_uncore::ReadOnlyPmu,
        config: u64,
    },
    /// x86 DR0--DR3 watchpoint selected for this task context.
    Breakpoint {
        addr: u64,
        len: u64,
        ty: u32,
    },
    /// Kernel-text probe location after its instruction-patch registration.
    Kprobe {
        addr: u64,
        retprobe: bool,
        query_offset: u64,
    },
    /// Executable inode identity plus file-relative probe offset.
    Uprobe {
        mount_id: u64,
        device: u64,
        inode: u64,
        offset: u64,
        retprobe: bool,
        /// File-relative USDT semaphore offset from perf attr.config[63:32].
        /// Zero means that this probe has no reference counter.
        reference_counter_offset: u64,
    },
}

/// Stable result returned while a queried perf descriptor remains pinned.
/// Probe names which are not representable by the current source registry are
/// absent; address-based kprobes deliberately use `probe_addr` instead.
#[cfg(feature = "bpf")]
pub(crate) struct PerfBpfTaskFdQuery {
    pub(crate) prog_id: u32,
    pub(crate) fd_type: u32,
    pub(crate) name: Option<PerfBpfTaskFdQueryName>,
    pub(crate) probe_offset: u64,
    pub(crate) probe_addr: u64,
}

#[cfg(feature = "bpf")]
pub(crate) enum PerfBpfTaskFdQueryName {
    Static(&'static [u8]),
    Owned(Arc<Vec<u8>>),
}

#[cfg(feature = "bpf")]
impl PerfBpfTaskFdQueryName {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Static(name) => name,
            Self::Owned(name) => name.as_slice(),
        }
    }
}

const fn dynamic_event_key(event: PerfEvent) -> u64 {
    match event {
        PerfEvent::Raw { config, .. } => config,
        PerfEvent::Uncore { config, .. } | PerfEvent::ReadOnly { config, .. } => config,
        PerfEvent::Breakpoint { addr, .. } | PerfEvent::Kprobe { addr, .. } => addr,
        PerfEvent::Uprobe {
            mount_id,
            device,
            inode,
            offset,
            retprobe,
            ..
        } => {
            mount_id
                ^ device.rotate_left(11)
                ^ inode.rotate_left(29)
                ^ offset.rotate_left(17)
                ^ ((retprobe as u64) << 63)
        }
        PerfEvent::Software(_) | PerfEvent::Hardware(_) | PerfEvent::Tracepoint(_) => 0,
    }
}

/// Equality of the producer-visible source.  Query-only probe metadata must
/// not split delivery: symbol offsets and USDT semaphore offsets describe how
/// a descriptor was opened, while the trap is identified by its resolved
/// instruction location and entry/return edge.
fn same_dynamic_source(left: PerfEvent, right: PerfEvent) -> bool {
    match (left, right) {
        (
            PerfEvent::Kprobe {
                addr: left,
                retprobe: left_ret,
                ..
            },
            PerfEvent::Kprobe {
                addr: right,
                retprobe: right_ret,
                ..
            },
        ) => left == right && left_ret == right_ret,
        (
            PerfEvent::Uprobe {
                mount_id: left_mount,
                device: left_device,
                inode: left_inode,
                offset: left_offset,
                retprobe: left_ret,
                ..
            },
            PerfEvent::Uprobe {
                mount_id: right_mount,
                device: right_device,
                inode: right_inode,
                offset: right_offset,
                retprobe: right_ret,
                ..
            },
        ) => {
            left_mount == right_mount
                && left_device == right_device
                && left_inode == right_inode
                && left_offset == right_offset
                && left_ret == right_ret
        }
        _ => left == right,
    }
}

/// The owner selected by `perf_event_open`.  The current implementation only
/// attaches task contexts to the scheduler, but retaining the target shape in
/// the group makes a CPU or task-on-CPU backend explicit rather than silently
/// treating it as a task event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PerfContext {
    Task { task_id: u64 },
    TaskOnCpu { task_id: u64, cpu: usize },
    Cpu { cpu: usize },
    Cgroup { cgroup_id: u64, cpu: usize },
}

impl PerfContext {
    const fn task_id(self) -> Option<u64> {
        match self {
            Self::Task { task_id } | Self::TaskOnCpu { task_id, .. } => Some(task_id),
            Self::Cpu { .. } | Self::Cgroup { .. } => None,
        }
    }

    const fn cpu(self) -> Option<usize> {
        match self {
            Self::Task { .. } => None,
            Self::TaskOnCpu { cpu, .. } | Self::Cpu { cpu } | Self::Cgroup { cpu, .. } => Some(cpu),
        }
    }
}

/// Pure per-CPU placement model used to keep the hardware adapter honest.
/// A group is either wholly placed or wholly absent.  `exclusive` consumes
/// the PMU for its interval, while pinned groups may not enter the flexible
/// rotation when there are insufficient counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SolverGroup {
    id: u64,
    fixed: u8,
    programmable: u8,
    pinned: bool,
    exclusive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SolverCapacity {
    fixed: u8,
    programmable: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SolverResult {
    Placed,
    Flexible,
    Rejected,
}

fn solve_group(
    capacity: SolverCapacity,
    group: SolverGroup,
    occupied: Option<SolverGroup>,
) -> SolverResult {
    if group.fixed > capacity.fixed || group.programmable > capacity.programmable {
        return SolverResult::Rejected;
    }
    if let Some(other) = occupied {
        if other.exclusive || group.exclusive {
            return if group.pinned {
                SolverResult::Rejected
            } else {
                SolverResult::Flexible
            };
        }
        if group.fixed.saturating_add(other.fixed) > capacity.fixed
            || group.programmable.saturating_add(other.programmable) > capacity.programmable
        {
            return if group.pinned {
                SolverResult::Rejected
            } else {
                SolverResult::Flexible
            };
        }
    }
    SolverResult::Placed
}

/// A deterministic round-robin selector for flexible *whole groups*.  The
/// switch is the accounting boundary: callers settle the old group before
/// advancing `cursor`, so enabled time continues while running time advances
/// only for the selected group.
fn next_flexible_group(groups: &[SolverGroup], cursor: &mut usize) -> Option<SolverGroup> {
    if groups.is_empty() {
        return None;
    }
    let selected = groups[*cursor % groups.len()];
    *cursor = cursor.wrapping_add(1) % groups.len();
    Some(selected)
}

/// Constraints which are not expressible as a simple counter count.  Kept as
/// a pure solver input so placement can be decided before either CPU touches
/// an MSR.  Every group is still accepted or rejected as a unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExtendedSolverConstraints {
    pebs_counter_mask: u64,
    needs_lbr: bool,
    offcore_slots: u8,
    needs_topdown: bool,
    smt_shared_slots: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExtendedSolverCapacity {
    pebs_counter_mask: u64,
    offcore_slots: u8,
    topdown_slots: u8,
    smt_shared_slots: u8,
}

/// Apply singleton/mask/shared-resource rules after ordinary counter capacity
/// has admitted the group.  The caller may turn a flexible overcommit into a
/// rotation; pinned groups are rejected rather than partially programmed.
fn solve_extended_constraints(
    capacity: ExtendedSolverCapacity,
    request: ExtendedSolverConstraints,
    occupied: Option<ExtendedSolverConstraints>,
    pinned: bool,
) -> SolverResult {
    let base_invalid = request.pebs_counter_mask & !capacity.pebs_counter_mask != 0
        || request.offcore_slots > capacity.offcore_slots
        || (request.needs_topdown && capacity.topdown_slots == 0)
        || request.smt_shared_slots > capacity.smt_shared_slots;
    if base_invalid {
        return SolverResult::Rejected;
    }
    let Some(other) = occupied else {
        return SolverResult::Placed;
    };
    let conflict = (request.needs_lbr && other.needs_lbr)
        || request.offcore_slots.saturating_add(other.offcore_slots) > capacity.offcore_slots
        || (request.needs_topdown && other.needs_topdown)
        || request
            .smt_shared_slots
            .saturating_add(other.smt_shared_slots)
            > capacity.smt_shared_slots;
    if !conflict {
        SolverResult::Placed
    } else if pinned {
        SolverResult::Rejected
    } else {
        SolverResult::Flexible
    }
}

/// Raw encodings are CPU-type-local by definition.  Generic aliases may be
/// expanded only after the registry has supplied an explicit semantic-equality
/// proof for both hybrid core types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HybridEventAdmission {
    Raw {
        core_type: u8,
    },
    Generic {
        semantic_on_core: bool,
        semantic_on_atom: bool,
    },
}

pub(crate) fn admit_hybrid_event(event: HybridEventAdmission, destination_core_type: u8) -> bool {
    match event {
        HybridEventAdmission::Raw { core_type } => core_type == destination_core_type,
        HybridEventAdmission::Generic {
            semantic_on_core,
            semantic_on_atom,
        } => semantic_on_core && semantic_on_atom,
    }
}
impl PerfEvent {
    fn hardware(self) -> Option<HardwareEvent> {
        if let Self::Hardware(event) = self {
            Some(event)
        } else {
            None
        }
    }
    const fn uses_pmu(self) -> bool {
        matches!(
            self,
            Self::Hardware(_) | Self::Raw { .. } | Self::Uncore { .. } | Self::ReadOnly { .. }
        )
    }
    const fn uses_external_counter(self) -> bool {
        matches!(self, Self::Uncore { .. } | Self::ReadOnly { .. })
    }
    #[cfg(feature = "pmu")]
    fn counting_constraints(self) -> axhal::pmu::CountingConstraints {
        match self {
            Self::Raw {
                precise,
                branch_stack,
                ..
            } => axhal::pmu::CountingConstraints {
                // The platform intersects this with its per-core discovered
                // PEBS mask; all ones here means the event needs any real
                // PEBS-capable programmable counter, not a guessed index.
                pebs_counter_mask: if precise { u64::MAX } else { 0 },
                needs_lbr: branch_stack,
                offcore_slots: 0,
                needs_topdown: false,
                smt_shared_slots: 0,
            },
            _ => axhal::pmu::CountingConstraints::default(),
        }
    }
}

/// Combines a completed interval with a non-destructive local PMU sample.
/// Hardware values are never stored here; `stop_locked` performs that single
/// settlement when it terminates the lease.
fn compose_live_count(settled: u64, live: u64) -> u64 {
    settled.saturating_add(live)
}

struct Member {
    file: Weak<PerfEventFile>,
    /// Inherited child events intentionally have no numeric descriptor in the
    /// child.  The group owns their file object (which itself only weakly
    /// references the group), avoiding both a leak and a second FD policy.
    inherited: Option<Arc<PerfEventFile>>,
    dead: bool,
}

struct InheritedMemberSpec {
    event: PerfEvent,
    disabled: bool,
    read: ReadPlan,
    lifecycle: thekernel_linux_perf::PerfLifecycle,
    placement: PerfPlacementPolicy,
    count_user: bool,
    count_kernel: bool,
    #[cfg(feature = "perf-sampling")]
    sampling: Option<Arc<crate::file::PerfSampleBackend>>,
}

struct ActiveGroup {
    // Temporary strong custody makes an FD close racing with a switch safe.
    // Files retain only a Weak group pointer, so this is not a cycle.
    files: Vec<Option<Arc<PerfEventFile>>>,
    #[cfg(feature = "pmu")]
    placement: Option<axhal::pmu::CountingPlacement>,
    task_active: bool,
    running: bool,
    /// The CPU which owns the current placement.  This is deliberately
    /// separate from the task id: a future remote reconcile must compare the
    /// generation as well as this CPU before touching a counter lease.
    cpu: Option<usize>,
    /// Incremented for every placement and synchronous teardown.  Consumers
    /// may acknowledge only the exact generation they observed; an old IPI
    /// can consequently never stop a newer placement after migration.
    generation: u64,
    /// A last-descriptor close has frozen this placement until the owning CPU
    /// settles it. This prevents a concurrent control operation from placing
    /// a new counter lease behind a close request.
    reconcile_frozen: bool,
    /// Hardware sampling ownership is per PMC.  A group is admitted only
    /// when all of its sampling members own one entry; flexible placement
    /// rotates the complete set at timer boundaries.
    #[cfg(feature = "perf-sampling")]
    sampling_cursor: usize,
    #[cfg(feature = "perf-sampling")]
    sampling_active: [Option<usize>; axhal::pmu::MAX_COUNTING_GROUP],
}
impl ActiveGroup {
    const fn new() -> Self {
        Self {
            files: Vec::new(),
            #[cfg(feature = "pmu")]
            placement: None,
            task_active: false,
            running: false,
            cpu: None,
            generation: 0,
            reconcile_frozen: false,
            #[cfg(feature = "perf-sampling")]
            sampling_cursor: 0,
            sampling_active: [None; axhal::pmu::MAX_COUNTING_GROUP],
        }
    }
}
struct GroupState {
    members: Vec<Member>,
    active: ActiveGroup,
}

pub(crate) struct PerfGroup {
    context: PerfContext,
    target_task_id: u64,
    leader_id: u64,
    state: SpinNoIrq<GroupState>,
    /// Published without taking the group lock so an IPI consumer can reject
    /// a stale mailbox before it attempts its bounded local reconciliation.
    generation: AtomicU64,
    /// Intrusive link for a raw Arc custody which an IPI has completed but is
    /// not permitted to drop. At most one close can freeze a group at once.
    retire_next: AtomicUsize,
}

// CPU-context registration stores groups in a static spin-locked table while
// files weakly name their group and may strongly own a sampling backend. That
// is a valid ownership graph but it is cyclic for auto-trait evaluation.
// Group mutation is exclusively through `state` or atomics, so the static
// registry may safely share groups across CPUs.
unsafe impl Send for PerfGroup {}
unsafe impl Sync for PerfGroup {}

/// CPU/system-wide contexts are scheduled at every task switch on their
/// target CPU.  The registry is their scheduler owner: event files point back
/// to a group weakly, so storing another weak reference here would destroy a
/// newly opened system-wide group as soon as `perf_event_open` returned.
///
/// The number is deliberately bounded at open time.  Switch, fault, trace and
/// timer paths can consequently iterate the already-reserved vector in place;
/// none of them allocates an `Arc` snapshot while IRQs are disabled.
const MAX_CPU_CONTEXT_GROUPS: usize = 64;
/// A scheduler edge has at most the target CPU's bounded system/cgroup set
/// plus the incoming task's bounded set.  This local Arc array is the unified
/// arbitration view; it owns no persistent registration and therefore cannot
/// retain a departed task group.
const MAX_CPU_ARBITER_GROUPS: usize = MAX_CPU_CONTEXT_GROUPS + MAX_GROUPS_PER_THREAD;
static CPU_CONTEXT_GROUPS: [SpinNoIrq<Vec<Arc<PerfGroup>>>; axconfig::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(Vec::new()) }; axconfig::plat::MAX_CPU_NUM];
/// Serializes the descriptor output graph with its sampling-backend mirror.
/// A route is visible to producers only after both edges and the destination
/// reference accounting have changed together under this lock.
pub(crate) static OUTPUT_ROUTING_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
static NEXT_INHERITED_EVENT_ID: AtomicU64 = AtomicU64::new(1 << 63);
static PERF_FLEX_CURSOR: [AtomicUsize; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicUsize::new(0) }; axconfig::plat::MAX_CPU_NUM];

impl PerfGroup {
    pub(crate) fn attach_cpu_context(group: &Arc<Self>) -> AxResult<()> {
        // Publication requires a live member; otherwise a concurrent
        // scheduler registry scan can prune the group before open finishes.
        if group.is_prunable() {
            return Err(AxError::InvalidInput);
        }
        let Some(cpu) = group.context.cpu() else {
            return Err(AxError::InvalidInput);
        };
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return Err(AxError::InvalidInput);
        };
        let mut groups = groups.lock();
        groups.retain(|entry| !entry.is_prunable());
        if groups.len() == MAX_CPU_CONTEXT_GROUPS {
            return Err(AxError::OperationNotSupported);
        }
        if groups.try_reserve(1).is_err() {
            return Err(AxError::NoMemory);
        }
        groups.push(group.clone());
        Ok(())
    }

    /// Roll back this syscall's just-created CPU/cgroup registration.  The
    /// registry owns a strong Arc, so merely dropping the caller's group is
    /// insufficient: it would otherwise linger until a later scheduler edge
    /// notices that it is prunable.
    pub(crate) fn detach_empty_cpu_context(group: &Arc<Self>) {
        let Some(cpu) = group.context.cpu() else {
            return;
        };
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        let mut groups = groups.lock();
        if let Some(index) = groups.iter().position(|entry| Arc::ptr_eq(entry, group)) {
            groups.remove(index);
        }
    }

    /// Enter the system-wide/cgroup contexts for one scheduled task.  Cgroup
    /// membership is checked at the scheduler boundary, after cgroup's
    /// membership transaction has published both of its indexes, so a group
    /// never borrows cycles from a task merely sharing its CPU.
    pub(crate) fn cpu_context_enter_for(cpu: usize, task_id: u64) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        let mut groups = groups.lock();
        groups.retain(|group| {
            if group.matches_cpu_context_task(task_id) {
                group.activate_on_current_cpu();
            }
            !group.is_prunable()
        });
    }

    pub(crate) fn cpu_context_leave(cpu: usize) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        groups.lock().retain(|group| {
            group.on_leave();
            !group.is_prunable()
        });
    }

    /// Rotate system-wide and cgroup groups at the same scheduler timer edge
    /// as task contexts.  The registry owns a bounded, pre-reserved `Arc`
    /// vector, so this IRQ-off path neither snapshots nor allocates.
    pub(crate) fn cpu_context_multiplex_tick(cpu: usize) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        let mut groups = groups.lock();
        groups.retain(|group| !group.is_prunable());
        Self::arbitrate_cpu_groups(cpu, groups.as_slice(), &[], true);
    }

    /// Settle the CPU/cgroup clock-source interval with the CPL directly
    /// captured by the timer trap frame. This runs IRQ-off and only visits
    /// the registry's pre-reserved strong references.
    pub(crate) fn cpu_context_account_clock_domain(cpu: usize, user: bool) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        let now = monotonic_time_nanos();
        for group in groups.lock().iter() {
            group.account_clock_sources_domain(now, user);
        }
    }

    pub(crate) fn cpu_context_clock_domain_transition(cpu: usize, user: bool) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        for group in groups.lock().iter() {
            group.account_clock_domain_transition(user);
        }
    }

    /// Reconcile the one per-CPU core-PMU scheduling domain.  The scheduler
    /// owns the task vector for this call and the CPU registry owns its vector;
    /// copying their Arcs into a fixed stack array takes no allocation and
    /// avoids a persistent task registration (and thus a close-time leak).
    ///
    /// The caller never holds a group-state lock while entering this routine.
    /// The scheduler-facing slot variant also releases the task vector before
    /// taking the CPU registry, so group control cannot create a lock cycle.
    pub(crate) fn arbitrate_cpu_with_task(cpu: usize, task_groups: &[Arc<Self>], tick: bool) {
        Self::arbitrate_cpu_with_task_iter(cpu, task_groups.iter(), tick);
    }

    /// Variant for a caller which copied its task holdings to fixed storage
    /// before releasing the task lock.  This is the scheduler-facing form and
    /// prevents task-lock -> CPU-registry lock nesting.
    pub(crate) fn arbitrate_cpu_with_task_slots(
        cpu: usize,
        task_groups: &[Option<Arc<Self>>],
        tick: bool,
    ) {
        Self::arbitrate_cpu_with_task_iter(cpu, task_groups.iter().flatten(), tick);
    }

    fn arbitrate_cpu_with_task_iter<'a>(
        cpu: usize,
        task_groups: impl Iterator<Item = &'a Arc<Self>>,
        tick: bool,
    ) {
        let Some(cpu_groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        let mut registered = cpu_groups.lock();
        registered.retain(|group| !group.is_prunable());
        let mut storage: [MaybeUninit<Arc<Self>>; MAX_CPU_ARBITER_GROUPS] =
            [const { MaybeUninit::uninit() }; MAX_CPU_ARBITER_GROUPS];
        let mut count = 0usize;
        for group in registered.iter() {
            if count == storage.len() {
                break;
            }
            storage[count].write(group.clone());
            count += 1;
        }
        drop(registered);
        for group in task_groups {
            if count == storage.len() {
                break;
            }
            storage[count].write(group.clone());
            count += 1;
        }
        // The copied Arcs protect groups from concurrent final-FD retirement
        // while state locks are taken below; no raw task pointer is retained.
        Self::arbitrate_cpu_storage(cpu, &mut storage, count, tick);
    }

    fn arbitrate_cpu_groups(
        cpu: usize,
        cpu_groups: &[Arc<Self>],
        task_groups: &[Arc<Self>],
        tick: bool,
    ) {
        let mut storage: [MaybeUninit<Arc<Self>>; MAX_CPU_ARBITER_GROUPS] =
            [const { MaybeUninit::uninit() }; MAX_CPU_ARBITER_GROUPS];
        let mut count = 0usize;
        for group in cpu_groups.iter().chain(task_groups) {
            if count == storage.len() {
                break;
            }
            storage[count].write(group.clone());
            count += 1;
        }
        Self::arbitrate_cpu_storage(cpu, &mut storage, count, tick);
    }

    fn arbitrate_cpu_storage(
        cpu: usize,
        storage: &mut [MaybeUninit<Arc<Self>>; MAX_CPU_ARBITER_GROUPS],
        count: usize,
        tick: bool,
    ) {
        // Every prefix entry was initialized exactly once by the collector;
        // the trailing capacity is never observed or dropped.
        {
            let groups =
                unsafe { core::slice::from_raw_parts(storage.as_ptr().cast::<Arc<Self>>(), count) };
            if tick {
                Self::multiplex_tick(groups);
            } else {
                Self::arbitrate_enter(groups);
            }
        }
        for group in &mut storage[..count] {
            // SAFETY: the initialized prefix is precisely the one borrowed
            // above, and no reference into it survives that scope.
            unsafe { group.assume_init_drop() };
        }
        let _ = cpu; // Documents that the caller selected the local domain.
    }

    /// Publish one already-materialized trace entry to every active CPU or
    /// cgroup context on this CPU. No registry snapshot is allocated on this
    /// producer path.
    pub(crate) fn cpu_context_tracepoint(cpu: usize, id: u64, raw: &[u8], timestamp: u64) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        let groups = groups.lock();
        for group in groups.iter() {
            group.emit_tracepoint_raw(id, raw, timestamp);
        }
    }

    pub(crate) fn cpu_context_dynamic_raw_at(cpu: usize, event: PerfEvent, ip: u64, raw: &[u8]) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        for group in groups.lock().iter() {
            group.emit_dynamic_raw_at(event, ip, raw);
        }
    }

    pub(crate) fn cpu_context_switch(
        cpu: usize,
        switch_out: bool,
        own: (u32, u32),
        peer: Option<(u32, u32)>,
    ) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        let groups = groups.lock();
        for group in groups.iter() {
            group.emit_switch_record(switch_out, own, peer);
        }
    }

    pub(crate) fn cpu_context_fault(cpu: usize) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        for group in groups.lock().iter() {
            group.on_fault();
        }
    }

    pub(crate) fn cpu_context_minor_fault(cpu: usize) {
        if let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) {
            for group in groups.lock().iter() {
                group.on_minor_fault();
            }
        }
    }

    pub(crate) fn cpu_context_major_fault(cpu: usize) {
        if let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) {
            for group in groups.lock().iter() {
                group.on_major_fault();
            }
        }
    }

    /// Re-evaluate cgroup-targeted contexts at the exact membership commit
    /// edge.  This is deliberately separate from the scheduler tick: a task
    /// moved out of a cgroup settles its old interval now, and a task moved
    /// in starts its new interval now when it remains current.
    pub(crate) fn cgroup_membership_changed(task_id: u64, cpu: usize) {
        if cpu == axhal::percpu::this_cpu_id() {
            Self::cpu_context_membership_changed_local(cpu, task_id);
            return;
        }
        #[cfg(all(feature = "perf-sampling", target_os = "none"))]
        {
            let Some(mailbox) = PERF_RECONCILE_MAILBOXES.get(cpu) else {
                return;
            };
            let generation = NEXT_RECONCILE_GENERATION
                .fetch_add(1, Ordering::Relaxed)
                .max(1);
            if !mailbox.try_publish_cgroup_membership(task_id, generation) {
                return;
            }
            if axhal::irq::send_ipi_reason(
                axhal::irq::IpiReason::PerfReconcile,
                axhal::irq::IpiTarget::Other { cpu_id: cpu },
            )
            .is_ok()
            {
                for _ in 0..RECONCILE_ACK_SPINS {
                    if mailbox.acknowledged.load(Ordering::Acquire) == generation {
                        return;
                    }
                    core::hint::spin_loop();
                }
            }
        }
    }

    fn cpu_context_membership_changed_local(cpu: usize, task_id: u64) {
        let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) else {
            return;
        };
        let mut groups = groups.lock();
        groups.retain(|group| {
            if matches!(group.context, PerfContext::Cgroup { .. }) {
                group.reconcile_cpu_context_membership_local(task_id);
            }
            !group.is_prunable()
        });
    }

    fn reconcile_cpu_context_membership_local(&self, task_id: u64) {
        let now = monotonic_time_nanos();
        let should_run = self.matches_cpu_context_task(task_id);
        let mut state = self.state.lock();
        if state.active.task_active {
            Self::stop_locked(&mut state, now, true);
            state.active.task_active = false;
            state.active.cpu = None;
            self.advance_generation_locked(&mut state);
        }
        if should_run {
            state.active.task_active = true;
            state.active.cpu = Some(axhal::percpu::this_cpu_id());
            state.active.reconcile_frozen = false;
            self.advance_generation_locked(&mut state);
            Self::start_locked(&mut state, now);
        }
    }

    fn matches_cpu_context_task(&self, task_id: u64) -> bool {
        match self.context {
            PerfContext::Cpu { .. } => true,
            PerfContext::Cgroup { cgroup_id, .. } => {
                thekernel_linux_process_adapter::try_pid_from_task_id(task_id)
                    .ok()
                    .is_some_and(|pid| {
                        crate::pseudofs::cgroup::perf_cgroup_contains(pid, cgroup_id)
                    })
            }
            PerfContext::Task { .. } | PerfContext::TaskOnCpu { .. } => false,
        }
    }
    pub(crate) fn new(target_task_id: u64, leader_id: u64) -> AxResult<Arc<Self>> {
        Self::new_for_context(
            PerfContext::Task {
                task_id: target_task_id,
            },
            leader_id,
        )
    }

    pub(crate) fn new_for_context(context: PerfContext, leader_id: u64) -> AxResult<Arc<Self>> {
        let target_task_id = context.task_id().unwrap_or(0);
        let mut members = Vec::new();
        members
            .try_reserve_exact(MAX_GROUP_MEMBERS)
            .map_err(|_| AxError::NoMemory)?;
        let mut active = ActiveGroup::new();
        active
            .files
            .try_reserve_exact(MAX_GROUP_MEMBERS)
            .map_err(|_| AxError::NoMemory)?;
        Arc::try_new(Self {
            context,
            target_task_id,
            leader_id,
            state: SpinNoIrq::new(GroupState { members, active }),
            generation: AtomicU64::new(0),
            retire_next: AtomicUsize::new(0),
        })
        .map_err(|_| AxError::NoMemory)
    }
    #[cfg(test)]
    fn placement_for_test(&self) -> (Option<usize>, u64) {
        let state = self.state.lock();
        (state.active.cpu, state.active.generation)
    }
    /// Reject an IPI which was published for an earlier placement.  The
    /// actual local stop remains serialized by `state`; this check is only a
    /// lock-free fast rejection and never grants authority to stop hardware.
    pub(crate) fn accepts_reconcile_generation(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }
    pub(crate) fn accepts_target(&self, id: u64) -> bool {
        self.context.task_id() == Some(id)
    }
    #[allow(dead_code)]
    pub(crate) const fn context(&self) -> PerfContext {
        self.context
    }
    fn is_leader(&self, id: u64) -> bool {
        self.leader_id == id
    }
    #[cfg(test)]
    pub(crate) fn is_group_leader_for_test(&self, id: u64) -> bool {
        self.is_leader(id)
    }
    pub(crate) fn has_hardware(&self) -> bool {
        self.state
            .lock()
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
            .any(|file| file.event.uses_pmu())
    }
    /// Construct the child-side event group for fork/clone inheritance.  It
    /// owns inherited files directly because those child contexts are not
    /// published as duplicate parent descriptors.  CPU and cgroup contexts
    /// are global scheduler objects and are therefore never inherited.
    pub(crate) fn inherit_for_child(
        &self,
        child_task_id: u64,
        clone_thread: bool,
    ) -> AxResult<Option<Arc<Self>>> {
        let context = match self.context {
            PerfContext::Task { .. } => PerfContext::Task {
                task_id: child_task_id,
            },
            PerfContext::TaskOnCpu { cpu, .. } => PerfContext::TaskOnCpu {
                task_id: child_task_id,
                cpu,
            },
            PerfContext::Cpu { .. } | PerfContext::Cgroup { .. } => return Ok(None),
        };
        let parent = self.state.lock();
        let inherited: Vec<InheritedMemberSpec> = parent
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
            .filter(|file| {
                file.lifecycle.inherit || (clone_thread && file.lifecycle.inherit_thread)
            })
            .map(|file| InheritedMemberSpec {
                event: file.event,
                disabled: !file.enabled(),
                read: file.read,
                lifecycle: file.lifecycle,
                placement: file.placement,
                count_user: file.count_user.load(Ordering::Acquire),
                count_kernel: file.count_kernel.load(Ordering::Acquire),
                #[cfg(feature = "perf-sampling")]
                sampling: file.sampling.clone(),
            })
            .collect();
        drop(parent);
        if inherited.is_empty() {
            return Ok(None);
        }
        let leader = NEXT_INHERITED_EVENT_ID.fetch_add(1, Ordering::Relaxed);
        let group = Self::new_for_context(context, leader)?;
        for (index, inherited) in inherited.into_iter().enumerate() {
            let id = if index == 0 {
                leader
            } else {
                NEXT_INHERITED_EVENT_ID.fetch_add(1, Ordering::Relaxed)
            };
            #[cfg(feature = "perf-sampling")]
            let sampling = inherited
                .sampling
                .as_ref()
                .map(|backend| backend.fork_clone(id, child_task_id))
                .transpose()?;
            #[cfg(feature = "perf-sampling")]
            let created = if let Some(backend) = sampling {
                PerfEventFile::new_sampling_placement(
                    id,
                    inherited.event,
                    &group,
                    inherited.read,
                    inherited.lifecycle,
                    inherited.placement,
                    backend,
                )
            } else {
                PerfEventFile::new_with_lifecycle_placement_domains(
                    id,
                    inherited.event,
                    inherited.disabled,
                    &group,
                    inherited.read,
                    inherited.lifecycle,
                    inherited.placement,
                    inherited.count_user,
                    inherited.count_kernel,
                )
            };
            #[cfg(not(feature = "perf-sampling"))]
            let created = PerfEventFile::new_with_lifecycle_placement_domains(
                id,
                inherited.event,
                inherited.disabled,
                &group,
                inherited.read,
                inherited.lifecycle,
                inherited.placement,
                inherited.count_user,
                inherited.count_kernel,
            );
            let file = created?;
            group.retain_inherited_file(&file)?;
        }
        Ok(Some(group))
    }
    /// Append this running group's watchpoints to a task-switch DR image.
    /// The image is bounded by the architectural four address registers;
    /// excess enabled breakpoints remain enabled but unscheduled until a
    /// later group placement refresh, matching perf's constrained semantics.
    pub(crate) fn append_debug_breakpoints(
        &self,
        slots: &mut [Option<(u64, u64, u32)>; 4],
        used: &mut usize,
    ) {
        let state = self.state.lock();
        if !state.active.running {
            return;
        }
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            let PerfEvent::Breakpoint { addr, len, ty } = file.event else {
                continue;
            };
            if file.enabled() && file.running() && *used < slots.len() {
                slots[*used] = Some((addr, len, ty));
                *used += 1;
            }
        }
    }

    pub(crate) fn emit_debug_exception(
        &self,
        slot_mask: u64,
        slot_base: &mut usize,
        ip: u64,
        user: bool,
    ) {
        let state = self.state.lock();
        if !state.active.running {
            return;
        }
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            if !matches!(file.event, PerfEvent::Breakpoint { .. }) {
                continue;
            }
            if file.enabled() && file.running() && *slot_base < 4 {
                if slot_mask & (1 << *slot_base) != 0 {
                    #[cfg(feature = "perf-sampling")]
                    if file.sampling.is_some() {
                        file.emit_source_raw_at(ip, user, &[], axhal::time::monotonic_time_nanos());
                    } else {
                        file.add_count(1);
                    }
                    #[cfg(not(feature = "perf-sampling"))]
                    file.add_count(1);
                }
                *slot_base += 1;
            }
        }
    }

    /// Append CPU/system-wide/cgroup watchpoints after task-attached slots.
    /// The same registry order is used by #DB attribution, so DR0--DR3 and
    /// perf event identities cannot drift at a switch boundary.
    pub(crate) fn cpu_context_append_debug_breakpoints(
        cpu: usize,
        slots: &mut [Option<(u64, u64, u32)>; 4],
        used: &mut usize,
    ) {
        if let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) {
            for group in groups.lock().iter() {
                group.append_debug_breakpoints(slots, used);
            }
        }
    }

    pub(crate) fn cpu_context_debug_exception(
        cpu: usize,
        slot_mask: u64,
        slot_base: &mut usize,
        ip: u64,
        user: bool,
    ) {
        if let Some(groups) = CPU_CONTEXT_GROUPS.get(cpu) {
            for group in groups.lock().iter() {
                group.emit_debug_exception(slot_mask, slot_base, ip, user);
            }
        }
    }
    pub(crate) fn is_prunable(&self) -> bool {
        let state = self.state.lock();
        // A task can stay on-CPU across arbitrarily many open/close cycles.
        // Once no member or hardware lease remains, its registry slot is
        // reclaimable without waiting for that task to leave the CPU.
        !state.active.running
            && !state.active.reconcile_frozen
            && state
                .members
                .iter()
                .all(|member| member.file.upgrade().is_none())
    }
    fn add(&self, event: &Arc<PerfEventFile>) -> AxResult<()> {
        let mut state = self.state.lock();
        Self::compact_locked(&mut state);
        if event.event.uses_pmu()
            && state
                .members
                .iter()
                .filter_map(|member| member.file.upgrade())
                .any(|member| member.event == event.event)
        {
            return Err(AxError::OperationNotSupported);
        }
        if state.members.len() == MAX_GROUP_MEMBERS {
            return Err(AxError::OperationNotSupported);
        }
        state.members.push(Member {
            file: Arc::downgrade(event),
            inherited: None,
            dead: false,
        });
        state.active.files.push(None);
        Ok(())
    }
    fn retain_inherited_file(&self, file: &Arc<PerfEventFile>) -> AxResult<()> {
        let mut state = self.state.lock();
        let Some(member) = state.members.iter_mut().find(|member| {
            member
                .file
                .upgrade()
                .is_some_and(|candidate| Arc::ptr_eq(&candidate, file))
        }) else {
            return Err(AxError::BadState);
        };
        member.inherited = Some(file.clone());
        Ok(())
    }
    fn live<'a>(
        state: &'a mut GroupState,
    ) -> impl Iterator<Item = (usize, Arc<PerfEventFile>)> + 'a {
        state
            .members
            .iter_mut()
            .enumerate()
            .filter_map(|(slot, member)| {
                let file = member.file.upgrade();
                member.dead = file.is_none();
                file.map(|file| (slot, file))
            })
    }
    fn compact_locked(state: &mut GroupState) {
        if state.active.running {
            return;
        }
        // Custody belongs to a running lease, not merely to a successful
        // Weak upgrade. An all-disabled group (or an unplaced sampler) can
        // finish start_locked without a lease. Release these references
        // before testing liveness, otherwise closed members never expire.
        state.active.files.fill(None);
        state
            .members
            .retain(|member| member.file.upgrade().is_some());
        // Every strong custody entry is now gone, so shrinking
        // the parallel slot vector preserves the all-None correspondence.
        state.active.files.truncate(state.members.len());
    }

    fn retire_member_locked(state: &mut GroupState, member_id: u64) {
        // The caller has settled the complete group. Remove membership now,
        // while close still owns the file, so re-admission cannot reacquire
        // the closing descriptor through its still-live Weak reference.
        state.members.retain(|member| {
            member.file.upgrade().is_some_and(|file| file.id != member_id)
        });
        state.active.files.truncate(state.members.len());
    }

    fn retire_member(&self, member_id: u64) {
        if !self.state.lock().members.iter().any(|member| {
            member.file.upgrade().is_some_and(|file| file.id == member_id)
        }) {
            return;
        }
        if self
            .synchronize_hardware_control(member_id, false, RECONCILE_CONTROL_RETIRE)
            .is_err()
        {
            // Preserve the existing bounded, fail-closed owner teardown if
            // the control mailbox cannot complete the retirement.
            self.reconcile_last_descriptor();
        }
    }

    /// A perf group is the hardware scheduling unit.  A member requesting
    /// pinned or exclusive semantics upgrades the complete group: allowing
    /// only a subset of its members to run would corrupt group reads and
    /// `time_running` accounting.
    fn placement_policy_locked(state: &GroupState) -> PerfPlacementPolicy {
        let mut policy = PerfPlacementPolicy::default();
        for file in state
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
        {
            if file.enabled() && file.event.uses_pmu() {
                policy.pinned |= file.placement.pinned;
                policy.exclusive |= file.placement.exclusive;
            }
        }
        // Linux's exclusive request has no useful flexible interpretation:
        // it either owns a complete PMU interval or is not scheduled.
        policy.pinned |= policy.exclusive;
        policy
    }

    #[cfg(feature = "perf-sampling")]
    fn sampling_slots_locked(state: &GroupState) -> (usize, bool) {
        let mut count = 0;
        let mut pinned = false;
        for file in &state.active.files {
            let Some(file) = file.as_ref().filter(|file| file.enabled()) else {
                continue;
            };
            let Some(backend) = file.sampling_hardware() else {
                continue;
            };
            count += 1;
            pinned |= backend.pinned();
        }
        (count, pinned)
    }

    #[cfg(feature = "perf-sampling")]
    fn start_sampling_locked(state: &mut GroupState, now: u64) -> bool {
        let (count, pinned) = Self::sampling_slots_locked(state);
        state.active.sampling_active.fill(None);
        if count == 0 {
            return true;
        }
        let mut armed = 0usize;
        // Rotate the first attempted member so flexible groups that cannot
        // currently fit are not biased forever toward their leader.
        let start = state.active.sampling_cursor % state.active.files.len().max(1);
        for offset in 0..state.active.files.len() {
            let slot = (start + offset) % state.active.files.len();
            let Some(file) = state.active.files[slot].as_ref() else {
                continue;
            };
            if !file.enabled() || file.sampling_hardware().is_none() {
                continue;
            }
            if !file.sampling_enter(state.active.generation) {
                // Group read semantics require a whole admitted set.  Pinned
                // admission is terminal; flexible admission will retry from
                // a rotated member at the next scheduler boundary.
                for active in state.active.sampling_active[..armed].iter_mut() {
                    if let Some(active_slot) = active.take()
                        && let Some(active_file) = state.active.files[active_slot].as_ref()
                    {
                        active_file.sampling_leave();
                        active_file.stop_running(now);
                    }
                }
                if pinned {
                    for file in state.active.files.iter().filter_map(Option::as_ref) {
                        if file.sampling_hardware().is_some() {
                            file.mark_invalid();
                        }
                    }
                }
                return false;
            }
            file.start_running(now);
            state.active.sampling_active[armed] = Some(slot);
            armed += 1;
        }
        armed == count
    }

    /// A timer boundary settles the selected sampler before arming its next
    /// sibling. This makes `time_running` describe actual hardware custody,
    /// while enabled time continues for every member of the live group.
    #[cfg(feature = "perf-sampling")]
    fn sampling_tick_locked(state: &mut GroupState, now: u64) {
        let (count, pinned) = Self::sampling_slots_locked(state);
        if count == 0 {
            return;
        }
        for slot in state.active.sampling_active.iter().flatten() {
            if let Some(file) = state.active.files[*slot].as_ref()
                && let Some(backend) = file.sampling_hardware()
            {
                backend.reconcile_current();
            }
        }
        if pinned {
            return;
        }
        // A flexible group voluntarily releases its complete set, then tries
        // a rotated all-or-nothing placement.  Failed members never receive
        // `time_running` and a sibling cannot be left sampling alone.
        for active in &mut state.active.sampling_active {
            if let Some(slot) = active.take()
                && let Some(file) = state.active.files[slot].as_ref()
            {
                file.sampling_leave();
                file.stop_running(now);
            }
        }
        state.active.sampling_cursor = state.active.sampling_cursor.wrapping_add(1);
        if !Self::start_sampling_locked(state, now) {
            // Do not retain a counting/external sibling after this group's
            // sampler set lost admission. The next flexible tick rebuilds
            // the whole group from members and tries a different order.
            state.active.running = true;
            Self::stop_locked(state, now, false);
        }
    }

    fn start_locked(state: &mut GroupState, now: u64) {
        if !state.active.task_active || state.active.running || state.active.reconcile_frozen {
            return;
        }
        for slot in 0..state.members.len() {
            let file = state.members[slot].file.upgrade();
            state.members[slot].dead = file.is_none();
            state.active.files[slot] = file;
        }
        #[cfg(feature = "pmu")]
        {
            let placement_policy = Self::placement_policy_locked(state);
            let mut wanted = [axhal::pmu::CountingProgram {
                event: axhal::pmu::Event::Cycles,
                cookie: 0,
            }; axhal::pmu::MAX_COUNTING_GROUP];
            let mut count = 0;
            let mut constraints = axhal::pmu::CountingConstraints::default();
            for file in state.active.files.iter().filter_map(Option::as_ref) {
                if file.enabled() {
                    if let Some(event) = file.counting_event() {
                        let requested = file.event.counting_constraints();
                        constraints.pebs_counter_mask |= requested.pebs_counter_mask;
                        constraints.needs_lbr |= requested.needs_lbr;
                        constraints.offcore_slots = constraints
                            .offcore_slots
                            .saturating_add(requested.offcore_slots);
                        constraints.needs_topdown |= requested.needs_topdown;
                        constraints.smt_shared_slots = constraints
                            .smt_shared_slots
                            .saturating_add(requested.smt_shared_slots);
                        wanted[count] = axhal::pmu::CountingProgram {
                            event,
                            cookie: file.id,
                        };
                        count += 1;
                    }
                }
            }
            if count != 0 {
                match axhal::pmu::counting_place_group_constrained_local(
                    state.active.generation,
                    &wanted[..count],
                    constraints,
                ) {
                    Ok(placement) => state.active.placement = Some(placement),
                    Err(_) => {
                        // A pinned/exclusive group is never silently put in
                        // the flexible pool.  Mark its PMU members invalid so
                        // its read side observes the failed live admission;
                        // flexible groups remain enabled and are retried at
                        // the next scheduler/tick placement boundary.
                        if placement_policy.pinned {
                            for file in state.active.files.iter().filter_map(Option::as_ref) {
                                if file.event.uses_pmu() {
                                    file.mark_invalid();
                                }
                            }
                        }
                        for file in &mut state.active.files {
                            *file = None;
                        }
                        state.active.running = false;
                        return;
                    }
                }
            }
            // Discovery/readonly PMUs have an independent owner transport.
            // Establish their baseline only after the core placement has
            // succeeded, so a failed mixed group cannot leave a programmed
            // package selector behind.
            for file in state.active.files.iter().filter_map(Option::as_ref) {
                if file.enabled()
                    && file.event.uses_external_counter()
                    && file.start_external_counter().is_err()
                {
                    // Core placement is already live. Settle it before
                    // dropping custody so a failed mixed group cannot leave
                    // a counter programmed behind an external-PMU error.
                    state.active.running = true;
                    Self::stop_locked(state, now, false);
                    return;
                }
            }
        }
        #[cfg(not(feature = "pmu"))]
        if state
            .active
            .files
            .iter()
            .filter_map(Option::as_ref)
            .any(|file| file.enabled() && file.event.uses_pmu())
        {
            for file in &mut state.active.files {
                *file = None;
            }
            return;
        }
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            #[cfg(feature = "perf-sampling")]
            if file.sampling_hardware().is_some() {
                continue;
            }
            file.start_running(now);
            #[cfg(feature = "perf-sampling")]
            let _ = file.sampling_enter(state.active.generation);
        }
        #[cfg(feature = "perf-sampling")]
        if !Self::start_sampling_locked(state, now) {
            // Counting/external setup happened before sampling. A mixed group
            // is still one scheduling unit, so a sampling admission failure
            // synchronously restores every already-programmed member.
            state.active.running = true;
            Self::stop_locked(state, now, false);
            return;
        }
        // A flexible sampler that lost the CPU-local transport has an enabled
        // descriptor but no running lease.  Do not publish it as active: the
        // inter-group selector will retry it after settling the incumbent.
        state.active.running = state
            .active
            .files
            .iter()
            .filter_map(Option::as_ref)
            .any(|file| file.running());
        Self::compact_locked(state);
    }
    fn stop_locked(state: &mut GroupState, now: u64, count_context_switch: bool) {
        if !state.active.running {
            Self::compact_locked(state);
            return;
        }
        #[cfg(feature = "pmu")]
        Self::settle_counting_locked(state);
        for file in state.active.files.iter_mut().filter_map(Option::take) {
            #[cfg(feature = "pmu")]
            file.settle_external_counter();
            #[cfg(feature = "perf-sampling")]
            file.sampling_leave();
            let was_running = file.stop_running(now);
            if count_context_switch
                && was_running
                && file.event == PerfEvent::Software(SoftwareEvent::ContextSwitches)
                && file.enabled()
            {
                file.add_count(1);
            }
        }
        state.active.running = false;
        #[cfg(feature = "perf-sampling")]
        {
            state.active.sampling_active.fill(None);
        }
        Self::compact_locked(state);
    }
    fn advance_generation_locked(&self, state: &mut GroupState) -> u64 {
        state.active.generation = NEXT_RECONCILE_GENERATION.fetch_add(1, Ordering::Relaxed);
        if state.active.generation == 0 {
            state.active.generation = NEXT_RECONCILE_GENERATION.fetch_add(1, Ordering::Relaxed);
        }
        self.generation
            .store(state.active.generation, Ordering::Release);
        state.active.generation
    }

    #[cfg(feature = "pmu")]
    fn settle_counting_locked(state: &mut GroupState) {
        let Some(placement) = state.active.placement.take() else {
            return;
        };
        let generation = placement.generation;
        let mut completions = [axhal::pmu::CountingCompletion {
            cookie: 0,
            generation: 0,
            delta: 0,
            overflowed: false,
        }; axhal::pmu::MAX_COUNTING_GROUP];
        let result = axhal::pmu::counting_stop_settle_current(generation)
            .and_then(|_| axhal::pmu::counting_take_completion_local(generation, &mut completions));
        match result {
            Ok(count) => {
                for completion in &completions[..count] {
                    if let Some(file) = state
                        .active
                        .files
                        .iter()
                        .filter_map(Option::as_ref)
                        .find(|file| file.id == completion.cookie)
                    {
                        if completion.overflowed {
                            #[cfg(feature = "bpf")]
                            file.run_attached_bpf(3, completion.delta);
                            file.mark_invalid();
                        } else {
                            file.add_count(completion.delta);
                        }
                    }
                }
            }
            Err(_) => {
                for file in state.active.files.iter().filter_map(Option::as_ref) {
                    if file.event.uses_pmu() {
                        file.mark_invalid();
                    }
                }
            }
        }
    }
    /// Execute one group operation at the owner-CPU hardware boundary.  The
    /// group is stopped and settled before changing software state, then
    /// placed again before acknowledgement.  `control` is deliberately a
    /// compact mailbox value rather than a callback so the remote half stays
    /// allocation-free and cannot invoke user memory.
    fn reconcile_control_local(
        &self,
        generation: u64,
        member_id: u64,
        group_control: bool,
        control: u8,
        cancelled: Option<&AtomicBool>,
    ) -> bool {
        if !self.accepts_reconcile_generation(generation) {
            return false;
        }
        let now = monotonic_time_nanos();
        let mut state = self.state.lock();
        if state.active.generation != generation
            || state.active.cpu != Some(axhal::percpu::this_cpu_id())
        {
            return false;
        }
        state.active.reconcile_frozen = true;
        // This is the single settlement point for counting, every active
        // sampler, and external/uncore baselines.  Nothing on the caller CPU
        // reads or reprograms a remotely-owned register after this returns.
        if state.active.running {
            Self::stop_locked(&mut state, now, false);
        }
        let mut found = matches!(control, RECONCILE_CONTROL_READ | RECONCILE_CONTROL_RETIRE);
        if control != RECONCILE_CONTROL_READ {
            let operation: fn(&PerfEventFile, u64) = match control {
                RECONCILE_CONTROL_ENABLE => PerfEventFile::enable_at,
                RECONCILE_CONTROL_DISABLE | RECONCILE_CONTROL_RETIRE => PerfEventFile::disable_at,
                RECONCILE_CONTROL_RESET => PerfEventFile::reset_at,
                _ => return false,
            };
            if group_control {
                for (_, file) in Self::live(&mut state) {
                    operation(&file, now);
                    found = true;
                }
            } else if let Some(file) = state
                .members
                .iter()
                .find_map(|member| member.file.upgrade().filter(|file| file.id == member_id))
            {
                operation(&file, now);
                found = true;
            }
        }
        if !found {
            state.active.reconcile_frozen = true;
            return false;
        }
        if control == RECONCILE_CONTROL_RETIRE {
            Self::retire_member_locked(&mut state, member_id);
        }
        if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
            for file in state
                .members
                .iter()
                .filter_map(|member| member.file.upgrade())
            {
                if file.event.uses_pmu() {
                    file.mark_invalid();
                }
            }
            state.active.reconcile_frozen = true;
            return false;
        }
        self.advance_generation_locked(&mut state);
        state.active.reconcile_frozen = false;
        let task_active = state.active.task_active;
        drop(state);
        // Re-admit through the whole CPU domain.  In particular, a remote
        // ENABLE must not program this one group around an already-running
        // exclusive peer, and a flexible remote READ may remain unplaced.
        if task_active {
            if let Some(thread) = current().try_as_thread() {
                thread.arbitrate_perf_current(false, false);
            } else {
                Self::arbitrate_cpu_with_task(axhal::percpu::this_cpu_id(), &[], false);
            }
        }
        let mut state = self.state.lock();
        if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
            Self::stop_locked(&mut state, now, false);
            for file in state
                .members
                .iter()
                .filter_map(|member| member.file.upgrade())
            {
                if file.event.uses_pmu() {
                    file.mark_invalid();
                }
            }
            state.active.reconcile_frozen = true;
            return false;
        }
        let needs_hardware = state
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
            .any(|file| file.enabled() && file.event.uses_pmu());
        let pinned = Self::placement_policy_locked(&state).pinned;
        if needs_hardware && pinned && !state.active.running {
            // Pinned/exclusive admission is synchronous: failing to restore
            // the complete group is an operation failure.  A flexible group
            // is different: READ is allowed to settle a multiplexed-out
            // interval, and ENABLE succeeds with time_running==0 until the
            // unified arbiter selects it at a later rotation boundary.
            for file in state
                .members
                .iter()
                .filter_map(|member| member.file.upgrade())
            {
                if file.event.uses_pmu() {
                    file.mark_invalid();
                }
            }
            state.active.reconcile_frozen = true;
            return false;
        }
        true
    }

    /// Cancellation is observable only after this CPU has claimed the
    /// mailbox.  It still settles the exact old generation, but deliberately
    /// does not perform the requested mutation or re-place counters.
    #[cfg(all(feature = "perf-sampling", target_os = "none"))]
    fn reconcile_cancel_local(&self, generation: u64) {
        if !self.accepts_reconcile_generation(generation) {
            return;
        }
        let now = monotonic_time_nanos();
        let mut state = self.state.lock();
        if state.active.generation != generation
            || state.active.cpu != Some(axhal::percpu::this_cpu_id())
        {
            return;
        }
        state.active.reconcile_frozen = true;
        if state.active.running {
            Self::stop_locked(&mut state, now, false);
        }
        for file in state
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
        {
            if file.event.uses_pmu() {
                file.mark_invalid();
            }
        }
    }

    #[cfg(all(feature = "perf-sampling", target_os = "none"))]
    fn synchronize_hardware_control(
        &self,
        member_id: u64,
        group_control: bool,
        control: u8,
    ) -> AxResult<()> {
        let owner = {
            let state = self.state.lock();
            let hardware_active = state.active.task_active
                && state
                    .members
                    .iter()
                    .filter_map(|member| member.file.upgrade())
                    .any(|file| file.requires_owner_control());
            hardware_active.then_some((state.active.cpu, state.active.generation))
        };
        let Some((Some(cpu), generation)) = owner else {
            if control == RECONCILE_CONTROL_READ {
                return Ok(());
            }
            // No CPU owns hardware at this moment.  The caller can mutate
            // only the descriptor state; no delayed PMU teardown is needed.
            return self.control_local_inactive(member_id, group_control, control);
        };
        if cpu == axhal::percpu::this_cpu_id() {
            return self
                .reconcile_control_local(generation, member_id, group_control, control, None)
                .then_some(())
                .ok_or(AxError::Io);
        }
        let Some(mailbox) = PERF_RECONCILE_MAILBOXES.get(cpu) else {
            self.fail_closed_reconcile();
            return Err(AxError::Io);
        };
        // The handler moves this explicit raw Arc into deferred task-context
        // retirement; it never drops it from the IPI/NMI path.
        unsafe { Arc::increment_strong_count(self as *const Self) };
        if !mailbox.try_publish_control(
            self as *const Self,
            generation,
            member_id,
            group_control,
            control,
        ) {
            unsafe { drop(Arc::from_raw(self as *const Self)) };
            self.fail_closed_reconcile();
            return Err(AxError::Io);
        }
        if axhal::irq::send_ipi_reason(
            axhal::irq::IpiReason::PerfReconcile,
            axhal::irq::IpiTarget::Other { cpu_id: cpu },
        )
        .is_ok()
        {
            for _ in 0..RECONCILE_ACK_SPINS {
                if mailbox.acknowledged.load(Ordering::Acquire) == generation {
                    let result = mailbox.result.load(Ordering::Acquire);
                    Self::drain_reconciled_custody();
                    if result == RECONCILE_RESULT_OK {
                        return Ok(());
                    }
                    self.fail_closed_reconcile();
                    return Err(AxError::Io);
                }
                core::hint::spin_loop();
            }
        } else if mailbox
            .group
            .compare_exchange(
                self as *const Self as usize,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            unsafe { drop(Arc::from_raw(self as *const Self)) };
        }
        // If an IPI raced the timeout, make its late owner-side execution a
        // stop-only fail-closed boundary instead of a belated successful
        // enable/reset.  The mailbox remains occupied until that CPU retires
        // the transferred raw Arc.
        mailbox.cancelled.store(true, Ordering::Release);
        // A late IPI owns the transferred raw reference.  Freeze all member
        // descriptors until it retires rather than claiming this operation
        // completed or waiting for a later scheduler tick.
        self.fail_closed_reconcile();
        Err(AxError::Io)
    }

    /// Controls without CPU-local custody settle under the group lock.
    /// Inactive hardware waits for a later scheduler placement; ordinary
    /// software groups can resume immediately. Owner-context controls use
    /// `reconcile_control_local` to settle an exact active generation.
    fn control_local_inactive(
        &self,
        member_id: u64,
        group_control: bool,
        control: u8,
    ) -> AxResult<()> {
        let mut state = self.state.lock();
        let now = monotonic_time_nanos();
        let caller_safe = !state
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
            .any(|file| file.requires_owner_control());
        // Ordinary software events have no CPU-local custody, but their
        // accounting can still be running. Clock samplers also need the owner
        // CPU because settlement captures the current task's sample context.
        // Serialize settlement and control with scheduler accounting under
        // the group lock, including controls
        // of a remote per-CPU tracepoint group.
        if state.active.running && !caller_safe {
            return Err(AxError::Io);
        }
        let operation: fn(&PerfEventFile, u64) = match control {
            RECONCILE_CONTROL_ENABLE => PerfEventFile::enable_at,
            RECONCILE_CONTROL_DISABLE | RECONCILE_CONTROL_RETIRE => PerfEventFile::disable_at,
            RECONCILE_CONTROL_RESET => PerfEventFile::reset_at,
            RECONCILE_CONTROL_READ => return Ok(()),
            _ => return Err(AxError::InvalidInput),
        };
        if caller_safe {
            Self::stop_locked(&mut state, now, false);
        }
        let mut found = control == RECONCILE_CONTROL_RETIRE;
        if group_control {
            for (_, file) in Self::live(&mut state) {
                operation(&file, now);
                found = true;
            }
        } else if let Some(file) = state
            .members
            .iter()
            .find_map(|member| member.file.upgrade().filter(|file| file.id == member_id))
        {
            operation(&file, now);
            found = true;
        }
        if control == RECONCILE_CONTROL_RETIRE {
            Self::retire_member_locked(&mut state, member_id);
        }
        if caller_safe {
            Self::start_locked(&mut state, now);
        }
        found.then_some(()).ok_or(AxError::BadFileDescriptor)
    }

    #[cfg(not(all(feature = "perf-sampling", target_os = "none")))]
    fn synchronize_hardware_control(
        &self,
        member_id: u64,
        group_control: bool,
        control: u8,
    ) -> AxResult<()> {
        let owner = {
            let state = self.state.lock();
            let hardware_active = state.active.task_active
                && state
                    .members
                    .iter()
                    .filter_map(|member| member.file.upgrade())
                    .any(|file| file.requires_owner_control());
            hardware_active.then_some((state.active.cpu, state.active.generation))
        };
        match owner {
            None if control == RECONCILE_CONTROL_READ => Ok(()),
            None => self.control_local_inactive(member_id, group_control, control),
            Some((Some(cpu), generation)) if cpu == axhal::percpu::this_cpu_id() => self
                .reconcile_control_local(generation, member_id, group_control, control, None)
                .then_some(())
                .ok_or(AxError::Io),
            Some(_) => {
                self.fail_closed_reconcile();
                Err(AxError::Io)
            }
        }
    }
    pub(crate) fn reconfigure_current(&self) {
        let now = monotonic_time_nanos();
        {
            let mut state = self.state.lock();
            if state.active.task_active {
                Self::stop_locked(&mut state, now, false);
            }
        }
        if let Some(thread) = current().try_as_thread() {
            thread.arbitrate_perf_current(false, false);
        } else {
            Self::arbitrate_cpu_with_task(axhal::percpu::this_cpu_id(), &[], false);
        }
    }

    /// Establish a local, stopped ownership boundary for an already-admitted
    /// sampling-attribute replacement.  Usercopy, ABI planning and every
    /// fallible capture-resource allocation must have happened before this
    /// method.  A remote owner is deliberately rejected: the IPI mailbox can
    /// settle control operations, but cannot safely carry arbitrary mutable
    /// sampling state or allocation ownership across CPUs.
    #[cfg(feature = "perf-sampling")]
    pub(crate) fn begin_sampling_modify(&self) -> AxResult<()> {
        let now = monotonic_time_nanos();
        let mut state = self.state.lock();
        if state.active.reconcile_frozen {
            return Err(AxError::ResourceBusy);
        }
        if state.active.running
            && state
                .active
                .cpu
                .is_some_and(|cpu| cpu != axhal::percpu::this_cpu_id())
        {
            return Err(AxError::ResourceBusy);
        }
        state.active.reconcile_frozen = true;
        if state.active.running {
            Self::stop_locked(&mut state, now, false);
        }
        Ok(())
    }

    /// Finish a successful or failed local sampling-attribute transaction.
    /// The exact old placement was stopped by `begin_sampling_modify`; only
    /// after the backend has swapped its fully prepared state may the local
    /// arbiter see this group again.
    #[cfg(feature = "perf-sampling")]
    pub(crate) fn finish_sampling_modify(&self) {
        let task_active = {
            let mut state = self.state.lock();
            if !state.active.reconcile_frozen {
                return;
            }
            self.advance_generation_locked(&mut state);
            state.active.reconcile_frozen = false;
            state.active.task_active
        };
        if task_active {
            if let Some(thread) = current().try_as_thread() {
                thread.arbitrate_perf_current(false, false);
            } else {
                Self::arbitrate_cpu_with_task(axhal::percpu::this_cpu_id(), &[], false);
            }
        }
    }
    /// Mark a group runnable on this CPU.  Hardware is deliberately not
    /// touched here: the caller follows with the unified CPU arbiter once it
    /// has made both task and CPU/cgroup groups visible.
    fn activate_on_current_cpu(&self) {
        if let Some(cpu) = self.context.cpu()
            && cpu != axhal::percpu::this_cpu_id()
        {
            // A task-on-CPU event remains enabled while its task runs on a
            // different CPU, but has no running interval or PMU placement.
            return;
        }
        let mut state = self.state.lock();
        state.active.task_active = true;
        state.active.cpu = Some(axhal::percpu::this_cpu_id());
        state.active.reconcile_frozen = false;
        self.advance_generation_locked(&mut state);
    }
    pub(crate) fn on_enter(&self) {
        self.activate_on_current_cpu();
    }
    pub(crate) fn on_leave(&self) {
        let now = monotonic_time_nanos();
        let mut state = self.state.lock();
        Self::stop_locked(&mut state, now, true);
        state.active.task_active = false;
        state.active.cpu = None;
        state.active.reconcile_frozen = false;
        self.advance_generation_locked(&mut state);
    }

    /// Scheduler tick multiplex boundary for hardware sampling members.  It
    /// rotates or retries a whole flexible group at its accounting boundary.
    #[cfg(feature = "perf-sampling")]
    pub(crate) fn on_sampling_tick(&self) {
        let now = monotonic_time_nanos();
        let mut state = self.state.lock();
        if !state.active.task_active || state.active.reconcile_frozen {
            return;
        }
        if state.active.running {
            Self::sampling_tick_locked(&mut state, now);
        } else if !Self::placement_policy_locked(&state).pinned {
            self.advance_generation_locked(&mut state);
            Self::start_locked(&mut state, now);
        }
    }

    fn flexible_hardware_active(&self) -> bool {
        let state = self.state.lock();
        if !state.active.task_active {
            return false;
        }
        let policy = Self::placement_policy_locked(&state);
        !policy.pinned
            && state
                .members
                .iter()
                .filter_map(|member| member.file.upgrade())
                .any(|file| file.enabled() && file.event.uses_pmu())
    }

    /// Initial admission for the complete CPU domain.  An exclusive group
    /// evicts every other hardware group; ordinary pinned groups evict every
    /// flexible group and are then placed before exactly one flexible whole
    /// group is allowed to consume the remaining PMU capacity.  Failed
    /// flexible placement intentionally remains enabled with zero running
    /// time, to be revisited on the next rotation boundary.
    fn arbitrate_enter(groups: &[Arc<Self>]) {
        let now = monotonic_time_nanos();
        let exclusive = groups.iter().find(|group| {
            let state = group.state.lock();
            state.active.task_active
                && Self::placement_policy_locked(&state).exclusive
                && state
                    .members
                    .iter()
                    .filter_map(|member| member.file.upgrade())
                    .any(|file| file.enabled() && file.event.uses_pmu())
        });
        for group in groups {
            let mut state = group.state.lock();
            let policy = Self::placement_policy_locked(&state);
            let hardware = state.active.task_active
                && state
                    .members
                    .iter()
                    .filter_map(|member| member.file.upgrade())
                    .any(|file| file.enabled() && file.event.uses_pmu());
            let selected_exclusive = exclusive.is_some_and(|chosen| Arc::ptr_eq(chosen, group));
            if exclusive.is_some() && policy.exclusive && !selected_exclusive {
                // Two simultaneous exclusive requests cannot both own one
                // PMU interval.  Unlike flexible overcommit this is a pinned
                // admission failure, made visible immediately to the second
                // complete group.
                for file in state
                    .members
                    .iter()
                    .filter_map(|member| member.file.upgrade())
                {
                    if file.event.uses_pmu() {
                        file.mark_invalid();
                    }
                }
            }
            if state.active.running
                && hardware
                && ((exclusive.is_some() && !selected_exclusive)
                    || (!policy.pinned && !selected_exclusive))
            {
                Self::stop_locked(&mut state, now, false);
            }
            // Pure software groups remain runnable irrespective of PMU
            // selection. A mixed group is admitted only as a complete unit.
            if !hardware && state.active.task_active && !state.active.running {
                Self::start_locked(&mut state, now);
            }
        }
        if let Some(exclusive) = exclusive {
            let mut state = exclusive.state.lock();
            if state.active.task_active && !state.active.running {
                exclusive.advance_generation_locked(&mut state);
                Self::start_locked(&mut state, now);
            }
            return;
        }
        for pinned in groups {
            let mut state = pinned.state.lock();
            if state.active.task_active
                && Self::placement_policy_locked(&state).pinned
                && !state.active.running
            {
                pinned.advance_generation_locked(&mut state);
                Self::start_locked(&mut state, now);
            }
        }
        // Begin the flexible round-robin with one complete group. Subsequent
        // ticks rotate this same unified list; no task-vs-CPU pool survives.
        if let Some(flexible) = groups.iter().find(|group| {
            let state = group.state.lock();
            state.active.task_active
                && !state.active.running
                && !Self::placement_policy_locked(&state).pinned
                && state
                    .members
                    .iter()
                    .filter_map(|member| member.file.upgrade())
                    .any(|file| file.enabled() && file.event.uses_pmu())
        }) {
            let mut state = flexible.state.lock();
            if state.active.task_active && !state.active.running {
                flexible.advance_generation_locked(&mut state);
                Self::start_locked(&mut state, now);
            }
        }
    }

    /// Scheduler-tick rotation is the accounting boundary for flexible PMU
    /// groups.  It stops the old whole group before placing the next one, so
    /// `time_enabled` remains continuous while `time_running` advances only
    /// during its admitted lease.  Pinned/exclusive groups never enter this
    /// selector.
    fn account_clock_sources_tick(&self, now: u64) {
        let state = self.state.lock();
        if !state.active.running || !state.active.task_active {
            return;
        }
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            let user = file.state.lock().clock_user;
            file.account_clock_until(now, user);
        }
    }

    pub(crate) fn account_clock_sources_domain(&self, now: u64, user: bool) {
        let state = self.state.lock();
        if !state.active.running || !state.active.task_active {
            return;
        }
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            file.account_clock_until(now, user);
        }
    }

    pub(crate) fn account_clock_domain_transition(&self, user: bool) {
        let now = monotonic_time_nanos();
        let state = self.state.lock();
        if !state.active.running || !state.active.task_active {
            return;
        }
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            let previous = file.state.lock().clock_user;
            file.account_clock_until(now, previous);
            file.state.lock().clock_user = user;
        }
    }

    pub(crate) fn multiplex_tick(groups: &[Arc<Self>]) {
        let now = monotonic_time_nanos();
        // CPU_CLOCK/TASK_CLOCK sample periods are measured while a task is
        // running. Settle their elapsed source units at every existing
        // scheduler boundary rather than postponing all crossings to leave.
        for group in groups {
            group.account_clock_sources_tick(now);
        }
        // An exclusive group owns this complete CPU PMU interval.  A stale
        // flexible lease from the preceding rotation is synchronously
        // settled; no flexible candidate may be selected until exclusivity
        // has left the active domain.
        if groups.iter().any(|group| {
            let state = group.state.lock();
            state.active.task_active && Self::placement_policy_locked(&state).exclusive
        }) {
            for group in groups {
                let mut state = group.state.lock();
                if state.active.running && !Self::placement_policy_locked(&state).exclusive {
                    Self::stop_locked(&mut state, now, false);
                }
            }
            return;
        }
        let cpu = axhal::percpu::this_cpu_id();
        let Some(cursor) = PERF_FLEX_CURSOR.get(cpu) else {
            return;
        };
        let mut candidates = [None; MAX_CPU_ARBITER_GROUPS];
        let mut count = 0usize;
        for group in groups {
            if group.flexible_hardware_active() && count < candidates.len() {
                candidates[count] = Some(group);
                count += 1;
            }
        }
        if count < 2 {
            // A single flexible group can still contain several sampling
            // members.  Its own cursor, rather than the inter-group cursor,
            // owns that rotation.
            #[cfg(feature = "perf-sampling")]
            for group in groups {
                group.on_sampling_tick();
            }
            return;
        }
        let selected = cursor.fetch_add(1, Ordering::Relaxed) % count;
        let previous = (selected + count - 1) % count;
        // Every entry is borrowed from `groups`, whose caller keeps all Arcs
        // alive for this bounded switch transaction.
        let old = candidates[previous].expect("bounded flexible candidate");
        let next = candidates[selected].expect("bounded flexible candidate");
        if !Arc::ptr_eq(old, next) {
            let mut old_state = old.state.lock();
            if old_state.active.running {
                Self::stop_locked(&mut old_state, now, false);
            }
            drop(old_state);
            let mut next_state = next.state.lock();
            if next_state.active.task_active && !next_state.active.running {
                next.advance_generation_locked(&mut next_state);
                Self::start_locked(&mut next_state, now);
            }
        }
        // The selected whole group may itself have more sampling members than
        // the NMI transport can host. Rotate only after group placement has
        // settled, so an unplaced sibling never gains `time_running`.
        #[cfg(feature = "perf-sampling")]
        next.on_sampling_tick();
    }

    /// Pinned admission precedes flexible rotation at a scheduler-enter edge.
    /// A previously running flexible group is synchronously settled before a
    /// pinned group is programmed; this is an atomic whole-group handoff, not
    /// a best-effort retry at some unrelated later tick.
    pub(crate) fn admit_pinned(groups: &[Arc<Self>]) {
        let now = monotonic_time_nanos();
        for pinned in groups {
            let wants_pinned = {
                let state = pinned.state.lock();
                state.active.task_active && Self::placement_policy_locked(&state).pinned
            };
            if !wants_pinned {
                continue;
            }
            for flexible in groups {
                if Arc::ptr_eq(pinned, flexible) || !flexible.flexible_hardware_active() {
                    continue;
                }
                let mut state = flexible.state.lock();
                if state.active.running {
                    Self::stop_locked(&mut state, now, false);
                }
            }
            let mut state = pinned.state.lock();
            if state.active.task_active && !state.active.running {
                pinned.advance_generation_locked(&mut state);
                Self::start_locked(&mut state, now);
            }
        }
    }
    /// Apply frozen attr lifecycle flags at the exec commit boundary.  This
    /// happens before CLOEXEC descriptor retirement, so both removal and
    /// enable-on-exec settle a live PMU interval synchronously rather than
    /// waiting for a later scheduler tick.
    pub(crate) fn on_exec(&self, pid: u32, tid: u32, comm: &[u8]) {
        let now = monotonic_time_nanos();
        let mut state = self.state.lock();
        let running = state.active.task_active;
        if running {
            Self::stop_locked(&mut state, now, true);
        }
        for (_, file) in Self::live(&mut state) {
            let _ = file.apply_exec_lifecycle(now);
            file.emit_comm_exec(pid, tid, comm);
        }
        if running {
            Self::start_locked(&mut state, now);
        }
    }
    pub(crate) fn emit_fork_record(
        &self,
        child_pid: u32,
        parent_pid: u32,
        child_tid: u32,
        parent_tid: u32,
    ) {
        let state = self.state.lock();
        for file in state
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
        {
            file.emit_fork_exit(
                thekernel_linux_perf::PERF_RECORD_FORK,
                child_pid,
                parent_pid,
                child_tid,
                parent_tid,
            );
        }
    }
    pub(crate) fn emit_exit_record(&self, pid: u32, ppid: u32, tid: u32, ptid: u32) {
        let state = self.state.lock();
        for file in state
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
        {
            file.emit_fork_exit(thekernel_linux_perf::PERF_RECORD_EXIT, pid, ppid, tid, ptid);
        }
    }
    pub(crate) fn emit_switch_record(
        &self,
        switch_out: bool,
        own: (u32, u32),
        next: Option<(u32, u32)>,
    ) {
        let next = matches!(
            self.context,
            PerfContext::Cpu { .. } | PerfContext::Cgroup { .. }
        )
        .then_some(next)
        .flatten();
        let state = self.state.lock();
        for file in state
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
        {
            file.emit_switch(switch_out, own, next);
        }
    }
    /// Stable MM transaction-commit callback: callers invoke only after the
    /// VMA publication succeeds, never on a speculative mapping.
    pub(crate) fn emit_mmap_record(
        &self,
        addr: u64,
        len: u64,
        pgoff: u64,
        info: &crate::perf_records::MmapInfo<'_>,
        pid: u32,
        tid: u32,
    ) {
        let state = self.state.lock();
        for file in state
            .members
            .iter()
            .filter_map(|member| member.file.upgrade())
        {
            file.emit_mmap(addr, len, pgoff, info, pid, tid);
        }
    }
    /// Stops and settles a placement immediately when called by its owning
    /// CPU.  This is the bounded local half of PerfReconcile: it allocates
    /// nothing, performs no usercopy, and holds no reference beyond the
    /// group-owned temporary custody already installed by `start_locked`.
    pub(crate) fn reconcile_local(&self, generation: u64) -> bool {
        if !self.accepts_reconcile_generation(generation) {
            return false;
        }
        let now = monotonic_time_nanos();
        let mut state = self.state.lock();
        if state.active.generation != generation
            || state.active.cpu != Some(axhal::percpu::this_cpu_id())
        {
            return false;
        }
        Self::stop_locked(&mut state, now, false);
        state.active.reconcile_frozen = true;
        self.advance_generation_locked(&mut state);
        true
    }

    /// Freeze and settle the currently placed group before its final FD can
    /// release the last `PerfEventFile` reference. A remote failure leaves the
    /// group frozen and invalid rather than allowing another placement; the
    /// scheduler's leave path remains able to release the pre-existing lease.
    pub(crate) fn reconcile_last_descriptor(&self) {
        Self::drain_reconciled_custody();
        let placement = {
            let mut state = self.state.lock();
            if !state.active.running || state.active.reconcile_frozen {
                return;
            }
            state.active.reconcile_frozen = true;
            (state.active.cpu, state.active.generation)
        };
        let (Some(cpu), generation) = placement else {
            return;
        };
        if cpu == axhal::percpu::this_cpu_id() {
            let _ = self.reconcile_local(generation);
            return;
        }
        #[cfg(all(feature = "perf-sampling", target_os = "none"))]
        {
            let Some(mailbox) = PERF_RECONCILE_MAILBOXES.get(cpu) else {
                self.fail_closed_reconcile();
                return;
            };
            // Keep this group alive even if the initiating last descriptor
            // is dropped after the bounded wait expires. The handler defers
            // this exact raw strong reference to a task-context retire list.
            unsafe { Arc::increment_strong_count(self as *const Self) };
            if !mailbox.try_publish(self as *const Self, generation) {
                // SAFETY: publication did not transfer the incremented
                // reference to an IPI consumer.
                unsafe { drop(Arc::from_raw(self as *const Self)) };
                self.fail_closed_reconcile();
                return;
            }
            if axhal::irq::send_ipi_reason(
                axhal::irq::IpiReason::PerfReconcile,
                axhal::irq::IpiTarget::Other { cpu_id: cpu },
            )
            .is_ok()
            {
                for _ in 0..RECONCILE_ACK_SPINS {
                    if mailbox.acknowledged.load(Ordering::Acquire) == generation {
                        self.settle_remote_reconcile(cpu, generation);
                        Self::drain_reconciled_custody();
                        return;
                    }
                    core::hint::spin_loop();
                }
            } else if mailbox
                .group
                .compare_exchange(
                    self as *const Self as usize,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                // SAFETY: no IPI was sent, so the mailbox did not transfer
                // this explicit strong reference to an interrupt consumer.
                unsafe { drop(Arc::from_raw(self as *const Self)) };
            }
            // Do not reclaim the raw Arc or clear the mailbox here: a late
            // IPI may already have loaded the pointer. Keeping that explicit
            // custody is fail-closed and prevents a use-after-free; a later
            // close drains it once the handler has retired it.
            self.fail_closed_reconcile();
        }
        #[cfg(not(all(feature = "perf-sampling", target_os = "none")))]
        self.fail_closed_reconcile();
    }

    fn fail_closed_reconcile(&self) {
        let state = self.state.lock();
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            file.mark_invalid();
        }
    }

    #[cfg(all(feature = "perf-sampling", target_os = "none"))]
    fn settle_remote_reconcile(&self, cpu: usize, generation: u64) {
        let Some(mailbox) = PERF_RECONCILE_MAILBOXES.get(cpu) else {
            self.fail_closed_reconcile();
            return;
        };
        let count = mailbox
            .completion_len
            .load(Ordering::Acquire)
            .min(axhal::pmu::MAX_COUNTING_GROUP);
        let now = monotonic_time_nanos();
        let mut state = self.state.lock();
        if state.active.generation != generation || !state.active.running {
            return;
        }
        for index in 0..count {
            let cookie = mailbox.completion_cookie[index].load(Ordering::Relaxed);
            let delta = mailbox.completion_delta[index].load(Ordering::Relaxed);
            let overflowed = mailbox.completion_overflow[index].load(Ordering::Relaxed) != 0;
            if let Some(file) = state
                .active
                .files
                .iter()
                .filter_map(Option::as_ref)
                .find(|file| file.id == cookie)
            {
                if overflowed {
                    file.mark_invalid();
                } else {
                    file.add_count(delta);
                }
            }
        }
        state.active.placement = None;
        for file in state.active.files.iter_mut().filter_map(Option::take) {
            let _ = file.stop_running(now);
        }
        state.active.running = false;
        Self::compact_locked(&mut state);
        self.advance_generation_locked(&mut state);
    }

    unsafe fn defer_reconcile_custody(&self) {
        loop {
            let head = PERF_RECONCILE_RETIRED.load(Ordering::Acquire);
            self.retire_next.store(head, Ordering::Relaxed);
            if PERF_RECONCILE_RETIRED
                .compare_exchange(
                    head,
                    self as *const Self as usize,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    pub(crate) fn has_deferred_reconcile_custody() -> bool {
        PERF_RECONCILE_RETIRED.load(Ordering::Acquire) != 0
    }

    pub(crate) fn drain_reconciled_custody() {
        let mut address = PERF_RECONCILE_RETIRED.swap(0, Ordering::AcqRel);
        while address != 0 {
            // SAFETY: each node owns precisely the additional Arc strong
            // reference transferred by `try_publish`; this runs only in task
            // context after the IPI has completed its hardware access.
            let group = unsafe { Arc::from_raw(address as *const Self) };
            address = group.retire_next.swap(0, Ordering::Relaxed);
            drop(group);
        }
    }
    pub(crate) fn on_fault(&self) {
        self.on_software_event(SoftwareEvent::PageFaults);
    }
    pub(crate) fn on_minor_fault(&self) {
        self.on_software_event(SoftwareEvent::PageFaults);
        self.on_software_event(SoftwareEvent::PageFaultsMin);
    }
    pub(crate) fn on_major_fault(&self) {
        self.on_software_event(SoftwareEvent::PageFaults);
        self.on_software_event(SoftwareEvent::PageFaultsMaj);
    }
    pub(crate) fn on_migration(&self) {
        self.on_software_event(SoftwareEvent::CpuMigrations);
    }
    pub(crate) fn emit_tracepoint(&self, id: u64) {
        self.emit_tracepoint_raw(id, &[], axhal::time::monotonic_time_nanos());
    }
    pub(crate) fn emit_tracepoint_raw(&self, id: u64, raw: &[u8], timestamp: u64) {
        let state = self.state.lock();
        if !state.active.running {
            return;
        }
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            if file.event == PerfEvent::Tracepoint(id) && file.running() && file.enabled() {
                file.add_count(1);
                file.emit_source_raw_at(0, false, raw, timestamp);
                #[cfg(feature = "bpf")]
                file.run_attached_bpf(2, id);
            }
        }
    }
    /// Counts a trap or a raw-source completion through the same group state
    /// used by software and PMU events.  Trap paths pass a value-only event
    /// descriptor and consequently allocate neither records nor ownership.
    pub(crate) fn emit_dynamic(&self, event: PerfEvent) {
        self.emit_dynamic_raw(event, &[]);
    }
    pub(crate) fn emit_dynamic_raw(&self, event: PerfEvent, raw: &[u8]) {
        self.emit_dynamic_raw_at(event, 0, raw);
    }
    pub(crate) fn emit_dynamic_raw_at(&self, event: PerfEvent, ip: u64, raw: &[u8]) {
        let state = self.state.lock();
        if !state.active.running {
            return;
        }
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            if same_dynamic_source(file.event, event) && file.running() && file.enabled() {
                file.add_count(1);
                file.emit_source_raw_at(
                    ip,
                    matches!(
                        event,
                        PerfEvent::Uprobe { .. } | PerfEvent::Breakpoint { .. }
                    ),
                    raw,
                    axhal::time::monotonic_time_nanos(),
                );
                #[cfg(feature = "bpf")]
                file.run_attached_bpf(3, dynamic_event_key(event));
            }
        }
    }
    fn on_software_event(&self, event: SoftwareEvent) {
        let state = self.state.lock();
        if !state.active.running {
            return;
        }
        for file in state.active.files.iter().filter_map(Option::as_ref) {
            if file.event == PerfEvent::Software(event) && file.running() && file.enabled() {
                file.add_count(1);
                file.emit_source_raw_at(0, false, &[], axhal::time::monotonic_time_nanos());
                #[cfg(feature = "bpf")]
                file.run_attached_bpf(1, event as u64);
            }
        }
    }
    fn snapshot_file_locked(
        &self,
        _state: &mut GroupState,
        file: &PerfEventFile,
        settled: bool,
    ) -> AxResult<Sample> {
        if file.invalid() {
            return Err(AxError::Io);
        }
        // The raw PMU placement intentionally has no remote/read-side lease
        // API. Counts are committed exactly once when a placement stops; a
        // live read therefore reports the settled prefix rather than risking
        // a cross-CPU MSR access.
        #[cfg(feature = "pmu")]
        let live = if !settled
            && file.event.uses_external_counter()
            && file.running()
            && file.external_running.load(Ordering::Acquire)
        {
            match file.external_current() {
                Ok(current) => Some(PerfEventFile::external_counter_delta(
                    file.event,
                    file.external_baseline.load(Ordering::Acquire),
                    current,
                )),
                Err(_) => return Err(AxError::OperationNotSupported),
            }
        } else {
            None
        };
        #[cfg(not(feature = "pmu"))]
        let live = None;
        file.sample_with_live(live)
    }
    fn snapshot_member(&self, id: u64, settled: bool) -> AxResult<Sample> {
        let mut state = self.state.lock();
        let Some(file) = state
            .members
            .iter()
            .find_map(|member| member.file.upgrade().filter(|file| file.id == id))
        else {
            return Err(AxError::BadFileDescriptor);
        };
        self.snapshot_file_locked(&mut state, &file, settled)
    }
    fn snapshots(&self, out: &mut Vec<Sample>, settled: bool) -> AxResult<()> {
        let mut state = self.state.lock();
        // Group format has group-level leader time fields. Once the leader
        // FD is gone we fail explicitly rather than silently substituting a
        // child descriptor's independent enabled/running timeline.
        if !state.members.iter().any(|member| {
            member
                .file
                .upgrade()
                .is_some_and(|file| file.id == self.leader_id)
        }) {
            return Err(AxError::BadFileDescriptor);
        }
        for slot in 0..state.members.len() {
            if let Some(file) = state.members[slot].file.upgrade() {
                let sample = self.snapshot_file_locked(&mut state, &file, settled)?;
                out.push(sample);
            }
        }
        Ok(())
    }
}

struct PerfEventState {
    enabled: bool,
    running: bool,
    invalid: bool,
    count: u64,
    enabled_total: u64,
    running_total: u64,
    enabled_since: u64,
    running_since: u64,
    /// Clock-source progress already handed to the sampling backend. Keeping
    /// this separate from `running_since` makes scheduler ticks real sample
    /// boundaries without double-counting the final switch-out interval.
    clock_accounted_since: u64,
    /// Privilege domain for the interval starting at `clock_accounted_since`.
    clock_user: bool,
}

/// Placement requests are immutable properties of the opened event.  They
/// are intentionally kept next to the FD instead of in the syscall layer so
/// an inherited member and a member added after a leader use the same live
/// group admission rules.
#[derive(Clone, Copy, Default)]
pub(crate) struct PerfPlacementPolicy {
    pub pinned: bool,
    pub exclusive: bool,
}

pub struct PerfEventFile {
    id: u64,
    event: PerfEvent,
    lifecycle: thekernel_linux_perf::PerfLifecycle,
    group: Weak<PerfGroup>,
    /// Strongly retains a redirected output owner. This is not a descriptor
    /// alias: it keeps its data ring alive after the target FD closes, while
    /// `set_output_target` rejects every graph cycle before publication.
    output_target: SpinNoIrq<Option<Arc<PerfEventFile>>>,
    output_users: AtomicUsize,
    read: ReadPlan,
    placement: PerfPlacementPolicy,
    count_user: AtomicBool,
    count_kernel: AtomicBool,
    /// Original symbol/path spelling used to create a kprobe/uprobe event.
    /// Object identity remains the attachment authority; this immutable byte
    /// string exists solely for Linux descriptor introspection.
    probe_query_name: SpinNoIrq<Option<Arc<Vec<u8>>>>,
    state: SpinNoIrq<PerfEventState>,
    #[cfg(feature = "perf-sampling")]
    sampling: Option<Arc<crate::file::PerfSampleBackend>>,
    /// A program reference is retained by the perf descriptor, never by an
    /// integer FD.  Thus replacing a perf-event-array entry or closing the
    /// program FD cannot create a use-after-close during an event callback.
    #[cfg(feature = "bpf")]
    bpf: SpinNoIrq<PerfBpfState>,
    /// Strong references held by PERF_EVENT_ARRAY slots.  They are separate
    /// from descriptor references: a numeric FD may close while the map still
    /// owns the event, exactly as in Linux.
    perf_map_refs: AtomicUsize,
    descriptor_final_closed: AtomicBool,
    source_released: AtomicBool,
    /// Baseline captured on the admitted owner CPU for external PMUs.
    external_baseline: AtomicU64,
    /// Exact package-counter ownership; unlike the old owner-wide restore,
    /// stopping this event cannot disturb another box or counter.
    #[cfg(feature = "pmu")]
    external_lease: SpinNoIrq<Option<axhal::perf_uncore::UncoreLease>>,
    /// True only after an uncore/read-only transport successfully published
    /// its baseline.  Group rollback must not settle a member whose program
    /// operation failed before ownership was established.
    external_running: AtomicBool,
}

#[cfg(feature = "bpf")]
struct PerfBpfState {
    program: Option<Arc<crate::bpf::prog::BpfProgram>>,
    generation: u64,
    owner: PerfBpfOwner,
}

#[cfg(feature = "bpf")]
#[derive(Clone, Copy, Eq, PartialEq)]
enum PerfBpfOwner {
    None,
    /// PERF_EVENT_IOC_SET_BPF owns this attachment.
    Direct,
    /// BPF_PROG_ATTACH owns this attachment.
    Legacy,
    /// A BPF link owns this attachment; only its generation may replace it.
    Link,
}
#[derive(Clone, Copy)]
struct Sample {
    id: u64,
    value: u64,
    enabled: u64,
    running: u64,
    lost: u64,
}

impl PerfEventFile {
    /// v6.18 `bpf_link_info.perf_event` payload for a BPF-owned attachment.
    pub(crate) fn bpf_link_info_data(&self) -> [u8; 48] {
        let mut data = [0u8; 48];
        let perf_type: u32 = match self.event {
            PerfEvent::Uprobe {
                retprobe: false,
                offset,
                ..
            } => {
                data[20..24].copy_from_slice(&(offset as u32).to_ne_bytes());
                1
            }
            PerfEvent::Uprobe {
                retprobe: true,
                offset,
                ..
            } => {
                data[20..24].copy_from_slice(&(offset as u32).to_ne_bytes());
                2
            }
            PerfEvent::Kprobe { addr, .. } => {
                data[24..32].copy_from_slice(&addr.to_ne_bytes());
                3
            }
            // Tracepoint's union begins with an in/out name pointer.  Generic
            // info copy must never reinterpret its numeric tracefs ID as a
            // userspace pointer; type-specific name-buffer usercopy is done
            // by the BPF info adapter when a provider exposes a name.
            PerfEvent::Tracepoint(_) => 5,
            PerfEvent::Raw { config, .. }
            | PerfEvent::Uncore { config, .. }
            | PerfEvent::ReadOnly { config, .. } => {
                data[8..16].copy_from_slice(&config.to_ne_bytes());
                6
            }
            _ => 6,
        };
        data[..4].copy_from_slice(&perf_type.to_ne_bytes());
        data
    }

    /// The perf-event link-info union has an in/out name buffer only for a
    /// tracepoint.  Return the provider-owned stable name rather than an ID
    /// which a generic info copy could accidentally expose as a userspace
    /// address.
    pub(crate) fn bpf_link_info_name(&self) -> AxResult<Option<&'static str>> {
        match self.event {
            PerfEvent::Tracepoint(id) => Ok(Some(crate::perf_sources::tracepoint(id)?.name)),
            _ => Ok(None),
        }
    }
    /// Dynamic trap sources share the mmap data-ring backend with PMU
    /// sampling.  The record is a bounded, fixed raw payload so #BP/#DB never
    /// allocates, parses user memory, or manufactures a second ring format.
    #[cfg(feature = "perf-sampling")]
    fn emit_dynamic_sample(&self, event: PerfEvent) {
        let Some(backend) = self.sampling.as_ref() else {
            return;
        };
        let mut payload = [0u8; 16];
        payload[..8].copy_from_slice(&dynamic_event_key(event).to_ne_bytes());
        payload[8..].copy_from_slice(&self.id.to_ne_bytes());
        let _ = backend.emit_raw_record(&payload);
    }

    pub fn new(
        id: u64,
        event: PerfEvent,
        disabled: bool,
        group: &Arc<PerfGroup>,
        read: ReadPlan,
    ) -> AxResult<Arc<Self>> {
        Self::new_with_lifecycle(
            id,
            event,
            disabled,
            group,
            read,
            thekernel_linux_perf::PerfLifecycle::default(),
        )
    }

    pub(crate) fn new_with_lifecycle(
        id: u64,
        event: PerfEvent,
        disabled: bool,
        group: &Arc<PerfGroup>,
        read: ReadPlan,
        lifecycle: thekernel_linux_perf::PerfLifecycle,
    ) -> AxResult<Arc<Self>> {
        #[cfg(feature = "perf-sampling")]
        {
            Self::new_inner(
                id,
                event,
                disabled,
                group,
                read,
                lifecycle,
                PerfPlacementPolicy::default(),
                true,
                true,
                None,
            )
        }
        #[cfg(not(feature = "perf-sampling"))]
        {
            Self::new_inner(
                id,
                event,
                disabled,
                group,
                read,
                lifecycle,
                PerfPlacementPolicy::default(),
                true,
                true,
            )
        }
    }

    pub(crate) fn new_with_lifecycle_placement(
        id: u64,
        event: PerfEvent,
        disabled: bool,
        group: &Arc<PerfGroup>,
        read: ReadPlan,
        lifecycle: thekernel_linux_perf::PerfLifecycle,
        placement: PerfPlacementPolicy,
    ) -> AxResult<Arc<Self>> {
        Self::new_with_lifecycle_placement_domains(
            id, event, disabled, group, read, lifecycle, placement, true, true,
        )
    }

    pub(crate) fn new_with_lifecycle_placement_domains(
        id: u64,
        event: PerfEvent,
        disabled: bool,
        group: &Arc<PerfGroup>,
        read: ReadPlan,
        lifecycle: thekernel_linux_perf::PerfLifecycle,
        placement: PerfPlacementPolicy,
        count_user: bool,
        count_kernel: bool,
    ) -> AxResult<Arc<Self>> {
        #[cfg(feature = "perf-sampling")]
        {
            Self::new_inner(
                id,
                event,
                disabled,
                group,
                read,
                lifecycle,
                placement,
                count_user,
                count_kernel,
                None,
            )
        }
        #[cfg(not(feature = "perf-sampling"))]
        {
            Self::new_inner(
                id,
                event,
                disabled,
                group,
                read,
                lifecycle,
                placement,
                count_user,
                count_kernel,
            )
        }
    }

    #[cfg(feature = "perf-sampling")]
    pub(crate) fn new_sampling(
        id: u64,
        event: PerfEvent,
        group: &Arc<PerfGroup>,
        read: ReadPlan,
        lifecycle: thekernel_linux_perf::PerfLifecycle,
        backend: Arc<crate::file::PerfSampleBackend>,
    ) -> AxResult<Arc<Self>> {
        Self::new_sampling_placement(
            id,
            event,
            group,
            read,
            lifecycle,
            PerfPlacementPolicy::default(),
            backend,
        )
    }

    #[cfg(feature = "perf-sampling")]
    pub(crate) fn new_sampling_placement(
        id: u64,
        event: PerfEvent,
        group: &Arc<PerfGroup>,
        read: ReadPlan,
        lifecycle: thekernel_linux_perf::PerfLifecycle,
        placement: PerfPlacementPolicy,
        backend: Arc<crate::file::PerfSampleBackend>,
    ) -> AxResult<Arc<Self>> {
        let (count_user, count_kernel) = backend.count_domains();
        Self::new_inner(
            id,
            event,
            false,
            group,
            read,
            lifecycle,
            placement,
            count_user,
            count_kernel,
            Some(backend),
        )
    }

    fn new_inner(
        id: u64,
        event: PerfEvent,
        disabled: bool,
        group: &Arc<PerfGroup>,
        read: ReadPlan,
        lifecycle: thekernel_linux_perf::PerfLifecycle,
        placement: PerfPlacementPolicy,
        count_user: bool,
        count_kernel: bool,
        #[cfg(feature = "perf-sampling")] sampling: Option<Arc<crate::file::PerfSampleBackend>>,
    ) -> AxResult<Arc<Self>> {
        if let PerfEvent::Kprobe { addr, retprobe, .. } = event {
            // The descriptor owns the probe reference from this point on.
            // Every construction failure below either releases it directly or
            // drops the partly-built descriptor through `release_dynamic_source`.
            crate::perf_sources::register_kprobe(addr, retprobe)?;
        }
        if let PerfEvent::Uprobe {
            mount_id,
            device,
            inode,
            offset,
            retprobe,
            reference_counter_offset,
        } = event
        {
            let key = crate::uprobe::UprobeFileKey {
                mount_id,
                device,
                inode,
            };
            crate::uprobe::register(key, offset, retprobe, reference_counter_offset)?;
        }
        let now = monotonic_time_nanos();
        let file = Arc::try_new(Self {
            id,
            event,
            lifecycle,
            group: Arc::downgrade(group),
            output_target: SpinNoIrq::new(None),
            output_users: AtomicUsize::new(0),
            read,
            placement,
            count_user: AtomicBool::new(count_user),
            count_kernel: AtomicBool::new(count_kernel),
            probe_query_name: SpinNoIrq::new(None),
            state: SpinNoIrq::new(PerfEventState {
                enabled: !disabled,
                running: false,
                invalid: false,
                count: 0,
                enabled_total: 0,
                running_total: 0,
                enabled_since: now,
                running_since: now,
                clock_accounted_since: now,
                clock_user: false,
            }),
            #[cfg(feature = "perf-sampling")]
            sampling: match event {
                PerfEvent::Software(SoftwareEvent::BpfOutput) => {
                    Some(crate::file::PerfSampleBackend::try_new_bpf_output(
                        id,
                        group.context.task_id().ok_or(AxError::InvalidInput)?,
                        disabled,
                    )?)
                }
                _ => sampling,
            },
            #[cfg(feature = "bpf")]
            bpf: SpinNoIrq::new(PerfBpfState {
                program: None,
                generation: 0,
                owner: PerfBpfOwner::None,
            }),
            perf_map_refs: AtomicUsize::new(0),
            descriptor_final_closed: AtomicBool::new(false),
            source_released: AtomicBool::new(false),
            external_baseline: AtomicU64::new(0),
            #[cfg(feature = "pmu")]
            external_lease: SpinNoIrq::new(None),
            external_running: AtomicBool::new(false),
        })
        .map_err(|_| AxError::NoMemory);
        let file = match file {
            Ok(file) => file,
            Err(error) => {
                if let PerfEvent::Kprobe { addr, retprobe, .. } = event {
                    crate::perf_sources::unregister_kprobe(addr, retprobe);
                }
                if let PerfEvent::Uprobe {
                    mount_id,
                    device,
                    inode,
                    offset,
                    retprobe,
                    reference_counter_offset,
                } = event
                {
                    crate::uprobe::unregister(
                        crate::uprobe::UprobeFileKey {
                            mount_id,
                            device,
                            inode,
                        },
                        offset,
                        retprobe,
                        reference_counter_offset,
                    );
                }
                return Err(error);
            }
        };
        #[cfg(all(feature = "perf-sampling", feature = "bpf"))]
        if let Some(backend) = file.sampling.as_ref() {
            backend.bind_owner(&file);
        }
        if let Err(error) = group.add(&file) {
            // `file` owns every dynamic-source registration.  Let Drop run
            // the one idempotent release path instead of unregistering an
            // uprobe here and again during open rollback.
            drop(file);
            return Err(error);
        }
        Ok(file)
    }

    #[cfg(feature = "perf-sampling")]
    fn sampling_enter(&self, generation: u64) -> bool {
        if let Some(backend) = &self.sampling {
            return !backend.uses_hardware() || backend.enter_current(generation);
        }
        true
    }

    #[cfg(feature = "perf-sampling")]
    fn sampling_leave(&self) {
        if let Some(backend) = self
            .sampling
            .as_ref()
            .filter(|backend| backend.uses_hardware())
        {
            backend.release_if_current();
        }
    }

    #[cfg(feature = "perf-sampling")]
    fn sampling_hardware(&self) -> Option<&Arc<crate::file::PerfSampleBackend>> {
        self.sampling
            .as_ref()
            .filter(|backend| backend.uses_hardware())
    }
    pub(crate) fn group(&self) -> Option<Arc<PerfGroup>> {
        self.group.upgrade()
    }

    fn apply_exec_lifecycle(&self, now: u64) -> bool {
        if self.lifecycle.remove_on_exec {
            self.disable_at(now);
            self.mark_invalid();
            return true;
        }
        if self.lifecycle.enable_on_exec {
            self.enable_at(now);
        }
        false
    }

    fn metadata_sample_id(&self, pid: u32, tid: u32) -> Option<crate::perf_records::SampleId> {
        if !self.lifecycle.sample_id_all {
            return None;
        }
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling {
            return Some(backend.metadata_sample_id(pid, tid));
        }
        Some(crate::perf_records::SampleId {
            sample_type: 0,
            pid,
            tid,
            time: 0,
            id: self.id,
            stream_id: self.id,
            cpu: 0,
        })
    }
    fn emit_record(&self, record: &[u8]) {
        if !self.enabled() {
            return;
        }
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling {
            let _ = backend.emit_metadata_record(record);
        }
    }
    /// Source hooks call this only after matching an enabled/running event.
    /// The backend owns a preallocated mmap ring; a missing mapping converts
    /// to its normal LOST/unsupported behavior and never changes the count.
    fn emit_source_raw_at(&self, ip: u64, user: bool, raw: &[u8], timestamp: u64) {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling {
            let _ =
                backend.emit_source_raw_record_at(current().id().as_u64() as u32, ip, user, raw, timestamp);
        }
    }
    fn emit_comm_exec(&self, pid: u32, tid: u32, comm: &[u8]) {
        if !self.lifecycle.comm || !self.lifecycle.comm_exec {
            return;
        }
        let mut record = [0u8; 256];
        if let Some(size) = crate::perf_records::comm(
            &mut record,
            thekernel_linux_perf::PERF_RECORD_MISC_COMM_EXEC,
            pid,
            tid,
            comm,
            self.metadata_sample_id(pid, tid),
        ) {
            self.emit_record(&record[..size]);
        }
    }
    fn emit_fork_exit(&self, kind: u32, pid: u32, ppid: u32, tid: u32, ptid: u32) {
        if !self.lifecycle.task {
            return;
        }
        let mut record = [0u8; 64];
        if let Some(size) = crate::perf_records::fork_exit(
            &mut record,
            kind,
            pid,
            ppid,
            tid,
            ptid,
            monotonic_time_nanos(),
            self.metadata_sample_id(pid, tid),
        ) {
            self.emit_record(&record[..size]);
        }
    }
    fn emit_switch(&self, switch_out: bool, own: (u32, u32), next: Option<(u32, u32)>) {
        if !self.lifecycle.context_switch {
            return;
        }
        let (pid, tid) = own;
        let mut record = [0u8; 64];
        let misc = if switch_out {
            thekernel_linux_perf::PERF_RECORD_MISC_SWITCH_OUT
        } else {
            0
        };
        if let Some(size) =
            crate::perf_records::switch(&mut record, misc, next, self.metadata_sample_id(pid, tid))
        {
            self.emit_record(&record[..size]);
        }
    }
    fn emit_read(&self) {
        let Some(group) = self.group() else {
            return;
        };
        let group_read = self.read.group;
        let mut samples = Vec::new();
        if samples.try_reserve(MAX_GROUP_MEMBERS).is_err() {
            #[cfg(feature = "perf-sampling")]
            if let Some(backend) = &self.sampling {
                backend.charge_lost_record();
            }
            return;
        }
        let snapshot = if group_read {
            group.snapshots(&mut samples, true)
        } else {
            group
                .snapshot_member(self.id, true)
                .map(|sample| samples.push(sample))
        };
        if snapshot.is_err() || samples.is_empty() {
            return;
        }
        let leader = if group_read {
            samples
                .iter()
                .copied()
                .find(|sample| sample.id == group.leader_id)
        } else {
            samples.first().copied()
        };
        let Some(leader) = leader else {
            return;
        };
        let fields = 1 + self.read.time_enabled as usize + self.read.time_running as usize;
        let member_fields = 1 + self.read.id as usize + self.read.lost as usize;
        let body_words = if group_read {
            fields.saturating_add(samples.len().saturating_mul(member_fields))
        } else {
            member_fields
                .saturating_add(self.read.time_enabled as usize + self.read.time_running as usize)
        };
        let Some(body_len) = body_words.checked_mul(core::mem::size_of::<u64>()) else {
            return;
        };
        let mut body = Vec::new();
        if body.try_reserve_exact(body_len).is_err() {
            #[cfg(feature = "perf-sampling")]
            if let Some(backend) = &self.sampling {
                backend.charge_lost_record();
            }
            return;
        }
        let mut push = |value: u64| body.extend_from_slice(&value.to_ne_bytes());
        if group_read {
            push(samples.len() as u64);
            if self.read.time_enabled {
                push(leader.enabled);
            }
            if self.read.time_running {
                push(leader.running);
            }
            for sample in &samples {
                push(sample.value);
                if self.read.id {
                    push(sample.id);
                }
                if self.read.lost {
                    push(sample.lost);
                }
            }
        } else {
            push(leader.value);
            if self.read.time_enabled {
                push(leader.enabled);
            }
            if self.read.time_running {
                push(leader.running);
            }
            if self.read.id {
                push(leader.id);
            }
            if self.read.lost {
                push(leader.lost);
            }
        }
        let pid = group
            .context
            .task_id()
            .unwrap_or_else(|| current().id().as_u64()) as u32;
        let sample_id = self.metadata_sample_id(pid, pid);
        // The only currently legal SAMPLE_ID_ALL trailer fields are six
        // eight-byte words. Allocate that exact upper bound plus header/body.
        let Some(capacity) = 16usize
            .checked_add(body.len())
            .and_then(|n| n.checked_add(48))
        else {
            return;
        };
        let mut record = Vec::new();
        if record.try_reserve_exact(capacity).is_err() {
            #[cfg(feature = "perf-sampling")]
            if let Some(backend) = &self.sampling {
                backend.charge_lost_record();
            }
            return;
        }
        record.resize(capacity, 0);
        if let Some(size) = crate::perf_records::read(&mut record, pid, pid, &body, sample_id) {
            self.emit_record(&record[..size]);
        } else {
            #[cfg(feature = "perf-sampling")]
            if let Some(backend) = &self.sampling {
                backend.charge_lost_record();
            }
        }
    }
    fn emit_mmap(
        &self,
        addr: u64,
        len: u64,
        pgoff: u64,
        info: &crate::perf_records::MmapInfo<'_>,
        pid: u32,
        tid: u32,
    ) {
        if (info.executable && !self.lifecycle.mmap)
            || (!info.executable && !self.lifecycle.mmap_data)
        {
            return;
        }
        let mmap2 = self.lifecycle.mmap2;
        let sample = self.metadata_sample_id(pid, tid);
        let Some(record_len) =
            crate::perf_records::mmap_record_len(mmap2, info.filename.len(), sample)
        else {
            #[cfg(feature = "perf-sampling")]
            if let Some(backend) = &self.sampling {
                backend.charge_lost_record();
            }
            return;
        };
        let mut record = Vec::new();
        if record.try_reserve_exact(record_len).is_err() {
            #[cfg(feature = "perf-sampling")]
            if let Some(backend) = &self.sampling {
                backend.charge_lost_record();
            }
            return;
        }
        record.resize(record_len, 0);
        if let Some(size) =
            crate::perf_records::mmap(&mut record, mmap2, pid, tid, addr, len, pgoff, info, sample)
        {
            self.emit_record(&record[..size]);
        } else {
            #[cfg(feature = "perf-sampling")]
            if let Some(backend) = &self.sampling {
                backend.charge_lost_record();
            }
        }
    }

    fn output_context(&self) -> AxResult<PerfContext> {
        self.group()
            .map(|group| group.context())
            .ok_or(AxError::BadFileDescriptor)
    }

    /// Validate and publish a shared data-ring destination. Context equality
    /// prevents a task event from writing a system/cgroup ring with unrelated
    /// authority; the bounded ancestor walk rejects self and indirect cycles.
    pub(crate) fn set_output_target(&self, target: Arc<PerfEventFile>) -> AxResult<()> {
        if core::ptr::eq(self, Arc::as_ptr(&target))
            || self.output_context()? != target.output_context()?
        {
            return Err(AxError::InvalidInput);
        }
        let _routing = OUTPUT_ROUTING_LOCK.lock();
        let mut cursor = Some(target.clone());
        for _ in 0..MAX_GROUP_MEMBERS {
            let Some(node) = cursor else {
                break;
            };
            if core::ptr::eq(self, Arc::as_ptr(&node)) {
                return Err(AxError::InvalidInput);
            }
            cursor = node.output_target.lock().clone();
        }
        if cursor.is_some() {
            return Err(AxError::InvalidInput);
        }
        #[cfg(feature = "perf-sampling")]
        {
            let source = self
                .sampling
                .as_ref()
                .ok_or(AxError::OperationNotSupported)?;
            let destination = target
                .sampling
                .as_ref()
                .ok_or(AxError::OperationNotSupported)?;
            source.share_output_from_transaction(destination)?;
        }
        #[cfg(not(feature = "perf-sampling"))]
        return Err(AxError::OperationNotSupported);
        let previous = self.output_target.lock().replace(target);
        let current = self
            .output_target
            .lock()
            .as_ref()
            .expect("output target was just installed")
            .clone();
        current.output_users.fetch_add(1, Ordering::AcqRel);
        let finalize =
            previous.filter(|previous| previous.output_users.fetch_sub(1, Ordering::AcqRel) == 1);
        drop(_routing);
        if let Some(previous) = finalize {
            previous.finish_output_target_close();
        }
        Ok(())
    }

    fn clear_output_target(&self) {
        let finalize = {
            let _routing = OUTPUT_ROUTING_LOCK.lock();
            #[cfg(feature = "perf-sampling")]
            if let Some(source) = &self.sampling {
                source.clear_output_target_transaction();
            }
            self.output_target
                .lock()
                .take()
                .filter(|previous| previous.output_users.fetch_sub(1, Ordering::AcqRel) == 1)
        };
        if let Some(previous) = finalize {
            previous.finish_output_target_close();
        }
    }

    fn finish_output_target_close(&self) {
        if !self.descriptor_final_closed.load(Ordering::Acquire)
            || self.output_users.load(Ordering::Acquire) != 0
        {
            return;
        }
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling {
            backend.final_close();
        }
    }

    /// Acquires the non-FD object reference used by PERF_EVENT_ARRAY.
    #[cfg(feature = "bpf")]
    pub(crate) fn retain_perf_map_ref(&self) {
        self.perf_map_refs.fetch_add(1, Ordering::AcqRel);
    }

    /// Releases a map-held event.  If the numeric descriptor has already
    /// closed, this is the exact final-close boundary rather than a delayed
    /// scheduler/tick cleanup.
    #[cfg(feature = "bpf")]
    pub(crate) fn release_perf_map_ref(&self) {
        let previous = self.perf_map_refs.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0, "unbalanced PERF_EVENT_ARRAY reference");
        if previous == 1 && self.descriptor_final_closed.load(Ordering::Acquire) {
            self.finish_map_held_close();
        }
    }

    fn finish_map_held_close(&self) {
        if let Some(group) = self.group() {
            // Retire membership at the owner CPU before restoring terminal
            // resources; surviving siblings are re-admitted there as part
            // of the same control operation.
            group.retire_member(self.id);
        }
        self.release_dynamic_source();
        #[cfg(feature = "pmu")]
        self.restore_external_terminal();
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling {
            if self.output_users.load(Ordering::Acquire) == 0 {
                backend.final_close();
            }
        }
    }
    fn release_dynamic_source(&self) {
        if self.source_released.swap(true, Ordering::AcqRel) {
            return;
        }
        match self.event {
            PerfEvent::Kprobe { addr, retprobe, .. } => {
                crate::perf_sources::unregister_kprobe(addr, retprobe)
            }
            PerfEvent::Uprobe {
                mount_id,
                device,
                inode,
                offset,
                retprobe,
                reference_counter_offset,
            } => crate::uprobe::unregister(
                crate::uprobe::UprobeFileKey {
                    mount_id,
                    device,
                    inode,
                },
                offset,
                retprobe,
                reference_counter_offset,
            ),
            _ => {}
        }
    }
    #[cfg(feature = "pmu")]
    fn restore_external_terminal(&self) {
        self.external_running.store(false, Ordering::Release);
        let PerfEvent::Uncore {
            box_type, box_id, ..
        } = self.event
        else {
            return;
        };
        let Ok(owner) = axhal::perf_uncore::owner_cpu(box_type, box_id) else {
            self.mark_invalid();
            return;
        };
        let Some(lease) = self.external_lease.lock().take() else {
            return;
        };
        if owner == axhal::percpu::this_cpu_id() {
            if axhal::perf_uncore::settle_release_owner_current(lease).is_err() {
                self.mark_invalid();
            }
            return;
        }
        let generation = NEXT_RECONCILE_GENERATION
            .fetch_add(1, Ordering::Relaxed)
            .max(1);
        if axhal::perf_uncore::publish_settle_release(owner, generation, lease).is_err() {
            self.mark_invalid();
            return;
        }
        if axhal::irq::send_ipi_reason(
            axhal::irq::IpiReason::PerfReconcile,
            axhal::irq::IpiTarget::Other { cpu_id: owner },
        )
        .is_err()
        {
            axhal::perf_uncore::cancel_reconcile(owner, generation);
            self.mark_invalid();
            return;
        }
        for _ in 0..RECONCILE_ACK_SPINS {
            if axhal::perf_uncore::reconcile_acknowledged(owner) == generation {
                if axhal::perf_uncore::reconcile_result(owner, generation).is_err() {
                    self.mark_invalid();
                }
                return;
            }
            core::hint::spin_loop();
        }
        self.mark_invalid();
    }
    /// Installs (or atomically replaces) a verified perf-event program.  The
    /// ring allocation is done here in ioctl/task context, never in the
    /// event/NMI producer path.
    #[cfg(feature = "bpf")]
    pub(crate) fn set_bpf_program(
        &self,
        program: Arc<crate::bpf::prog::BpfProgram>,
    ) -> AxResult<()> {
        let mut bpf = self.bpf.lock();
        bpf.program = Some(program);
        bpf.owner = PerfBpfOwner::Direct;
        bpf.generation = bpf.generation.wrapping_add(1).max(1);
        Ok(())
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn attach_bpf_link(
        &self,
        program: Arc<crate::bpf::prog::BpfProgram>,
    ) -> AxResult<u64> {
        let mut bpf = self.bpf.lock();
        if bpf.program.is_some() {
            return Err(AxError::AlreadyExists);
        }
        bpf.program = Some(program);
        bpf.owner = PerfBpfOwner::Link;
        bpf.generation = bpf.generation.wrapping_add(1).max(1);
        Ok(bpf.generation)
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn attach_legacy_bpf_program(
        &self,
        program: Arc<crate::bpf::prog::BpfProgram>,
    ) -> AxResult<()> {
        let mut bpf = self.bpf.lock();
        if bpf.program.is_some() {
            return Err(AxError::AlreadyExists);
        }
        bpf.program = Some(program);
        bpf.owner = PerfBpfOwner::Legacy;
        bpf.generation = bpf.generation.wrapping_add(1).max(1);
        Ok(())
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn detach_legacy_bpf_program(&self, expected: Option<u32>) -> AxResult<()> {
        let mut bpf = self.bpf.lock();
        if bpf.owner != PerfBpfOwner::Legacy
            || expected.is_some_and(|id| {
                bpf.program
                    .as_ref()
                    .map_or(true, |program| program.prog_id != id)
            })
        {
            return Err(AxError::NotFound);
        }
        bpf.program = None;
        bpf.owner = PerfBpfOwner::None;
        bpf.generation = bpf.generation.wrapping_add(1).max(1);
        Ok(())
    }

    /// Detaches the perf-program link in task context.  Incrementing the
    /// generation makes a concurrently observed old attachment stale before
    /// its retained program reference is dropped.
    #[cfg(feature = "bpf")]
    pub(crate) fn clear_bpf_program(&self) {
        let mut bpf = self.bpf.lock();
        bpf.program = None;
        bpf.owner = PerfBpfOwner::None;
        bpf.generation = bpf.generation.wrapping_add(1).max(1);
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn detach_bpf_link_if_current(&self, generation: u64) {
        let mut bpf = self.bpf.lock();
        if bpf.generation == generation {
            bpf.program = None;
            bpf.owner = PerfBpfOwner::None;
            bpf.generation = bpf.generation.wrapping_add(1).max(1);
        }
    }

    /// Replaces the program owned by one particular BPF link. A perf ioctl
    /// may independently replace the event program, so a stale link must not
    /// overwrite the attachment that superseded it.
    #[cfg(feature = "bpf")]
    pub(crate) fn replace_bpf_link_if_current(
        &self,
        generation: u64,
        program: Arc<crate::bpf::prog::BpfProgram>,
    ) -> AxResult<u64> {
        let mut bpf = self.bpf.lock();
        if bpf.generation != generation || bpf.owner != PerfBpfOwner::Link || bpf.program.is_none()
        {
            return Err(AxError::NotFound);
        }
        bpf.program = Some(program);
        bpf.owner = PerfBpfOwner::Link;
        bpf.generation = bpf.generation.wrapping_add(1).max(1);
        Ok(bpf.generation)
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn bpf_program_id(&self) -> Option<u32> {
        self.bpf
            .lock()
            .program
            .as_ref()
            .map(|program| program.prog_id)
    }

    /// Installs the exact copied probe spelling before descriptor
    /// publication.  The first spelling wins so a later alias can never
    /// rewrite the identity userspace originally opened.
    pub(crate) fn install_probe_query_name(&self, name: Arc<Vec<u8>>) {
        let mut slot = self.probe_query_name.lock();
        if slot.is_none() {
            *slot = Some(name);
        }
    }

    /// Metadata exposed by `BPF_TASK_FD_QUERY` for a perf-event descriptor.
    /// The program reference and event classification are sampled under the
    /// attachment lock, so a concurrent detach yields either the complete old
    /// answer or `ENOENT`, never a mixed program/event tuple.
    #[cfg(feature = "bpf")]
    pub(crate) fn bpf_task_fd_query(&self) -> AxResult<PerfBpfTaskFdQuery> {
        let bpf = self.bpf.lock();
        let program = bpf.program.as_ref().ok_or(AxError::NotFound)?;
        if program.prog_type == crate::bpf::defs::BPF_PROG_TYPE_PERF_EVENT {
            return Err(AxError::OperationNotSupported);
        }
        let (fd_type, name, probe_offset, probe_addr) = match self.event {
            PerfEvent::Tracepoint(id) => {
                let tracepoint = crate::perf_sources::tracepoint(id)?;
                (
                    thekernel_linux_bpf::BPF_FD_TYPE_TRACEPOINT,
                    Some(PerfBpfTaskFdQueryName::Static(tracepoint.name.as_bytes())),
                    0,
                    0,
                )
            }
            PerfEvent::Kprobe {
                addr,
                retprobe,
                query_offset,
            } => {
                let name = self
                    .probe_query_name
                    .lock()
                    .as_ref()
                    .cloned()
                    .map(PerfBpfTaskFdQueryName::Owned);
                let has_name = name.is_some();
                (
                    if retprobe {
                        thekernel_linux_bpf::BPF_FD_TYPE_KRETPROBE
                    } else {
                        thekernel_linux_bpf::BPF_FD_TYPE_KPROBE
                    },
                    name,
                    if has_name { query_offset } else { 0 },
                    if has_name { 0 } else { addr },
                )
            }
            PerfEvent::Uprobe {
                offset,
                retprobe,
                reference_counter_offset,
                ..
            } => (
                if retprobe {
                    thekernel_linux_bpf::BPF_FD_TYPE_URETPROBE
                } else {
                    thekernel_linux_bpf::BPF_FD_TYPE_UPROBE
                },
                self.probe_query_name
                    .lock()
                    .as_ref()
                    .cloned()
                    .map(PerfBpfTaskFdQueryName::Owned),
                offset,
                reference_counter_offset,
            ),
            _ => return Err(AxError::OperationNotSupported),
        };
        Ok(PerfBpfTaskFdQuery {
            prog_id: program.prog_id,
            fd_type,
            name,
            probe_offset,
            probe_addr,
        })
    }

    /// Emits a BPF payload into the preallocated perf data ring.  Producers
    /// only copy into that ring while holding its IRQ-safe lock; they neither
    /// allocate nor usercopy.  Full rings deliberately report `Ok(())` after
    /// charging LOST, matching perf's non-fatal producer overflow semantics.
    #[cfg(feature = "bpf")]
    pub(crate) fn emit_bpf_output(&self, data: &[u8]) -> AxResult<()> {
        if self.event != PerfEvent::Software(SoftwareEvent::BpfOutput) {
            return Err(AxError::InvalidInput);
        }
        self.sampling
            .as_ref()
            .ok_or(AxError::OperationNotSupported)?
            .emit_raw_record(data)
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn bpf_lost(&self) -> u64 {
        self.sampling
            .as_ref()
            .map_or(0, |backend| backend.lost_records())
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn bpf_generation(&self) -> u64 {
        self.bpf.lock().generation
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn bpf_read_value(&self) -> (u64, u64, u64) {
        self.sample()
            .map(|sample| (sample.value, sample.enabled, sample.running))
            .unwrap_or((0, 0, 0))
    }

    /// Executes an attached PERF_EVENT program at a real perf producer.  Its
    /// stable, read-only context is: event id, timestamp, CPU, source kind,
    /// and source-specific detail. It intentionally contains no kernel
    /// pointers, registers, or mutable scheduler state.
    #[cfg(feature = "bpf")]
    pub(crate) fn run_attached_bpf(&self, source: u32, detail: u64) {
        let program = self.bpf.lock().program.clone();
        let Some(program) = program else {
            return;
        };
        let mut context = [0u8; 32];
        context[..8].copy_from_slice(&self.id.to_ne_bytes());
        context[8..16].copy_from_slice(&monotonic_time_nanos().to_ne_bytes());
        context[16..20].copy_from_slice(&(axhal::percpu::this_cpu_id() as u32).to_ne_bytes());
        context[20..24].copy_from_slice(&source.to_ne_bytes());
        context[24..].copy_from_slice(&detail.to_ne_bytes());
        // struct_ops tables are separate from PERF_EVENT links: they observe
        // the same producer context but cannot replace or detach this event's
        // explicitly attached program.
        crate::bpf::run_struct_ops(&mut context);
        // Program failures are contained to the current producer; capability
        // and target authority remain the open-time perf snapshot.
        let stats = crate::bpf::prog::BpfStatsRunGuard::begin();
        let _ = crate::bpf::helpers::BpfExecution::new(&mut context, &program.maps, 4096)
            .with_streams(&program.streams)
            .execute(&program.mechanism);
        program.account_run(&stats);
    }
    pub(crate) fn is_group_leader(&self) -> bool {
        self.group().is_some_and(|group| group.is_leader(self.id))
    }
    fn enabled(&self) -> bool {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = self.sampling.as_ref() {
            return backend.enabled();
        }
        self.state.lock().enabled
    }
    #[cfg(feature = "pmu")]
    fn counting_event(&self) -> Option<axhal::pmu::Event> {
        #[cfg(feature = "perf-sampling")]
        if self.sampling.is_some() {
            return None;
        }
        match self.event {
            PerfEvent::Hardware(HardwareEvent::Cycles) => Some(axhal::pmu::Event::Cycles),
            PerfEvent::Hardware(HardwareEvent::Instructions) => {
                Some(axhal::pmu::Event::Instructions)
            }
            PerfEvent::Hardware(HardwareEvent::Architectural {
                event_select,
                availability_bit,
            }) => Some(axhal::pmu::Event::Architectural {
                event_select,
                availability_bit,
            }),
            PerfEvent::Raw {
                config, core_type, ..
            } => Some(axhal::pmu::Event::Raw {
                event_select: config,
                core_type: match core_type {
                    1 => axhal::pmu::IntelCoreType::Core,
                    2 => axhal::pmu::IntelCoreType::Atom,
                    value => axhal::pmu::IntelCoreType::Unknown(value),
                },
            }),
            _ => None,
        }
    }
    #[cfg(feature = "pmu")]
    fn external_current(&self) -> AxResult<u64> {
        match self.event {
            PerfEvent::Uncore {
                box_type, box_id, ..
            } => {
                let owner = axhal::perf_uncore::owner_cpu(box_type, box_id)
                    .map_err(|_| AxError::OperationNotSupported)?;
                let lease = (*self.external_lease.lock()).ok_or(AxError::OperationNotSupported)?;
                if owner == axhal::percpu::this_cpu_id() {
                    axhal::perf_uncore::read_lease_owner_current(lease)
                        .map_err(|_| AxError::OperationNotSupported)
                } else {
                    let generation = NEXT_RECONCILE_GENERATION
                        .fetch_add(1, Ordering::Relaxed)
                        .max(1);
                    axhal::perf_uncore::publish_read_lease(owner, generation, lease)
                        .map_err(|_| AxError::OperationNotSupported)?;
                    if axhal::irq::send_ipi_reason(
                        axhal::irq::IpiReason::PerfReconcile,
                        axhal::irq::IpiTarget::Other { cpu_id: owner },
                    )
                    .is_err()
                    {
                        axhal::perf_uncore::cancel_reconcile(owner, generation);
                        return Err(AxError::OperationNotSupported);
                    }
                    for _ in 0..RECONCILE_ACK_SPINS {
                        if axhal::perf_uncore::reconcile_acknowledged(owner) == generation {
                            return axhal::perf_uncore::reconcile_result(owner, generation)
                                .map_err(|_| AxError::OperationNotSupported);
                        }
                        core::hint::spin_loop();
                    }
                    Err(AxError::Io)
                }
            }
            PerfEvent::ReadOnly { pmu, config } => {
                axhal::perf_uncore::read_readonly_current(pmu, config)
                    .map_err(|_| AxError::OperationNotSupported)
            }
            _ => Err(AxError::InvalidInput),
        }
    }
    #[cfg(feature = "pmu")]
    fn start_external_counter(&self) -> AxResult {
        if let PerfEvent::Uncore {
            box_type,
            box_id,
            config,
        } = self.event
        {
            let owner = axhal::perf_uncore::owner_cpu(box_type, box_id)
                .map_err(|_| AxError::OperationNotSupported)?;
            let lease = if owner == axhal::percpu::this_cpu_id() {
                axhal::perf_uncore::reserve_program_owner_current(box_type, box_id, config)
                    .map_err(|_| AxError::OperationNotSupported)?
            } else {
                let generation = NEXT_RECONCILE_GENERATION
                    .fetch_add(1, Ordering::Relaxed)
                    .max(1);
                axhal::perf_uncore::publish_reserve_program(
                    owner, generation, box_type, box_id, config,
                )
                .map_err(|_| AxError::OperationNotSupported)?;
                if axhal::irq::send_ipi_reason(
                    axhal::irq::IpiReason::PerfReconcile,
                    axhal::irq::IpiTarget::Other { cpu_id: owner },
                )
                .is_err()
                {
                    axhal::perf_uncore::cancel_reconcile(owner, generation);
                    return Err(AxError::OperationNotSupported);
                }
                let mut lease = None;
                for _ in 0..RECONCILE_ACK_SPINS {
                    if axhal::perf_uncore::reconcile_acknowledged(owner) == generation {
                        lease = Some(
                            axhal::perf_uncore::reconcile_lease_result(owner, generation)
                                .map_err(|_| AxError::OperationNotSupported)?,
                        );
                        break;
                    }
                    core::hint::spin_loop();
                }
                lease.ok_or(AxError::Io)?
            };
            *self.external_lease.lock() = Some(lease);
        }
        let baseline = match self.external_current() {
            Ok(baseline) => baseline,
            Err(error) => {
                // Programming and baseline capture are one ownership
                // transaction.  An uncore selector must never remain live
                // when its initial read failed.
                if matches!(self.event, PerfEvent::Uncore { .. }) {
                    self.restore_external_terminal();
                }
                return Err(error);
            }
        };
        self.external_baseline.store(baseline, Ordering::Release);
        self.external_running.store(true, Ordering::Release);
        Ok(())
    }
    #[cfg(feature = "pmu")]
    fn settle_external_counter(&self) {
        if !self.event.uses_external_counter()
            || !self.external_running.swap(false, Ordering::AcqRel)
        {
            return;
        }
        if let PerfEvent::Uncore {
            box_type, box_id, ..
        } = self.event
        {
            let Some(lease) = self.external_lease.lock().take() else {
                self.mark_invalid();
                return;
            };
            let current = axhal::perf_uncore::owner_cpu(box_type, box_id).and_then(|owner| {
                if owner == axhal::percpu::this_cpu_id() {
                    axhal::perf_uncore::settle_release_owner_current(lease)
                } else {
                    let generation = NEXT_RECONCILE_GENERATION
                        .fetch_add(1, Ordering::Relaxed)
                        .max(1);
                    axhal::perf_uncore::publish_settle_release(owner, generation, lease)?;
                    if axhal::irq::send_ipi_reason(
                        axhal::irq::IpiReason::PerfReconcile,
                        axhal::irq::IpiTarget::Other { cpu_id: owner },
                    )
                    .is_err()
                    {
                        axhal::perf_uncore::cancel_reconcile(owner, generation);
                        return Err(axhal::pmu::Error::Busy);
                    }
                    for _ in 0..RECONCILE_ACK_SPINS {
                        if axhal::perf_uncore::reconcile_acknowledged(owner) == generation {
                            return axhal::perf_uncore::reconcile_result(owner, generation);
                        }
                        core::hint::spin_loop();
                    }
                    Err(axhal::pmu::Error::Busy)
                }
            });
            match current {
                Ok(current) => {
                    let baseline = self.external_baseline.swap(current, Ordering::AcqRel);
                    self.add_count(Self::external_counter_delta(self.event, baseline, current));
                }
                Err(_) => self.mark_invalid(),
            }
            return;
        }
        match self.external_current() {
            Ok(current) => {
                let baseline = self.external_baseline.swap(current, Ordering::AcqRel);
                self.add_count(Self::external_counter_delta(self.event, baseline, current));
            }
            Err(_) => self.mark_invalid(),
        }
    }

    #[cfg(feature = "pmu")]
    fn external_counter_delta(event: PerfEvent, previous: u64, current: u64) -> u64 {
        match event {
            PerfEvent::ReadOnly {
                pmu: axhal::perf_uncore::ReadOnlyPmu::Power,
                ..
            } => axhal::perf_uncore::wrapping_counter_delta(previous, current, 32),
            _ => current.wrapping_sub(previous),
        }
    }
    fn start_running(&self, now: u64) {
        let mut state = self.state.lock();
        if state.enabled && !state.running {
            state.running = true;
            state.running_since = now;
            state.clock_accounted_since = now;
        }
    }

    /// Settle one running clock-source interval. This is called with the
    /// owning group locked at a scheduler tick/leave boundary, so it neither
    /// allocates nor performs usercopy.
    fn account_clock_until(&self, now: u64, user: bool) {
        if !matches!(
            self.event,
            PerfEvent::Software(SoftwareEvent::CpuClock | SoftwareEvent::TaskClock)
        ) {
            return;
        }
        let elapsed = {
            let mut state = self.state.lock();
            if !state.running || !state.enabled {
                return;
            }
            let elapsed = now.saturating_sub(state.clock_accounted_since);
            state.clock_accounted_since = now;
            state.clock_user = user;
            if (user && self.count_user.load(Ordering::Acquire))
                || (!user && self.count_kernel.load(Ordering::Acquire))
            {
                state.count = state.count.saturating_add(elapsed);
                elapsed
            } else {
                0
            }
        };
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = self.sampling.as_ref() {
            let _ = backend.account_source_time(current().id().as_u64() as u32, elapsed, user);
        }
    }

    fn stop_running(&self, now: u64) -> bool {
        let user = self.state.lock().clock_user;
        self.account_clock_until(now, user);
        let mut state = self.state.lock();
        if state.running {
            let elapsed = now.saturating_sub(state.running_since);
            state.running_total = state.running_total.saturating_add(elapsed);
            state.running = false;
            true
        } else {
            false
        }
    }
    fn running(&self) -> bool {
        self.state.lock().running
    }
    fn requires_owner_control(&self) -> bool {
        if self.event.uses_pmu() {
            return true;
        }
        #[cfg(feature = "perf-sampling")]
        if self.sampling.is_some()
            && matches!(
                self.event,
                PerfEvent::Software(SoftwareEvent::CpuClock | SoftwareEvent::TaskClock)
            )
        {
            return true;
        }
        false
    }
    fn add_count(&self, value: u64) {
        let mut state = self.state.lock();
        state.count = state.count.saturating_add(value);
    }
    fn mark_invalid(&self) {
        self.state.lock().invalid = true;
    }
    fn invalid(&self) -> bool {
        self.state.lock().invalid
    }
    fn enable_at(&self, now: u64) {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = self.sampling.as_ref() {
            backend.set_enabled_at(true, now);
            return;
        }
        let mut state = self.state.lock();
        if !state.enabled {
            state.enabled = true;
            state.enabled_since = now;
        }
    }
    fn disable_at(&self, now: u64) {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = self.sampling.as_ref() {
            backend.set_enabled_at(false, now);
            return;
        }
        let mut state = self.state.lock();
        if state.running {
            state.running_total = state
                .running_total
                .saturating_add(now.saturating_sub(state.running_since));
            state.running = false;
        }
        if state.enabled {
            state.enabled_total = state
                .enabled_total
                .saturating_add(now.saturating_sub(state.enabled_since));
            state.enabled = false;
        }
    }
    fn reset_at(&self, _: u64) {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = self.sampling.as_ref() {
            backend.reset_count();
            return;
        }
        let mut state = self.state.lock();
        state.count = 0;
        state.invalid = false;
    }
    fn sample(&self) -> AxResult<Sample> {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = self.sampling.as_ref() {
            let sample = backend.count_snapshot()?;
            return Ok(Sample {
                id: self.id,
                value: sample.value,
                enabled: sample.enabled,
                running: sample.running,
                lost: sample.lost,
            });
        }
        let now = monotonic_time_nanos();
        let state = self.state.lock();
        Ok(Sample {
            id: self.id,
            value: state.count,
            enabled: state.enabled_total.saturating_add(if state.enabled {
                now.saturating_sub(state.enabled_since)
            } else {
                0
            }),
            running: state.running_total.saturating_add(if state.running {
                now.saturating_sub(state.running_since)
            } else {
                0
            }),
            lost: 0,
        })
    }
    fn sample_with_live(&self, live: Option<u64>) -> AxResult<Sample> {
        let mut sample = self.sample()?;
        if let Some(live) = live {
            sample.value = compose_live_count(sample.value, live);
        }
        Ok(sample)
    }
    pub(crate) fn on_enter(&self) {
        if let Some(group) = self.group() {
            group.on_enter();
        }
    }
    pub(crate) fn on_leave(&self) {
        if let Some(group) = self.group() {
            group.on_leave();
        }
    }
    pub(crate) fn on_fault(&self) {
        if let Some(group) = self.group() {
            group.on_fault();
        }
    }
    fn read_samples(&self, dst: &mut IoDst) -> AxResult<usize> {
        let group_read = self.read.group;
        let group = self.group().ok_or(AxError::BadFileDescriptor)?;
        group.synchronize_hardware_control(self.id, group_read, RECONCILE_CONTROL_READ)?;
        let result = self.read_samples_settled(dst, &group, group_read);
        result
    }

    fn read_samples_settled(
        &self,
        dst: &mut IoDst,
        group: &Arc<PerfGroup>,
        group_read: bool,
    ) -> AxResult<usize> {
        let mut samples = Vec::new();
        if group_read {
            samples
                .try_reserve(MAX_GROUP_MEMBERS)
                .map_err(|_| AxError::NoMemory)?;
            group.snapshots(&mut samples, true)?;
        } else {
            samples.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            samples.push(group.snapshot_member(self.id, true)?);
        }
        let ids = self.read.id;
        let lost = self.read.lost;
        let timing = self.read.time_enabled as usize + self.read.time_running as usize;
        let words = if group_read {
            (1 + timing)
                .checked_add(
                    samples
                        .len()
                        .checked_mul(1 + ids as usize + lost as usize)
                        .ok_or(AxError::InvalidInput)?,
                )
                .ok_or(AxError::InvalidInput)?
        } else {
            1 + timing + ids as usize + lost as usize
        };
        let bytes = words
            .checked_mul(size_of::<u64>())
            .ok_or(AxError::InvalidInput)?;
        if dst.remaining_mut() < bytes {
            return Err(AxError::InvalidInput);
        }
        let leader = if group_read {
            samples
                .iter()
                .copied()
                .find(|sample| sample.id == group.leader_id)
                .ok_or(AxError::BadFileDescriptor)?
        } else {
            samples[0]
        };
        if group_read {
            dst.write(&(samples.len() as u64).to_ne_bytes())?;
            if self.read.time_enabled {
                dst.write(&leader.enabled.to_ne_bytes())?;
            }
            if self.read.time_running {
                dst.write(&leader.running.to_ne_bytes())?;
            }
            for sample in samples {
                dst.write(&sample.value.to_ne_bytes())?;
                if ids {
                    dst.write(&sample.id.to_ne_bytes())?;
                }
                if lost {
                    dst.write(&sample.lost.to_ne_bytes())?;
                }
            }
        } else {
            dst.write(&leader.value.to_ne_bytes())?;
            if self.read.time_enabled {
                dst.write(&leader.enabled.to_ne_bytes())?;
            }
            if self.read.time_running {
                dst.write(&leader.running.to_ne_bytes())?;
            }
            if ids {
                dst.write(&leader.id.to_ne_bytes())?;
            }
            if lost {
                dst.write(&leader.lost.to_ne_bytes())?;
            }
        }
        Ok(bytes)
    }
}

impl FileLike for PerfEventFile {
    fn pre_close(&self) {
        if self.perf_map_refs.load(Ordering::Acquire) != 0 {
            return;
        }
        self.release_dynamic_source();
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling {
            // Sampling custody has its own NMI-safe stop primitive, but the
            // descriptor is still this unified PerfEventFile.  Do not route
            // it through counting's completion array: that lane deliberately
            // has no sampling token ownership.
            if self.output_users.load(Ordering::Acquire) == 0 {
                backend.final_close();
            }
        }
        if let Some(group) = self.group() {
            group.retire_member(self.id);
        }
    }

    fn final_close(&self) {
        self.descriptor_final_closed.store(true, Ordering::Release);
        self.finish_output_target_close();
        if self.perf_map_refs.load(Ordering::Acquire) != 0 {
            return;
        }
        self.finish_map_held_close();
        // `FileDescription::drop` may execute from an interrupt context.  It
        // cannot wait for a remote IPI; retain the frozen, invalid state as a
        // fail-closed fallback and let the owner CPU's leave path release the
        // only remaining hardware custody.
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let result = self.read_samples(dst);
        if result.is_ok() {
            self.emit_read();
        }
        result
    }
    fn write(&self, _: &mut IoSrc) -> AxResult<usize> {
        Err(AxError::BadFileDescriptor)
    }
    fn prepare_mmap(
        &self,
        request: crate::file::FileMmapRequest,
    ) -> AxResult<Option<crate::file::PreparedFileMmap>> {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling {
            let mapping = backend.prepare_mmap(request)?;
            if let Some(group) = self.group() {
                group.reconfigure_current();
            }
            return Ok(mapping);
        }
        Ok(None)
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling
            && cmd == PERF_EVENT_IOC_MODIFY_ATTRIBUTES
        {
            // All usercopy, planning, capability and exact-capture backing
            // allocation is complete before the group reaches its stopped
            // owner-CPU boundary.
            let prepared = backend.prepare_modify_attributes(context, arg)?;
            if let Some(group) = self.group() {
                group.begin_sampling_modify()?;
                let result = backend.commit_modify_attributes(prepared);
                group.finish_sampling_modify();
                result?;
            } else {
                backend.commit_modify_attributes(prepared)?;
            }
            return Ok(0);
        }
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling
            && !matches!(
                cmd,
                PERF_EVENT_IOC_SET_BPF
                    | PERF_EVENT_IOC_QUERY_BPF
                    | PERF_EVENT_IOC_SET_OUTPUT
                    | PERF_EVENT_IOC_ENABLE
                    | PERF_EVENT_IOC_DISABLE
                    | PERF_EVENT_IOC_RESET
            )
        {
            let result = backend.ioctl(context, cmd, arg)?;
            if cmd != PERF_EVENT_IOC_DISABLE {
                if let Some(group) = self.group() {
                    group.reconfigure_current();
                }
            }
            return Ok(result);
        }
        if matches!(
            cmd,
            PERF_EVENT_IOC_ENABLE
                | PERF_EVENT_IOC_DISABLE
                | PERF_EVENT_IOC_REFRESH
                | PERF_EVENT_IOC_RESET
        ) && arg & !PERF_IOC_FLAG_GROUP != 0
        {
            return Err(AxError::InvalidInput);
        }
        if cmd == PERF_EVENT_IOC_REFRESH {
            return Err(AxError::OperationNotSupported);
        }
        let perf_group = self.group().ok_or(AxError::BadFileDescriptor)?;

        match cmd {
            PERF_EVENT_IOC_ENABLE => {
                perf_group.synchronize_hardware_control(
                    self.id,
                    arg & PERF_IOC_FLAG_GROUP != 0,
                    RECONCILE_CONTROL_ENABLE,
                )?;
                Ok(0)
            }
            PERF_EVENT_IOC_DISABLE => {
                perf_group.synchronize_hardware_control(
                    self.id,
                    arg & PERF_IOC_FLAG_GROUP != 0,
                    RECONCILE_CONTROL_DISABLE,
                )?;
                #[cfg(feature = "pmu")]
                self.restore_external_terminal();
                Ok(0)
            }
            PERF_EVENT_IOC_RESET => {
                perf_group.synchronize_hardware_control(
                    self.id,
                    arg & PERF_IOC_FLAG_GROUP != 0,
                    RECONCILE_CONTROL_RESET,
                )?;
                Ok(0)
            }
            PERF_EVENT_IOC_ID => {
                context
                    .user_memory()
                    .write_value(arg as *mut u64, self.id)
                    .map_err(map_usercopy_error)?;
                Ok(0)
            }
            PERF_EVENT_IOC_SET_OUTPUT => {
                if arg == usize::MAX {
                    self.clear_output_target();
                    return Ok(0);
                }
                let fd = i32::try_from(arg).map_err(|_| AxError::InvalidInput)?;
                let target = context
                    .get_file_like(fd)?
                    .downcast::<PerfEventFile>()?
                    .file
                    .clone();
                self.set_output_target(target)?;
                Ok(0)
            }
            #[cfg(feature = "bpf")]
            PERF_EVENT_IOC_SET_BPF => {
                if arg == usize::MAX {
                    self.clear_bpf_program();
                    return Ok(0);
                }
                let fd = i32::try_from(arg).map_err(|_| AxError::InvalidInput)?;
                let program = context
                    .get_file_like(fd)?
                    .downcast::<crate::file::bpf::BpfProgFd>()?
                    .prog
                    .clone();
                if program.prog_type != crate::bpf::defs::BPF_PROG_TYPE_PERF_EVENT {
                    return Err(AxError::InvalidInput);
                }
                // `perf_event_open` already froze target/cgroup authority in
                // this descriptor.  Attaching a program cannot modify that
                // immutable context, and the verifier exposes only the
                // read-only perf BPF context below.
                self.set_bpf_program(program)?;
                Ok(0)
            }
            #[cfg(feature = "bpf")]
            PERF_EVENT_IOC_QUERY_BPF => {
                if arg == 0 {
                    return Err(AxError::InvalidInput);
                }
                let ids_len = context
                    .user_memory()
                    .read_value(arg as *const u32)
                    .map_err(map_usercopy_error)?;
                let id = self.bpf_program_id();
                context
                    .user_memory()
                    .write_value(
                        arg.checked_add(core::mem::size_of::<u32>())
                            .ok_or(AxError::InvalidInput)? as *mut u32,
                        u32::from(id.is_some()),
                    )
                    .map_err(map_usercopy_error)?;
                if let Some(id) = id {
                    if ids_len == 0 {
                        return Ok(0);
                    }
                    let ids = arg
                        .checked_add(2 * core::mem::size_of::<u32>())
                        .ok_or(AxError::InvalidInput)?;
                    context
                        .user_memory()
                        .write_value(ids as *mut u32, id)
                        .map_err(map_usercopy_error)?;
                }
                Ok(0)
            }
            #[cfg(not(feature = "bpf"))]
            PERF_EVENT_IOC_SET_BPF | PERF_EVENT_IOC_QUERY_BPF => {
                Err(AxError::OperationNotSupported)
            }
            _ => Err(AxError::InvalidInput),
        }
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

impl Drop for PerfEventFile {
    fn drop(&mut self) {
        // Covers open rollback and descriptor-table allocation failure, where
        // `pre_close` is never reached. The atomic makes this idempotent with
        // the normal close path.
        self.release_dynamic_source();
        let _routing = OUTPUT_ROUTING_LOCK.lock();
        #[cfg(feature = "perf-sampling")]
        if let Some(source) = &self.sampling {
            source.clear_output_target_transaction();
        }
        if let Some(target) = self.output_target.get_mut().take()
            && target.output_users.fetch_sub(1, Ordering::AcqRel) == 1
        {
            drop(_routing);
            target.finish_output_target_close();
        }
    }
}
impl Pollable for PerfEventFile {
    fn poll(&self) -> IoEvents {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling {
            return backend.poll();
        }
        IoEvents::READABLE
    }
    fn register<'a>(
        &'a self,
        cx: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        #[cfg(feature = "perf-sampling")]
        if let Some(backend) = &self.sampling {
            return backend.register(cx, events);
        }
        PollRegistration::empty()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec::Vec};
    use core::sync::atomic::Ordering;

    use thekernel_linux_perf::ReadPlan;

    use super::{
        ExtendedSolverCapacity, ExtendedSolverConstraints, HardwareEvent, HybridEventAdmission,
        PerfContext, PerfEvent, PerfEventFile, PerfGroup, SoftwareEvent, SolverCapacity,
        SolverGroup, SolverResult, admit_hybrid_event, next_flexible_group,
        solve_extended_constraints, solve_group,
    };

    const NO_READ: ReadPlan = ReadPlan {
        group: false,
        time_enabled: false,
        time_running: false,
        id: false,
        lost: false,
    };

    #[test]
    fn group_rejects_duplicate_hardware_event() {
        let group = PerfGroup::new(1, 1).unwrap();
        let first = PerfEventFile::new(
            1,
            PerfEvent::Hardware(HardwareEvent::Cycles),
            true,
            &group,
            NO_READ,
        )
        .unwrap();
        assert!(
            PerfEventFile::new(
                2,
                PerfEvent::Hardware(HardwareEvent::Cycles),
                true,
                &group,
                NO_READ
            )
            .is_err()
        );
        drop(first);
    }

    #[test]
    fn running_software_group_accepts_enable_reset_and_disable() {
        let group = PerfGroup::new(1, 1).unwrap();
        let file = PerfEventFile::new(
            1,
            PerfEvent::Software(SoftwareEvent::CpuMigrations),
            true,
            &group,
            NO_READ,
        )
        .unwrap();
        {
            let mut state = group.state.lock();
            state.active.task_active = true;
            // Software control must also work for a remotely active group.
            state.active.cpu = Some(usize::MAX);
        }
        for _ in 0..2 {
            group
                .synchronize_hardware_control(file.id, false, super::RECONCILE_CONTROL_ENABLE)
                .unwrap();
            assert!(file.running());
            file.add_count(7);
            group
                .synchronize_hardware_control(file.id, false, super::RECONCILE_CONTROL_RESET)
                .unwrap();
            assert_eq!(file.state.lock().count, 0);
            assert!(file.running());
            group
                .synchronize_hardware_control(file.id, false, super::RECONCILE_CONTROL_DISABLE)
                .unwrap();
            assert!(!file.running());
            assert!(!file.enabled());
            assert!(!group.state.lock().active.running);
            assert!(group.state.lock().active.files.iter().all(Option::is_none));
        }
    }

    #[test]
    fn disabled_software_group_releases_closed_member_without_task_switch() {
        let group = PerfGroup::new(1, 1).unwrap();
        let file = PerfEventFile::new(
            1,
            PerfEvent::Software(SoftwareEvent::CpuMigrations),
            true,
            &group,
            NO_READ,
        )
        .unwrap();
        let weak_file = Arc::downgrade(&file);
        {
            let mut state = group.state.lock();
            state.active.task_active = true;
            state.active.cpu = Some(usize::MAX);
        }
        group
            .synchronize_hardware_control(file.id, false, super::RECONCILE_CONTROL_ENABLE)
            .unwrap();
        group
            .synchronize_hardware_control(file.id, false, super::RECONCILE_CONTROL_DISABLE)
            .unwrap();
        group.reconcile_last_descriptor();
        drop(file);
        // Closing must release the descriptor immediately; leaving the task
        // then makes the group eligible for registry quota reclamation.
        assert!(weak_file.upgrade().is_none());
        group.on_leave();
        assert!(group.is_prunable());
        assert!(group.state.lock().members.is_empty());
        assert!(group.state.lock().active.files.is_empty());
    }

    #[test]
    fn disabled_software_groups_do_not_exhaust_registry_quota_without_task_switch() {
        let _context = crate::test_support::scheduler_test_context();
        for id in 1..=(super::MAX_CPU_CONTEXT_GROUPS as u64 * 2) {
            let group = PerfGroup::new_for_context(PerfContext::Cpu { cpu: 0 }, id).unwrap();
            let file = PerfEventFile::new(
                id,
                PerfEvent::Software(SoftwareEvent::CpuMigrations),
                true,
                &group,
                NO_READ,
            )
            .unwrap();
            PerfGroup::attach_cpu_context(&group).unwrap();
            let description = crate::file::FileDescription::new(file.clone()).unwrap();
            description.mark_open_committed();
            description.begin_descriptor_publication().unwrap().commit();
            group.on_enter();
            group
                .synchronize_hardware_control(id, false, super::RECONCILE_CONTROL_ENABLE)
                .unwrap();
            group
                .synchronize_hardware_control(id, false, super::RECONCILE_CONTROL_DISABLE)
                .unwrap();
            description.descriptor_closed();
            drop(description);
            drop(file);
            assert!(group.state.lock().active.task_active);
            assert!(group.is_prunable());
            // Leave reclamation to the real registry's next open, so a
            // leaked descriptor exhausts its 64 entries on iteration 65.
        }
        super::CPU_CONTEXT_GROUPS[0].lock().retain(|group| !group.is_prunable());
    }

    #[test]
    fn closing_group_member_keeps_survivor_counting_without_task_switch() {
        let _context = crate::test_support::scheduler_test_context();
        // Exercise both member and leader close through the real OFD
        // lifecycle; membership removal must not depend on the final Arc
        // disappearing, because close itself still holds the file alive.
        for close_leader in [false, true] {
            let group = PerfGroup::new(1, 1).unwrap();
            let leader = PerfEventFile::new(
                1,
                PerfEvent::Software(SoftwareEvent::CpuMigrations),
                true,
                &group,
                NO_READ,
            )
            .unwrap();
            let child = PerfEventFile::new(
                2,
                PerfEvent::Software(SoftwareEvent::ContextSwitches),
                true,
                &group,
                NO_READ,
            )
            .unwrap();
            let (closing, survivor) = if close_leader {
                (&leader, &child)
            } else {
                (&child, &leader)
            };
            let description = crate::file::FileDescription::new(closing.clone()).unwrap();
            description.mark_open_committed();
            description.begin_descriptor_publication().unwrap().commit();
            group.on_enter();
            group
                .synchronize_hardware_control(1, true, super::RECONCILE_CONTROL_ENABLE)
                .unwrap();
            description.descriptor_closed();
            drop(description);
            assert!(!closing.enabled());
            assert!(!closing.running());
            assert!(survivor.running());
            assert!(!group.state.lock().active.reconcile_frozen);
            assert_eq!(group.state.lock().members.len(), 1);
            let event = match survivor.event {
                PerfEvent::Software(event) => event,
                _ => unreachable!(),
            };
            let before = survivor.state.lock().count;
            group.on_software_event(event);
            assert_eq!(survivor.state.lock().count, before + 1);
            group.on_leave();
        }
    }

    #[cfg(feature = "perf-sampling")]
    #[test]
    fn running_software_tracepoint_wrapper_can_enable_disabled_backend() {
        use crate::file::perf_sampling::{PerfOpenIdentity, SamplingConfig, SamplingEvent};
        let _context = crate::test_support::scheduler_test_context();
        let group = PerfGroup::new_for_context(PerfContext::Cpu { cpu: 0 }, 1).unwrap();
        let backend = crate::file::PerfSampleBackend::try_new(SamplingConfig {
            id: 1,
            target_task_id: 0,
            event: SamplingEvent::Source,
            period: 1,
            frequency: None,
            sample_type: thekernel_linux_perf::PERF_SAMPLE_TIME,
            count_user: true,
            count_kernel: true,
            disabled: true,
            read_format: 0,
            aux: None,
            identity: PerfOpenIdentity {
                attr: thekernel_linux_perf::PerfEventAttr::default(),
                target: thekernel_linux_perf::PerfOpenTarget {
                    target: thekernel_linux_perf::PerfTarget::Cpu { cpu: 0 },
                    group_fd: -1,
                    output_fd: -1,
                    open_flags: 0,
                },
                authority: crate::perf_security::PerfAuthority::Restricted,
            },
        })
        .unwrap();
        let file = PerfEventFile::new_sampling(
            1,
            PerfEvent::Tracepoint(1),
            &group,
            NO_READ,
            thekernel_linux_perf::PerfLifecycle::default(),
            backend.clone(),
        )
        .unwrap();
        {
            let mut state = group.state.lock();
            state.active.task_active = true;
            state.active.cpu = Some(usize::MAX);
            PerfGroup::start_locked(&mut state, super::monotonic_time_nanos());
            assert!(state.active.running);
        }
        assert!(!backend.enabled());
        assert!(!file.requires_owner_control());
        // Sharing the same source backend with a clock event requires the
        // owner's CPU/task context when settlement emits a pending sample.
        let clock_group = PerfGroup::new(1, 2).unwrap();
        let clock = PerfEventFile::new_sampling(
            2,
            PerfEvent::Software(SoftwareEvent::CpuClock),
            &clock_group,
            NO_READ,
            thekernel_linux_perf::PerfLifecycle::default(),
            backend.clone(),
        )
        .unwrap();
        assert!(clock.requires_owner_control());
        {
            let mut state = clock_group.state.lock();
            state.active.running = true;
        }
        assert_eq!(
            clock_group.control_local_inactive(clock.id, false, super::RECONCILE_CONTROL_ENABLE),
            Err(axerrno::AxError::Io)
        );
        group
            .synchronize_hardware_control(file.id, false, super::RECONCILE_CONTROL_ENABLE)
            .unwrap();
        assert!(backend.enabled());
        group
            .synchronize_hardware_control(file.id, false, super::RECONCILE_CONTROL_DISABLE)
            .unwrap();
        assert!(!backend.enabled());
    }

    #[test]
    fn cpu_group_publication_requires_live_member_before_registry_pruning() {
        let _context = crate::test_support::scheduler_test_context();
        let group = PerfGroup::new_for_context(PerfContext::Cpu { cpu: 0 }, 1).unwrap();
        assert_eq!(PerfGroup::attach_cpu_context(&group), Err(super::AxError::InvalidInput));
        let file = PerfEventFile::new(
            1,
            PerfEvent::Software(SoftwareEvent::CpuMigrations),
            true,
            &group,
            NO_READ,
        )
        .unwrap();
        PerfGroup::attach_cpu_context(&group).unwrap();
        // This is the registry sweep that used to run between attaching the
        // empty group and creating its first descriptor member.
        super::CPU_CONTEXT_GROUPS[0].lock().retain(|entry| !entry.is_prunable());
        drop(group);
        let retained = file.group().expect("registry must retain an unopened live member");
        retained
            .synchronize_hardware_control(file.id, false, super::RECONCILE_CONTROL_ENABLE)
            .unwrap();
        PerfGroup::detach_empty_cpu_context(&retained);
    }

    #[test]
    fn file_holds_only_weak_group_reference() {
        let group = PerfGroup::new(1, 1).unwrap();
        let file = PerfEventFile::new(
            1,
            PerfEvent::Hardware(HardwareEvent::Instructions),
            true,
            &group,
            NO_READ,
        )
        .unwrap();
        assert_eq!(Arc::strong_count(&group), 1);
        drop(file);
    }

    #[test]
    fn live_count_composition_saturates_without_settlement() {
        assert_eq!(super::compose_live_count(7, 9), 16);
        assert_eq!(super::compose_live_count(u64::MAX - 1, 8), u64::MAX);
    }

    #[test]
    fn reset_clears_sticky_counter_fault() {
        let group = PerfGroup::new(1, 1).unwrap();
        let file = PerfEventFile::new(
            1,
            PerfEvent::Hardware(HardwareEvent::Cycles),
            true,
            &group,
            NO_READ,
        )
        .unwrap();
        file.mark_invalid();
        assert!(file.invalid());
        file.reset_at(0);
        assert!(!file.invalid());
    }

    #[test]
    fn inactive_group_does_not_start_counter_window() {
        let mut active = super::ActiveGroup::new();
        assert!(!active.task_active);
        assert!(!active.running);
        active.task_active = true;
        assert!(!active.running);
    }

    #[test]
    fn placement_generation_rejects_a_stale_remote_ack() {
        let group = PerfGroup::new(1, 1).unwrap();
        let first = {
            let mut state = group.state.lock();
            state.active.cpu = Some(0);
            group.advance_generation_locked(&mut state)
        };
        assert!(group.accepts_reconcile_generation(first));
        let second = {
            let mut state = group.state.lock();
            // Model a migration which has completed before the old CPU sees
            // its queued reconcile request.
            state.active.cpu = Some(1);
            group.advance_generation_locked(&mut state)
        };
        assert_ne!(first, second);
        assert!(!group.accepts_reconcile_generation(first));
        assert!(group.accepts_reconcile_generation(second));
        assert_eq!(group.placement_for_test(), (Some(1), second));
    }

    #[test]
    fn group_placement_generation_never_publishes_zero() {
        let group = PerfGroup::new(1, 1).unwrap();
        let generation = {
            let mut state = group.state.lock();
            state.active.generation = u64::MAX;
            group.advance_generation_locked(&mut state)
        };
        // Generations come from the global allocator, so preceding groups
        // may already have consumed the first value.
        assert_ne!(generation, 0);
        assert!(group.accepts_reconcile_generation(generation));
    }

    #[cfg(all(feature = "perf-sampling", target_os = "none"))]
    #[test]
    fn reconcile_mailbox_rejects_stale_ack_and_keeps_exact_ack() {
        let mailbox = super::PerfReconcileMailbox::new();
        mailbox.desired.store(7, Ordering::Release);
        assert!(mailbox.acknowledge_if_current(7));
        mailbox.desired.store(8, Ordering::Release);
        assert!(!mailbox.acknowledge_if_current(7));
        assert_eq!(mailbox.acknowledged.load(Ordering::Acquire), 7);
    }

    #[test]
    fn reconcile_freeze_prevents_a_concurrent_replacement_placement() {
        let mut active = super::ActiveGroup::new();
        active.task_active = true;
        active.reconcile_frozen = true;
        let mut state = super::GroupState {
            members: Vec::new(),
            active,
        };
        super::PerfGroup::start_locked(&mut state, 1);
        assert!(!state.active.running);
    }

    #[test]
    fn group_read_rejects_a_closed_leader_instead_of_using_child_time() {
        let group = PerfGroup::new(1, 1).unwrap();
        let leader = PerfEventFile::new(
            1,
            PerfEvent::Software(SoftwareEvent::CpuClock),
            true,
            &group,
            NO_READ,
        )
        .unwrap();
        let child = PerfEventFile::new(
            2,
            PerfEvent::Software(SoftwareEvent::TaskClock),
            true,
            &group,
            NO_READ,
        )
        .unwrap();
        drop(leader);
        let mut samples = Vec::new();
        samples.try_reserve_exact(super::MAX_GROUP_MEMBERS).unwrap();
        assert!(group.snapshots(&mut samples, false).is_err());
        drop(child);
    }

    #[test]
    fn solver_places_a_group_atomically_or_not_at_all() {
        let capacity = SolverCapacity {
            fixed: 1,
            programmable: 2,
        };
        assert_eq!(
            solve_group(
                capacity,
                SolverGroup {
                    id: 1,
                    fixed: 1,
                    programmable: 2,
                    pinned: true,
                    exclusive: false
                },
                None,
            ),
            SolverResult::Placed
        );
        assert_eq!(
            solve_group(
                capacity,
                SolverGroup {
                    id: 2,
                    fixed: 2,
                    programmable: 0,
                    pinned: false,
                    exclusive: false
                },
                None,
            ),
            SolverResult::Rejected
        );
    }

    #[test]
    fn extended_solver_keeps_pebs_lbr_offcore_topdown_and_smt_atomic() {
        let capacity = ExtendedSolverCapacity {
            pebs_counter_mask: 0b0110,
            offcore_slots: 1,
            topdown_slots: 1,
            smt_shared_slots: 1,
        };
        let pebs_lbr = ExtendedSolverConstraints {
            pebs_counter_mask: 0b0010,
            needs_lbr: true,
            offcore_slots: 1,
            needs_topdown: true,
            smt_shared_slots: 1,
        };
        assert_eq!(
            solve_extended_constraints(capacity, pebs_lbr, None, true),
            SolverResult::Placed
        );
        assert_eq!(
            solve_extended_constraints(capacity, pebs_lbr, Some(pebs_lbr), true),
            SolverResult::Rejected
        );
        assert_eq!(
            solve_extended_constraints(capacity, pebs_lbr, Some(pebs_lbr), false),
            SolverResult::Flexible
        );
        assert_eq!(
            solve_extended_constraints(
                capacity,
                ExtendedSolverConstraints {
                    pebs_counter_mask: 0b1000,
                    ..pebs_lbr
                },
                None,
                false,
            ),
            SolverResult::Rejected
        );
    }

    #[test]
    fn hybrid_raw_never_crosses_core_type_and_generic_needs_both_semantics() {
        assert!(admit_hybrid_event(
            HybridEventAdmission::Raw { core_type: 1 },
            1
        ));
        assert!(!admit_hybrid_event(
            HybridEventAdmission::Raw { core_type: 1 },
            2
        ));
        assert!(admit_hybrid_event(
            HybridEventAdmission::Generic {
                semantic_on_core: true,
                semantic_on_atom: true
            },
            1,
        ));
        assert!(!admit_hybrid_event(
            HybridEventAdmission::Generic {
                semantic_on_core: true,
                semantic_on_atom: false
            },
            2,
        ));
    }

    #[test]
    fn solver_rejects_pinned_overcommit_and_rotates_flexible_groups_fairly() {
        let capacity = SolverCapacity {
            fixed: 0,
            programmable: 1,
        };
        let running = SolverGroup {
            id: 1,
            fixed: 0,
            programmable: 1,
            pinned: false,
            exclusive: false,
        };
        let pinned = SolverGroup {
            id: 2,
            fixed: 0,
            programmable: 1,
            pinned: true,
            exclusive: false,
        };
        assert_eq!(
            solve_group(capacity, pinned, Some(running)),
            SolverResult::Rejected
        );
        let flexible = SolverGroup {
            pinned: false,
            ..pinned
        };
        assert_eq!(
            solve_group(capacity, flexible, Some(running)),
            SolverResult::Flexible
        );
        let mut cursor = 0;
        assert_eq!(
            next_flexible_group(&[running, flexible], &mut cursor),
            Some(running)
        );
        assert_eq!(
            next_flexible_group(&[running, flexible], &mut cursor),
            Some(flexible)
        );
        assert_eq!(
            next_flexible_group(&[running, flexible], &mut cursor),
            Some(running)
        );
    }

    #[test]
    fn exclusive_group_excludes_a_second_group_and_context_keeps_cpu_binding() {
        let capacity = SolverCapacity {
            fixed: 1,
            programmable: 1,
        };
        let exclusive = SolverGroup {
            id: 1,
            fixed: 0,
            programmable: 1,
            pinned: true,
            exclusive: true,
        };
        let peer = SolverGroup {
            id: 2,
            fixed: 0,
            programmable: 1,
            pinned: true,
            exclusive: false,
        };
        assert_eq!(
            solve_group(capacity, peer, Some(exclusive)),
            SolverResult::Rejected
        );
        let group =
            PerfGroup::new_for_context(PerfContext::TaskOnCpu { task_id: 7, cpu: 3 }, 1).unwrap();
        assert_eq!(
            group.context(),
            PerfContext::TaskOnCpu { task_id: 7, cpu: 3 }
        );
        assert!(group.accepts_target(7));
    }

    #[test]
    fn cgroup_context_is_cpu_scoped_without_task_target_aliasing() {
        let group = PerfGroup::new_for_context(
            PerfContext::Cgroup {
                cgroup_id: 0xfeed,
                cpu: 2,
            },
            1,
        )
        .unwrap();
        assert_eq!(group.context().cpu(), Some(2));
        assert!(!group.accepts_target(7));
    }

    #[test]
    fn exec_lifecycle_removal_disables_and_invalidates_immediately() {
        let group = PerfGroup::new(1, 1).unwrap();
        let file = PerfEventFile::new_with_lifecycle(
            1,
            PerfEvent::Software(SoftwareEvent::CpuClock),
            false,
            &group,
            NO_READ,
            thekernel_linux_perf::PerfLifecycle {
                remove_on_exec: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(file.apply_exec_lifecycle(10));
        assert!(!file.enabled());
        assert!(file.invalid());
    }

    #[test]
    fn inherit_creates_child_owned_group_without_descriptor_aliasing() {
        let parent = PerfGroup::new(7, 1).unwrap();
        let _file = PerfEventFile::new_with_lifecycle(
            1,
            PerfEvent::Software(SoftwareEvent::TaskClock),
            false,
            &parent,
            NO_READ,
            thekernel_linux_perf::PerfLifecycle {
                inherit: true,
                ..Default::default()
            },
        )
        .unwrap();
        let child = parent.inherit_for_child(8, false).unwrap().unwrap();
        assert!(child.accepts_target(8));
        assert_ne!(child.leader_id, parent.leader_id);
        assert_eq!(child.state.lock().members.len(), 1);
        assert!(child.state.lock().members[0].inherited.is_some());
    }

    // This is the ownership state machine shared by SET_BPF, QUERY_BPF and
    // BPF link close.  Keep it as a small host model: constructing a real
    // verifier-approved BPF program is intentionally outside the perf unit
    // tests, while the stale-generation contract is independent of bytecode.
    #[derive(Clone, Copy)]
    struct BpfAttachmentModel {
        generation: u64,
        program: Option<u32>,
        link_owned: bool,
    }

    impl BpfAttachmentModel {
        fn attach_link(&mut self, program: u32) -> u64 {
            assert!(self.program.is_none());
            self.program = Some(program);
            self.link_owned = true;
            self.bump()
        }

        fn set_ioctl(&mut self, program: Option<u32>) {
            self.program = program;
            self.link_owned = false;
            self.bump();
        }

        fn query(&self, ids_len: u32) -> (u32, Option<u32>) {
            (
                u32::from(self.program.is_some()),
                (ids_len != 0).then_some(self.program).flatten(),
            )
        }

        fn close_link_if_current(&mut self, generation: u64) {
            if self.link_owned && self.generation == generation {
                self.program = None;
                self.link_owned = false;
                self.bump();
            }
        }

        fn bump(&mut self) -> u64 {
            self.generation = self.generation.wrapping_add(1).max(1);
            self.generation
        }
    }

    #[test]
    fn bpf_attach_query_detach_and_link_close_are_generation_exact() {
        let mut model = BpfAttachmentModel {
            generation: 0,
            program: None,
            link_owned: false,
        };
        let link_generation = model.attach_link(41);
        assert_eq!(model.query(0), (1, None));
        assert_eq!(model.query(1), (1, Some(41)));

        // A direct ioctl replacement supersedes the old link. Its final
        // close must not detach the replacement program.
        model.set_ioctl(Some(99));
        model.close_link_if_current(link_generation);
        assert_eq!(model.query(1), (1, Some(99)));

        model.set_ioctl(None);
        assert_eq!(model.query(1), (0, None));
        let current_link = model.attach_link(7);
        model.close_link_if_current(current_link);
        assert_eq!(model.query(1), (0, None));
    }

    #[test]
    fn source_close_and_reconcile_generation_do_not_wait_for_a_tick() {
        let group = PerfGroup::new(1, 1).unwrap();
        let generation = {
            let mut state = group.state.lock();
            state.active.cpu = Some(0);
            state.active.running = true;
            group.advance_generation_locked(&mut state)
        };
        // Closing invalidates the exact active generation. A queued old IPI
        // cannot settle a later placement and no scheduler tick is needed to
        // make the generation stale.
        {
            let mut state = group.state.lock();
            state.active.running = false;
            group.advance_generation_locked(&mut state);
        }
        assert!(!group.accepts_reconcile_generation(generation));
    }

    #[cfg(feature = "pmu")]
    #[test]
    fn power_live_delta_uses_the_architectural_32_bit_wrap() {
        let power = PerfEvent::ReadOnly {
            pmu: axhal::perf_uncore::ReadOnlyPmu::Power,
            config: 0,
        };
        assert_eq!(
            PerfEventFile::external_counter_delta(power, u32::MAX as u64, 1),
            2
        );
    }
}
