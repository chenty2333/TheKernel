//! All-online-CPU user shadow-stack capability commit.
//!
//! Probe is deliberately read-only.  APs are brought up after the BSP's
//! `init_later` and before the IPI broker is installed, so publishing CET only
//! after every logical CPU prepared avoids a rollback path that would require
//! a not-yet-existing remote execution channel.

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use kspin::SpinNoIrq;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unsupported,
}

/// Immutable outcome of one CPU's CET capability probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilitySnapshot {
    pub prepared: bool,
    pub user_shadow_stack: bool,
    pub fleet_active: bool,
}

const EMPTY: CapabilitySnapshot = CapabilitySnapshot {
    prepared: false,
    user_shadow_stack: false,
    fleet_active: false,
};

static SNAPSHOTS: [SpinNoIrq<CapabilitySnapshot>; crate::config::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(EMPTY) }; crate::config::plat::MAX_CPU_NUM];

// This is captured before the fleet activates CET lazily on any CPU.  It is
// not task state and must never participate in normal context switches.
static BOOT_BASELINES: [SpinNoIrq<axcpu::asm::UserCetBootBaseline>;
    crate::config::plat::MAX_CPU_NUM] = [const {
    SpinNoIrq::new(axcpu::asm::UserCetBootBaseline {
        captured: false,
        cr4_cet: false,
        u_cet: 0,
        pl3_ssp: 0,
    })
}; crate::config::plat::MAX_CPU_NUM];

const COLLECTING: u8 = 0;
const ACTIVE: u8 = 1;
const ABORTED: u8 = 2;
static PHASE: AtomicU8 = AtomicU8::new(COLLECTING);
static PREPARED: AtomicUsize = AtomicUsize::new(0);

/// The all-online-Cpu admission predicate used by the CET fleet commit.
///
/// A locally positive probe alone is deliberately insufficient: publication
/// is legal only after every discovered online CPU has prepared successfully.
const fn fleet_commit_ready(local_positive: bool, prepared: usize, online: usize) -> bool {
    local_positive && prepared == online
}

fn local<T>(f: impl FnOnce(&mut CapabilitySnapshot) -> T) -> T {
    let _guard = kernel_guard::NoPreemptIrqSave::new();
    let cpu = crate::cpu::current_logical_cpu_id();
    f(&mut SNAPSHOTS[cpu].lock())
}

/// Read-only local capability probe.  A negative result aborts the entire
/// fleet without making boot fail.
pub fn prepare_current() -> Result<(), Error> {
    if PHASE.load(Ordering::Acquire) == ABORTED {
        return Err(Error::Unsupported);
    }
    let prepared = local(|snapshot| {
        if snapshot.prepared {
            return snapshot
                .user_shadow_stack
                .then_some(false)
                .ok_or(Error::Unsupported);
        }
        let supported = axcpu::asm::user_shadow_stack_supported();
        let cpu = crate::cpu::current_logical_cpu_id();
        *BOOT_BASELINES[cpu].lock() = axcpu::asm::user_cet_boot_baseline();
        snapshot.prepared = true;
        snapshot.user_shadow_stack = supported;
        if supported {
            Ok(true)
        } else {
            Err(Error::Unsupported)
        }
    });
    match prepared {
        Ok(newly_prepared) => {
            if newly_prepared {
                PREPARED.fetch_add(1, Ordering::AcqRel);
            }
            Ok(())
        }
        Err(error) => {
            abort_fleet();
            Err(error)
        }
    }
}

/// Restore this CPU's firmware CET baseline for a terminal kexec handoff.
///
/// The stop IPI calls this before publishing its acknowledgement, and the
/// initiator calls it before transferring control.  No scheduler path calls
/// this routine, so it cannot affect an ordinary task switch.
pub fn restore_current_boot_baseline_for_kexec() {
    let cpu = crate::cpu::current_logical_cpu_id();
    let baseline = *BOOT_BASELINES[cpu].lock();
    axcpu::asm::restore_user_cet_boot_baseline(baseline);
}

/// Commit only after every discovered online CPU completed a positive probe.
/// Returns false while AP bring-up is still in progress or after an abort.
pub fn commit_current() -> bool {
    let local_positive = local(|snapshot| snapshot.prepared && snapshot.user_shadow_stack);
    if !fleet_commit_ready(
        local_positive,
        PREPARED.load(Ordering::Acquire),
        crate::cpu::cpu_num(),
    ) {
        return false;
    }
    let active =
        match PHASE.compare_exchange(COLLECTING, ACTIVE, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) | Err(ACTIVE) => true,
            Err(_) => false,
        };
    if active {
        axcpu::asm::set_user_shadow_stack_fleet_active(true);
        local(|snapshot| snapshot.fleet_active = true);
    }
    active
}

/// Abort the uncommitted fleet.  Prepare never writes CR4 or CET MSRs, hence
/// no remote state needs restoring at this boot stage.
pub fn abort_current() {
    abort_fleet();
    local(|snapshot| snapshot.fleet_active = false);
}

fn abort_fleet() {
    PHASE.store(ABORTED, Ordering::Release);
    axcpu::asm::set_user_shadow_stack_fleet_active(false);
}

/// Whether the hardware-independent global CET gate was committed.
pub fn is_active() -> bool {
    PHASE.load(Ordering::Acquire) == ACTIVE
}

/// Return a per-CPU probe snapshot, if `cpu` is in the platform topology.
pub fn capability_snapshot(cpu: usize) -> Option<CapabilitySnapshot> {
    (cpu < crate::cpu::cpu_num()).then(|| {
        let mut snapshot = *SNAPSHOTS[cpu].lock();
        // Fleet activation is a global two-phase commit outcome, not a
        // property of whichever CPU won the commit CAS.  Synthesize the
        // published bit for every positively prepared online CPU so
        // capability reporting cannot disagree with the global gate.
        snapshot.fleet_active = is_active() && snapshot.prepared && snapshot.user_shadow_stack;
        snapshot
    })
}

#[cfg(test)]
mod tests {
    use super::fleet_commit_ready;

    #[test]
    fn fleet_model_requires_every_online_cpu() {
        let mut prepared = 0usize;
        let online = 4usize;
        for _ in 0..online - 1 {
            prepared += 1;
            assert_ne!(prepared, online);
        }
        prepared += 1;
        assert_eq!(prepared, online);
    }

    #[test]
    fn negative_probe_is_an_abort_not_a_boot_failure() {
        let supported = [true, false, true];
        assert!(supported.iter().any(|supported| !supported));
        assert!(!supported.iter().all(|supported| *supported));
    }

    #[test]
    fn fleet_commit_never_publishes_after_a_negative_or_missing_probe() {
        assert!(!fleet_commit_ready(false, 4, 4));
        assert!(!fleet_commit_ready(true, 3, 4));
        assert!(!fleet_commit_ready(true, 5, 4));
        assert!(fleet_commit_ready(true, 4, 4));
    }
}
