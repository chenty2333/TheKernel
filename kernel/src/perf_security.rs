//! Open-time perf authority and global perf sysctl policy.
//!
//! Event authority is retained at open. Ring locked-memory admission instead
//! uses mmap-time credentials, limits, and online CPU count, as Linux does.

use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;
use linux_raw_sys::general::{CAP_PERFMON, CAP_SYS_ADMIN};
use thekernel_linux_perf::{
    ATTR_EXCLUDE_KERNEL, ATTR_FREQ, ATTR_PRECISE_IP, PERF_FLAG_PID_CGROUP, PERF_SAMPLE_AUX,
    PERF_SAMPLE_BRANCH_STACK, PERF_SAMPLE_REGS_INTR, PERF_TYPE_RAW, PERF_TYPE_TRACEPOINT,
    PerfEventAttr,
};

use crate::task::AsThread;

const PERF_LOCKED_MEMORY_OWNERS: usize = 16_384;
// Intel EventSel bit 21 is AnyThread.  The PMU programming path deliberately
// owns this bit and clears it; until the scheduler can reserve an SMT
// exclusive placement, admitting the request would silently change its
// delivery scope.
const RAW_CONFIG_ANYTHREAD: u64 = 1 << 21;

/// A mmap-time charge: the per-user allowance and the overflow charged to
/// this address space are refunded independently, even after credentials or
/// sysctls change. Stable mm IDs avoid retaining an address-space/file cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PerfMlockOwner {
    kuid: u32,
    mm: u64,
}

#[derive(Clone, Copy)]
struct PerfMlockEntry {
    owner: Option<PerfMlockOwner>,
    user_bytes: usize,
    pinned_bytes: usize,
}
impl PerfMlockEntry {
    const EMPTY: Self = Self { owner: None, user_bytes: 0, pinned_bytes: 0 };
}
static PERF_LOCKED_MEMORY: SpinNoIrq<[PerfMlockEntry; PERF_LOCKED_MEMORY_OWNERS]> =
    SpinNoIrq::new([PerfMlockEntry::EMPTY; PERF_LOCKED_MEMORY_OWNERS]);

pub(crate) struct PerfMlockReservation {
    owner: PerfMlockOwner,
    user_bytes: usize,
    pinned_bytes: usize,
}
#[cfg(test)]
impl PerfMlockReservation {
    pub(crate) fn for_test() -> Self {
        reserve_perf_locked_memory_for(PerfMlockOwner { kuid: u32::MAX - 1, mm: u64::MAX - 2 }, 4096, 4096, 0, false).unwrap()
    }
}

impl Drop for PerfMlockReservation {
    fn drop(&mut self) {
        let mut ledger = PERF_LOCKED_MEMORY.lock();
        let entry = ledger.iter_mut().find(|entry| entry.owner == Some(self.owner))
            .expect("perf locked-memory refund without an owner slot");
        entry.user_bytes = entry.user_bytes.checked_sub(self.user_bytes).expect("perf user charge underflow");
        entry.pinned_bytes = entry.pinned_bytes.checked_sub(self.pinned_bytes).expect("perf pinned charge underflow");
        if entry.user_bytes == 0 && entry.pinned_bytes == 0 { *entry = PerfMlockEntry::EMPTY; }
    }
}

fn split_perf_locked_memory(
    bytes: usize, user_used: usize, user_limit: usize, pinned_used: usize,
    memlock_limit: u64, bypass_limit: bool,
) -> AxResult<(usize, usize)> {
    let user_bytes = bytes.min(user_limit.saturating_sub(user_used));
    let pinned_bytes = bytes - user_bytes;
    let pinned_total = pinned_used.checked_add(pinned_bytes).ok_or(AxError::NoMemory)?;
    if !bypass_limit && pinned_total as u128 > u128::from(memlock_limit) {
        return Err(AxError::OperationNotPermitted);
    }
    Ok((user_bytes, pinned_bytes))
}

pub(crate) struct PerfMlockContext {
    owner: PerfMlockOwner,
    user_limit: usize,
    memlock_limit: u64,
    bypass_limit: bool,
}
impl PerfMlockContext {
    pub(crate) fn reserve(self, bytes: usize) -> AxResult<PerfMlockReservation> {
        reserve_perf_locked_memory_for(self.owner, bytes, self.user_limit, self.memlock_limit, self.bypass_limit)
    }
}

