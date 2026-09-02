//! BPF admission and object-memory accounting.
//!
//! BPF objects are kernel-resident allocations whose lifetime is the lifetime
//! of an FD, a pinned object, or a link.  Admission therefore has to snapshot
//! the caller's authority and retain a real `RLIMIT_MEMLOCK` charge until the
//! last owning object disappears; checking a limit only at `BPF_MAP_CREATE`
//! would let duplicated and retained objects escape resource governance.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};

use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;
use axtask::current;
use linux_raw_sys::general::{
    CAP_BPF, CAP_IPC_LOCK, CAP_PERFMON, CAP_SYS_ADMIN, RLIM_INFINITY, RLIMIT_MEMLOCK,
};

use crate::task::{AsThread, ProcessData, UserNamespace};

/// Linux's sysctl has three states: 0 permits the carefully constrained
/// unprivileged subset, 1 permanently disables it, and 2 disables it until a
/// privileged administrator changes it.  The kernel does not offer a write
/// path that can weaken state 1.
static UNPRIVILEGED_BPF_DISABLED: AtomicU32 = AtomicU32::new(0);

struct ChargeEntry {
    owner: usize,
    bytes: usize,
}

static BPF_MEMLOCK: SpinNoIrq<Vec<ChargeEntry>> = SpinNoIrq::new(Vec::new());

/// A retained, process-owned BPF memory charge.  The `Arc` prevents the
/// process identity from being reused while a detached object still exists.
pub struct BpfMemoryCharge {
    owner: Arc<ProcessData>,
    bytes: usize,
}

impl Drop for BpfMemoryCharge {
    fn drop(&mut self) {
        let owner = Arc::as_ptr(&self.owner) as usize;
        let mut charges = BPF_MEMLOCK.lock();
        let index = charges
            .iter()
            .position(|entry| entry.owner == owner)
            .expect("BPF memory charge owner disappeared");
        let entry = &mut charges[index];
        entry.bytes = entry
            .bytes
            .checked_sub(self.bytes)
            .expect("BPF memory charge underflow");
        if entry.bytes == 0 {
            charges.swap_remove(index);
        }
    }
}

