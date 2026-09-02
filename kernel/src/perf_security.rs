//! Open-time perf authority and global perf sysctl policy.
//!
//! Perf descriptors retain the result of this policy at creation: capability
//! changes and later sysctl writes never revoke an already-open descriptor.

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

/// Frozen owner of perf mmap/AUX locked-memory accounting.  Keeping scalar
/// namespace/uid identities avoids retaining credentials after open while
/// ensuring later credential changes cannot move an existing charge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PerfMlockOwner {
    user_namespace: u64,
    kuid: u32,
}

impl PerfMlockOwner {
    pub(crate) fn current() -> Self {
        let credential = axtask::current().as_thread().current_cred();
        Self {
            user_namespace: credential.user_ns().identity().into_raw(),
            kuid: credential.ids().euid.into_raw(),
        }
    }

    #[cfg(test)]
    pub(crate) const fn kernel_test_owner() -> Self {
        Self {
            user_namespace: 0,
            kuid: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct PerfMlockEntry {
    owner: Option<PerfMlockOwner>,
    bytes: usize,
}

impl PerfMlockEntry {
    const EMPTY: Self = Self {
        owner: None,
        bytes: 0,
    };
}

static PERF_LOCKED_MEMORY: SpinNoIrq<[PerfMlockEntry; PERF_LOCKED_MEMORY_OWNERS]> =
    SpinNoIrq::new([PerfMlockEntry::EMPTY; PERF_LOCKED_MEMORY_OWNERS]);

/// RAII reservation used across fallible page/ring allocation.  Until
/// committed, every early return refunds the exact open-time owner.
pub(crate) struct PerfMlockReservation {
    owner: PerfMlockOwner,
    bytes: usize,
    committed: bool,
}

impl PerfMlockReservation {
    pub(crate) fn commit(mut self) -> usize {
        self.committed = true;
        self.bytes
    }
}

impl Drop for PerfMlockReservation {
    fn drop(&mut self) {
        if !self.committed {
            release_perf_locked_memory(self.owner, self.bytes);
        }
    }
}

pub(crate) fn reserve_perf_locked_memory(
    owner: PerfMlockOwner,
    bytes: usize,
    bypass_limit: bool,
) -> AxResult<PerfMlockReservation> {
    // The descriptor records this decision at open.  A later sysctl change
    // must not retroactively charge or uncharge an existing mmap/AUX ring.
    // A zero-byte reservation deliberately uses the normal RAII path while
    // leaving the per-owner ledger untouched.
    if bypass_limit {
        return Ok(PerfMlockReservation {
            owner,
            bytes: 0,
            committed: false,
        });
    }
    let limit = usize::try_from(perf_event_mlock_kb())
        .ok()
        .and_then(|kb| kb.checked_mul(1024))
        .ok_or(AxError::InvalidInput)?;
    let mut ledger = PERF_LOCKED_MEMORY.lock();
    let mut empty = None;
    for (index, entry) in ledger.iter_mut().enumerate() {
        if entry.owner == Some(owner) {
            let total = entry.bytes.checked_add(bytes).ok_or(AxError::NoMemory)?;
            if total > limit {
                return Err(AxError::OperationNotPermitted);
            }
            entry.bytes = total;
            return Ok(PerfMlockReservation {
                owner,
                bytes,
                committed: false,
            });
        }
        if entry.owner.is_none() && empty.is_none() {
            empty = Some(index);
        }
    }
    if bytes > limit {
        return Err(AxError::OperationNotPermitted);
    }
    let slot = empty.ok_or(AxError::NoMemory)?;
    ledger[slot] = PerfMlockEntry {
        owner: Some(owner),
        bytes,
    };
    Ok(PerfMlockReservation {
        owner,
        bytes,
        committed: false,
    })
}

pub(crate) fn release_perf_locked_memory(owner: PerfMlockOwner, bytes: usize) {
    if bytes == 0 {
        return;
    }
    let mut ledger = PERF_LOCKED_MEMORY.lock();
    let entry = ledger
        .iter_mut()
        .find(|entry| entry.owner == Some(owner))
        .expect("perf locked-memory refund without an owner slot");
    entry.bytes = entry
        .bytes
        .checked_sub(bytes)
        .expect("perf locked-memory refund exceeded its charge");
    if entry.bytes == 0 {
        *entry = PerfMlockEntry::EMPTY;
    }
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

/// Linux's `perf_event_paranoid=-1` also lifts perf mmap locked-memory
/// accounting.  Sampling configurations store this boolean at open so mmap
/// and AUX setup cannot consult changed credentials or a changed sysctl.
pub(crate) fn perf_mlock_limit_bypassed_at_open() -> bool {
    perf_event_paranoid() < 0
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