/// Capture sleepable task/mm policy before taking a perf backend spinlock.
pub(crate) fn perf_mlock_context() -> AxResult<PerfMlockContext> {
    let current = axtask::current();
    let thread = current.as_thread();
    let credential = thread.current_cred();
    let owner = PerfMlockOwner {
        kuid: credential.ids().ruid.into_raw(),
        mm: thread.proc_data.aspace().lock().address_space_id().get(),
    };
    // Linux rounds the per-CPU allowance and RLIMIT down to complete pages.
    let user_limit = (perf_event_mlock_kb() as usize / 4)
        .checked_mul(4096).and_then(|bytes| bytes.checked_mul(axhal::cpu_num().max(1)))
        .ok_or(AxError::InvalidInput)?;
    let memlock_limit = thread.proc_data.rlim.read()[linux_raw_sys::general::RLIMIT_MEMLOCK].current & !4095;
    let bypass_limit = perf_event_paranoid() < 0
        || credential.has_effective_capability(linux_raw_sys::general::CAP_IPC_LOCK);
    Ok(PerfMlockContext { owner, user_limit, memlock_limit, bypass_limit })
}

fn reserve_perf_locked_memory_for(
    owner: PerfMlockOwner, bytes: usize, user_limit: usize,
    memlock_limit: u64, bypass_limit: bool,
) -> AxResult<PerfMlockReservation> {
    let mut ledger = PERF_LOCKED_MEMORY.lock();
    let mut user_used = 0usize;
    let mut pinned_used = 0usize;
    for entry in ledger.iter() {
        if let Some(other) = entry.owner {
            if other.kuid == owner.kuid { user_used = user_used.checked_add(entry.user_bytes).ok_or(AxError::NoMemory)?; }
            if other.mm == owner.mm { pinned_used = pinned_used.checked_add(entry.pinned_bytes).ok_or(AxError::NoMemory)?; }
        }
    }
    let (user_bytes, pinned_bytes) = split_perf_locked_memory(
        bytes, user_used, user_limit, pinned_used, memlock_limit, bypass_limit)?;
    let index = ledger.iter().position(|entry| entry.owner == Some(owner))
        .or_else(|| ledger.iter().position(|entry| entry.owner.is_none())).ok_or(AxError::NoMemory)?;
    let entry = &mut ledger[index];
    let user_total = entry.user_bytes.checked_add(user_bytes).ok_or(AxError::NoMemory)?;
    let pinned_total = entry.pinned_bytes.checked_add(pinned_bytes).ok_or(AxError::NoMemory)?;
    *entry = PerfMlockEntry { owner: Some(owner), user_bytes: user_total, pinned_bytes: pinned_total };
    Ok(PerfMlockReservation { owner, user_bytes, pinned_bytes })
}

/// Linux's safe default: unprivileged callers may profile task-attached work
/// only in user mode, subject to the normal ptrace real-credential check.
/// The bounds deliberately match the documented -1..=3 range.
const PERF_EVENT_PARANOID_DEFAULT: i32 = 2;
const PERF_EVENT_PARANOID_MIN: i32 = -1;
const PERF_EVENT_PARANOID_MAX: i32 = 3;
const PERF_EVENT_MLOCK_KB_DEFAULT: u32 = 516;
const PERF_EVENT_MAX_SAMPLE_RATE_DEFAULT: u32 = 100_000;
const PERF_SYSCTL_MAX: u32 = i32::MAX as u32;

static PERF_EVENT_PARANOID: AtomicI32 = AtomicI32::new(PERF_EVENT_PARANOID_DEFAULT);
static PERF_EVENT_MLOCK_KB: AtomicU32 = AtomicU32::new(PERF_EVENT_MLOCK_KB_DEFAULT);
static PERF_EVENT_MAX_SAMPLE_RATE: AtomicU32 = AtomicU32::new(PERF_EVENT_MAX_SAMPLE_RATE_DEFAULT);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PerfAuthority {
    /// `CAP_PERFMON` is the preferred perf authority; `CAP_SYS_ADMIN` remains
    /// a Linux-compatible fallback for older userspace capability setups.
    Privileged,
    Restricted,
}