/// Reserve the bytes calculated from the Linux UAPI request.  The accounting
/// is per process (matching `RLIMIT_MEMLOCK`), not per current thread or UID.
pub(crate) fn reserve_memory(bytes: usize) -> AxResult<Arc<BpfMemoryCharge>> {
    let current = current();
    let thread = current.as_thread();
    let owner = thread.proc_data.clone();
    let owner_key = Arc::as_ptr(&owner) as usize;
    let privileged = thread.has_effective_capability(CAP_IPC_LOCK);
    let limit = owner.rlim.read()[RLIMIT_MEMLOCK].current;

    let mut charges = BPF_MEMLOCK.lock();
    if let Some(entry) = charges.iter_mut().find(|entry| entry.owner == owner_key) {
        let total = entry.bytes.checked_add(bytes).ok_or(AxError::NoMemory)?;
        if !privileged && limit != RLIM_INFINITY as u64 && total > limit as usize {
            return Err(AxError::OperationNotPermitted);
        }
        entry.bytes = total;
    } else {
        if !privileged && limit != RLIM_INFINITY as u64 && bytes > limit as usize {
            return Err(AxError::OperationNotPermitted);
        }
        charges.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        charges.push(ChargeEntry {
            owner: owner_key,
            bytes,
        });
    }
    drop(charges);
    Arc::try_new(BpfMemoryCharge { owner, bytes }).map_err(|_| {
        // The ledger entry is committed before the fallible ownership object
        // allocation, so undo it exactly here rather than leaking a charge.
        let mut charges = BPF_MEMLOCK.lock();
        let index = charges
            .iter()
            .position(|entry| entry.owner == owner_key)
            .expect("BPF charge vanished before ownership allocation");
        let entry = &mut charges[index];
        entry.bytes = entry
            .bytes
            .checked_sub(bytes)
            .expect("BPF charge underflow");
        if entry.bytes == 0 {
            charges.swap_remove(index);
        }
        AxError::NoMemory
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BpfAuthority {
    Unprivileged,
    Bpf,
    BpfAndPerfmon,
}

/// Authority captured by `BPF_TOKEN_CREATE`.
///
/// A token is deliberately narrower than an ambient capability check: it is
/// anchored in the issuer's user namespace, and every admission using it is
/// checked against the command class and concrete object type.  In
/// particular, passing a token across a user-namespace boundary cannot turn
/// an otherwise unprivileged nested namespace into the issuer's namespace.
#[derive(Clone)]
pub(crate) struct BpfTokenGrant {
    authority: BpfAuthority,
    issuer_user_ns: Arc<UserNamespace>,
}

impl BpfTokenGrant {
    pub(crate) fn from_current(authority: BpfAuthority) -> Self {
        Self {
            authority,
            issuer_user_ns: current().as_thread().current_cred().user_ns().clone(),
        }
    }

    fn authorize_namespace(&self) -> AxResult<BpfAuthority> {
        let caller_ns = current().as_thread().current_cred().user_ns().clone();
        if !Arc::ptr_eq(&caller_ns, &self.issuer_user_ns) {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(self.authority)
    }

    pub(crate) fn authorize_by_id_lookup(&self) -> AxResult<()> {
        if self.authorize_namespace()?.bpf_capable() {
            Ok(())
        } else {
            Err(AxError::OperationNotPermitted)
        }
    }
}

impl BpfAuthority {
    pub(crate) fn current() -> Self {
        let current = current();
        let thread = current.as_thread();
        if thread.has_effective_capability(CAP_SYS_ADMIN) {
            return Self::BpfAndPerfmon;
        }
        if thread.has_effective_capability(CAP_BPF) {
            if thread.has_effective_capability(CAP_PERFMON) {
                Self::BpfAndPerfmon
            } else {
                Self::Bpf
            }
        } else {
            Self::Unprivileged
        }
    }

    pub(crate) fn bpf_capable(self) -> bool {
        !matches!(self, Self::Unprivileged)
    }

    pub(crate) fn perfmon_capable(self) -> bool {
        matches!(self, Self::BpfAndPerfmon)
    }
}

pub(crate) fn unprivileged_bpf_disabled() -> u32 {
    UNPRIVILEGED_BPF_DISABLED.load(Ordering::Acquire)
}

/// Privileged sysctl writer.  State 1 is intentionally irreversible, as on
/// Linux; state 2 can be changed by a capable administrator.
pub(crate) fn set_unprivileged_bpf_disabled(value: u32) -> AxResult<()> {
    if value > 2 {
        return Err(AxError::InvalidInput);
    }
    // This is a global sysctl rather than a BPF-object operation.  Linux
    // keeps its write authority at CAP_SYS_ADMIN; CAP_BPF must not let a
    // container program loader change the policy for unrelated processes.
    if !current()
        .as_thread()
        .has_effective_capability(CAP_SYS_ADMIN)
    {
        return Err(AxError::OperationNotPermitted);
    }
    let old = unprivileged_bpf_disabled();
    if old == 1 && value != 1 {
        return Err(AxError::OperationNotPermitted);
    }
    UNPRIVILEGED_BPF_DISABLED.store(value, Ordering::Release);
    Ok(())
}

fn authorize_map_create_as(authority: BpfAuthority, map_type: u32) -> AxResult<()> {
    if authority.bpf_capable() {
        return Ok(());
    }
    if unprivileged_bpf_disabled() != 0 {
        return Err(AxError::OperationNotPermitted);
    }
    // PERF_EVENT_ARRAY transports references to a privileged observation
    // source; all other currently implemented map kinds have verifier-bound
    // userspace-only semantics and remain available to the unprivileged
    // socket-filter subset.
    if map_type == crate::bpf::defs::BPF_MAP_TYPE_PERF_EVENT_ARRAY {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(())
}

pub(crate) fn authorize_map_create(map_type: u32) -> AxResult<()> {
    authorize_map_create_as(BpfAuthority::current(), map_type)
}

/// Token admission is intentionally separate from the ambient path.  The
/// only authority usable here is the immutable capability snapshot in the
/// token, after validating that the caller remains in its anchored userns.
pub(crate) fn authorize_token_map_create(token: &BpfTokenGrant, map_type: u32) -> AxResult<()> {
    authorize_map_create_as(token.authorize_namespace()?, map_type)
}

fn authorize_program_load_as(authority: BpfAuthority, prog_type: u32) -> AxResult<()> {
    if authority.bpf_capable() {
        // Programs able to observe kernel execution require both independent
        // capabilities. CAP_BPF alone is deliberately insufficient.
        if matches!(
            prog_type,
            crate::bpf::defs::BPF_PROG_TYPE_KPROBE
                | crate::bpf::defs::BPF_PROG_TYPE_TRACEPOINT
                | crate::bpf::defs::BPF_PROG_TYPE_PERF_EVENT
                | crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT
                | crate::bpf::prog::BPF_PROG_TYPE_TRACING
                | crate::bpf::prog::BPF_PROG_TYPE_LSM
        ) && !authority.perfmon_capable()
        {
            return Err(AxError::OperationNotPermitted);
        }
        return Ok(());
    }
    if unprivileged_bpf_disabled() != 0
        || prog_type != crate::bpf::defs::BPF_PROG_TYPE_SOCKET_FILTER
    {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(())
}

pub(crate) fn authorize_program_load(prog_type: u32) -> AxResult<()> {
    authorize_program_load_as(BpfAuthority::current(), prog_type)
}

pub(crate) fn authorize_token_program_load(token: &BpfTokenGrant, prog_type: u32) -> AxResult<()> {
    authorize_program_load_as(token.authorize_namespace()?, prog_type)
}

pub(crate) fn authorize_link_create() -> AxResult<()> {
    let authority = BpfAuthority::current();
    if authority.bpf_capable() {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

/// Perf links expose sampling/PMU state and therefore retain the independent
/// CAP_PERFMON requirement.  Network/cgroup links use their namespace-local
/// CAP_NET_ADMIN gate at the concrete target instead.
pub(crate) fn authorize_perf_link_create() -> AxResult<()> {
    let authority = BpfAuthority::current();
    if authority.bpf_capable() && authority.perfmon_capable() {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

pub(crate) fn authorize_token_btf_load(token: &BpfTokenGrant) -> AxResult<()> {
    if token.authorize_namespace()?.bpf_capable() {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}