impl PerfAuthority {
    pub(crate) fn snapshot(has_perfmon: bool, has_sys_admin: bool) -> Self {
        if has_perfmon || has_sys_admin {
            Self::Privileged
        } else {
            Self::Restricted
        }
    }

    pub(crate) fn current() -> Self {
        let task = axtask::current();
        let thread = task.as_thread();
        Self::snapshot(
            thread.has_effective_capability(CAP_PERFMON),
            thread.has_effective_capability(CAP_SYS_ADMIN),
        )
    }
}

pub(crate) fn perf_event_paranoid() -> i32 {
    PERF_EVENT_PARANOID.load(Ordering::Acquire)
}

pub(crate) fn set_perf_event_paranoid(value: i32) -> AxResult<()> {
    if !(PERF_EVENT_PARANOID_MIN..=PERF_EVENT_PARANOID_MAX).contains(&value) {
        return Err(AxError::InvalidInput);
    }
    PERF_EVENT_PARANOID.store(value, Ordering::Release);
    Ok(())
}

pub(crate) fn perf_event_mlock_kb() -> u32 {
    PERF_EVENT_MLOCK_KB.load(Ordering::Acquire)
}

pub(crate) fn set_perf_event_mlock_kb(value: u32) -> AxResult<()> {
    if value > PERF_SYSCTL_MAX {
        return Err(AxError::InvalidInput);
    }
    PERF_EVENT_MLOCK_KB.store(value, Ordering::Release);
    Ok(())
}

pub(crate) fn perf_event_max_sample_rate() -> u32 {
    PERF_EVENT_MAX_SAMPLE_RATE.load(Ordering::Acquire)
}

pub(crate) fn set_perf_event_max_sample_rate(value: u32) -> AxResult<()> {
    if value == 0 || value > PERF_SYSCTL_MAX {
        return Err(AxError::InvalidInput);
    }
    PERF_EVENT_MAX_SAMPLE_RATE.store(value, Ordering::Release);
    Ok(())
}

/// Reject an open-time frequency request above the current global ceiling.
/// Period requests are bounded by the PMU planner; frequency has no hardware
/// period representation until the sampling backend translates it.
pub(crate) fn authorize_sampling_rate(attr: &PerfEventAttr) -> AxResult<()> {
    if attr.flags & ATTR_FREQ != 0 && attr.sample_period > u64::from(perf_event_max_sample_rate()) {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

/// Product sources which can expose machine-wide state or kernel control-flow
/// details.  These remain capability-gated even at `paranoid=-1`: that sysctl
/// level relaxes Linux's broad profiling restrictions, not the product's PMU
/// ownership and disclosure boundaries.
fn requires_perf_capability(attr: &PerfEventAttr) -> bool {
    use crate::pmu_registry::{DynamicPmu, dynamic_pmu};

    let dynamic = dynamic_pmu(attr.event_type);
    let raw_pmu = attr.event_type == PERF_TYPE_RAW
        || matches!(dynamic, Some(DynamicPmu::CpuCore | DynamicPmu::CpuAtom));
    let any_thread = raw_pmu && attr.config & (1 << 21) != 0;
    let precise_or_branch_or_aux = attr.flags & ATTR_PRECISE_IP != 0
        || attr.sample_type & (PERF_SAMPLE_BRANCH_STACK | PERF_SAMPLE_REGS_INTR | PERF_SAMPLE_AUX)
            != 0
        || attr.branch_sample_type != 0
        || matches!(dynamic, Some(DynamicPmu::IntelPt | DynamicPmu::IntelBts));
    let uncore_or_msr = matches!(
        dynamic,
        Some(DynamicPmu::Uncore { .. } | DynamicPmu::ReadOnly(_))
    );

    raw_pmu || any_thread || precise_or_branch_or_aux || uncore_or_msr
}

fn requests_unavailable_anythread_raw(attr: &PerfEventAttr) -> bool {
    use crate::pmu_registry::{DynamicPmu, dynamic_pmu};

    let raw_pmu = attr.event_type == PERF_TYPE_RAW
        || matches!(
            dynamic_pmu(attr.event_type),
            Some(DynamicPmu::CpuCore | DynamicPmu::CpuAtom)
        );
    raw_pmu && attr.config & RAW_CONFIG_ANYTHREAD != 0
}

/// Dynamic kernel tracing sources are intentionally separate from the PMU
/// sensitivity class above.  Linux's `paranoid >= 0` restriction applies to
/// raw tracepoint/ftrace access and kernel probes, while a uprobe remains a
/// task-address-space operation governed by the ptrace gate below.
fn is_kernel_dynamic_trace_source(attr: &PerfEventAttr) -> bool {
    attr.event_type == PERF_TYPE_TRACEPOINT
        || matches!(
            crate::pmu_registry::dynamic_pmu(attr.event_type),
            Some(crate::pmu_registry::DynamicPmu::Kprobe)
        )
}

/// Enforces perf_event_paranoid before event implementation selection.  This
/// ordering prevents unsupported raw/trace events from revealing availability
/// to callers who are not allowed to request them.
pub(crate) fn authorize_open(
    authority: PerfAuthority,
    attr: &PerfEventAttr,
    pid: i32,
    _cpu: i32,
    flags: u64,
) -> AxResult<()> {
    // AnyThread is currently stripped by PMU programming.  Keep this
    // unsupported request out of every authority path until an SMT-exclusive
    // placement can preserve the requested delivery scope.
    if requests_unavailable_anythread_raw(attr) {
        return Err(AxError::PermissionDenied);
    }

    // Capability is evaluated once at descriptor creation.  CAP_PERFMON is
    // preferred and CAP_SYS_ADMIN is its compatibility fallback; either
    // bypasses every paranoid/product source restriction below.
    if authority == PerfAuthority::Privileged {
        return Ok(());
    }

    if requires_perf_capability(attr) {
        return Err(AxError::PermissionDenied);
    }

    let paranoid = perf_event_paranoid();
    if paranoid < 0 {
        return Ok(());
    }

    if is_kernel_dynamic_trace_source(attr) {
        return Err(AxError::PermissionDenied);
    }

    // A task-on-CPU request remains task-attached: `pid != -1` selects the
    // task and the later ptrace real-credential gate authorizes that target.
    // Only a true pid=-1 CPU event is system-wide.  Cgroup targets are always
    // broad, regardless of their CPU selector.
    let system_wide = pid == -1;
    let cgroup = flags & PERF_FLAG_PID_CGROUP != 0;
    if paranoid >= 1 && (system_wide || cgroup) {
        return Err(AxError::PermissionDenied);
    }

    // At paranoid=2, Linux's default, user-mode restriction applies to every
    // task target rather than only the caller.  The syscall's later ptrace
    // ReadReal check is the independent cross-task authorization boundary.
    // RDPMC/CR4.PCE are never enabled by this kernel.
    if paranoid >= 2 && attr.flags & ATTR_EXCLUDE_KERNEL == 0 {
        return Err(AxError::PermissionDenied);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr() -> PerfEventAttr {
        PerfEventAttr {
            flags: ATTR_EXCLUDE_KERNEL,
            ..PerfEventAttr::default()
        }
    }

    #[test]
    fn perf_mmap_splits_user_allowance_and_mm_limit_without_waiving_accounting() {
        use super::*;
        let page = 4096;
        assert_eq!(split_perf_locked_memory(65 * page, 65 * page, 129 * page, 0, page as u64, false), Ok((64 * page, page)));
        assert_eq!(split_perf_locked_memory(65 * page, 65 * page, 129 * page, 0, 0, false), Err(AxError::OperationNotPermitted));
        assert_eq!(split_perf_locked_memory(65 * page, 65 * page, 129 * page, 0, 0, true), Ok((64 * page, page)));
        assert_eq!(split_perf_locked_memory(65 * page, 65 * page, 4 * 129 * page, 0, 0, false), Ok((65 * page, 0)));
        assert_eq!(split_perf_locked_memory(page, 2 * page, page, 0, page as u64, false), Ok((0, page)));
    }

    #[test]
    fn perf_mmap_shares_uid_allowance_but_charges_overflow_to_each_mm_and_refunds() {
        use super::*;
        let first = PerfMlockOwner { kuid: u32::MAX, mm: u64::MAX };
        let second = PerfMlockOwner { kuid: first.kuid, mm: first.mm - 1 };
        let a = reserve_perf_locked_memory_for(first, 4096, 4096, 0, false).unwrap();
        assert!(matches!(reserve_perf_locked_memory_for(second, 4096, 4096, 0, false), Err(AxError::OperationNotPermitted)));
        let b = reserve_perf_locked_memory_for(second, 4096, 4096, 4096, false).unwrap();
        assert_eq!((b.user_bytes, b.pinned_bytes), (0, 4096));
        assert!(matches!(reserve_perf_locked_memory_for(second, 4096, 4096, 4096, false), Err(AxError::OperationNotPermitted)));
        drop(a);
        let c = reserve_perf_locked_memory_for(first, 4096, 4096, 0, false).unwrap();
        assert_eq!((c.user_bytes, c.pinned_bytes), (4096, 0));
        drop((b, c));
        assert!(PERF_LOCKED_MEMORY.lock().iter().all(|entry| entry.owner != Some(first) && entry.owner != Some(second)));
    }

    #[test]
    fn default_policy_is_linux_safe_and_privileged_snapshot_wins() {
        assert_eq!(perf_event_paranoid(), 2);
        let request = attr();
        assert_eq!(
            authorize_open(PerfAuthority::Restricted, &request, 0, -1, 0),
            Ok(())
        );
        assert_eq!(
            authorize_open(PerfAuthority::snapshot(true, false), &request, 7, 0, 0),
            Ok(())
        );
        assert_eq!(
            authorize_open(PerfAuthority::snapshot(false, true), &request, 7, 0, 0),
            Ok(())
        );
    }

    #[test]
    fn restricted_open_rejects_kernel_and_sensitive_sources() {
        let mut request = attr();
        request.flags = 0;
        assert_eq!(
            authorize_open(PerfAuthority::Restricted, &request, 0, -1, 0),
            Err(AxError::PermissionDenied)
        );
        request.flags = ATTR_EXCLUDE_KERNEL;
        request.event_type = PERF_TYPE_RAW;
        assert_eq!(
            authorize_open(PerfAuthority::Restricted, &request, 0, -1, 0),
            Err(AxError::PermissionDenied)
        );
        request.event_type = 0;
        request.sample_type = PERF_SAMPLE_BRANCH_STACK;
        assert_eq!(
            authorize_open(PerfAuthority::Restricted, &request, 0, -1, 0),
            Err(AxError::PermissionDenied)
        );
    }

    #[test]
    fn anythread_raw_is_never_admitted_before_smt_exclusive_placement_exists() {
        let request = PerfEventAttr {
            event_type: PERF_TYPE_RAW,
            config: 1 << 21,
            ..attr()
        };
        assert_eq!(
            authorize_open(PerfAuthority::Privileged, &request, 0, -1, 0),
            Err(AxError::PermissionDenied)
        );
    }

    #[test]
    fn sysctl_values_are_bounded() {
        assert_eq!(set_perf_event_paranoid(-2), Err(AxError::InvalidInput));
        assert_eq!(set_perf_event_paranoid(4), Err(AxError::InvalidInput));
        assert_eq!(
            set_perf_event_mlock_kb(u32::MAX),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            set_perf_event_max_sample_rate(0),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            set_perf_event_max_sample_rate(u32::MAX),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn frequency_is_bounded_at_open_time() {
        let old = perf_event_max_sample_rate();
        set_perf_event_max_sample_rate(100).unwrap();
        assert_eq!(
            authorize_sampling_rate(&PerfEventAttr {
                flags: ATTR_FREQ,
                sample_period: 101,
                ..attr()
            }),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            authorize_sampling_rate(&PerfEventAttr {
                flags: ATTR_FREQ,
                sample_period: 100,
                ..attr()
            }),
            Ok(())
        );
        set_perf_event_max_sample_rate(old).unwrap();
    }
}
