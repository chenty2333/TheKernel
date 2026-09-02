//! eBPF subsystem: virtual machine, maps, verifier, and program management.

pub mod btf;
pub mod defs;
pub mod helpers;
pub mod map;
pub mod prog;
pub mod verifier;

use alloc::{
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

use axerrno::AxError;
use axfs_ng_vfs::{Location, ObjectKey};
use axsync::spin::SpinNoIrq;
use bytemuck::AnyBitPattern;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::mm::map_usercopy_error;

static NEXT_MAP_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_PROG_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_LINK_ID: AtomicU32 = AtomicU32::new(1);
const BPF_ATTR_MAX_SIZE: usize = 4096;

struct MapIdEntry {
    id: u32,
    map: Weak<dyn map::BpfMap>,
    charge: Weak<crate::bpf_security::BpfMemoryCharge>,
    name: [u8; defs::BPF_OBJ_NAME_LEN],
    btf: Option<Arc<btf::BpfBtf>>,
}
struct ProgIdEntry {
    id: u32,
    program: Weak<prog::BpfProgram>,
}
static MAP_IDS: SpinNoIrq<Vec<MapIdEntry>> = SpinNoIrq::new(Vec::new());
static PROG_IDS: SpinNoIrq<Vec<ProgIdEntry>> = SpinNoIrq::new(Vec::new());

/// Link IDs name the link object itself, never its transient descriptor.  A
/// producer/pin/duplicated FD therefore observes one lifecycle and one ID.
#[derive(Clone)]
pub(crate) enum BpfLinkObject {
    Network(Arc<crate::file::bpf::BpfNetworkLink>),
    Iter(Arc<crate::file::bpf::BpfIterLink>),
    Lsm(Arc<crate::file::bpf::BpfLsmLink>),
    PerfEvent(Arc<crate::file::bpf::BpfPerfEventLink>),
    RawTracepoint(Arc<crate::file::bpf::BpfRawTracepointLink>),
}
enum BpfLinkWeak {
    Network(Weak<crate::file::bpf::BpfNetworkLink>),
    Iter(Weak<crate::file::bpf::BpfIterLink>),
    Lsm(Weak<crate::file::bpf::BpfLsmLink>),
    PerfEvent(Weak<crate::file::bpf::BpfPerfEventLink>),
    RawTracepoint(Weak<crate::file::bpf::BpfRawTracepointLink>),
}
struct LinkIdEntry {
    id: u32,
    object: BpfLinkWeak,
}
static LINK_IDS: SpinNoIrq<Vec<LinkIdEntry>> = SpinNoIrq::new(Vec::new());

impl BpfLinkObject {
    fn downgrade(&self) -> BpfLinkWeak {
        match self {
            Self::Network(link) => BpfLinkWeak::Network(Arc::downgrade(link)),
            Self::Iter(link) => BpfLinkWeak::Iter(Arc::downgrade(link)),
            Self::Lsm(link) => BpfLinkWeak::Lsm(Arc::downgrade(link)),
            Self::PerfEvent(link) => BpfLinkWeak::PerfEvent(Arc::downgrade(link)),
            Self::RawTracepoint(link) => BpfLinkWeak::RawTracepoint(Arc::downgrade(link)),
        }
    }
    pub(crate) fn add_to_fd_table(&self, cloexec: bool) -> axerrno::AxResult<i32> {
        match self {
            Self::Network(link) => crate::file::add_file_like(link.clone(), cloexec),
            Self::Iter(link) => crate::file::add_file_like(link.clone(), cloexec),
            Self::Lsm(link) => crate::file::add_file_like(link.clone(), cloexec),
            Self::PerfEvent(link) => crate::file::add_file_like(link.clone(), cloexec),
            Self::RawTracepoint(link) => crate::file::add_file_like(link.clone(), cloexec),
        }
    }
    fn abort_publication(&self) {
        match self {
            Self::Network(link) => {
                let _ = link.detach();
            }
            Self::Iter(link) => {
                let _ = link.detach();
            }
            Self::Lsm(link) => {
                let _ = link.detach();
            }
            Self::PerfEvent(link) => {
                let _ = link.detach();
            }
            Self::RawTracepoint(link) => {
                let _ = link.detach();
            }
        }
    }
    pub(crate) fn link_type_and_program(&self) -> axerrno::AxResult<(u32, u32)> {
        match self {
            Self::Network(link) => Ok((link.link_type(), link.program_id()?)),
            Self::Iter(link) => Ok((4, link.program_id()?)),
            Self::Lsm(link) => Ok((2, link.program_id()?)),
            Self::PerfEvent(link) => Ok((7, link.program_id()?)),
            Self::RawTracepoint(link) => Ok((1, link.program_id()?)),
        }
    }
}
impl BpfLinkWeak {
    fn upgrade(&self) -> Option<BpfLinkObject> {
        match self {
            Self::Network(link) => link.upgrade().map(BpfLinkObject::Network),
            Self::Iter(link) => link.upgrade().map(BpfLinkObject::Iter),
            Self::Lsm(link) => link.upgrade().map(BpfLinkObject::Lsm),
            Self::PerfEvent(link) => link.upgrade().map(BpfLinkObject::PerfEvent),
            Self::RawTracepoint(link) => link.upgrade().map(BpfLinkObject::RawTracepoint),
        }
    }
}

/// Publish an already-attached link and its first FD as one transaction.  An
/// FD allocation failure removes the just-created registry entry, so neither
/// ID enumeration nor by-ID lookup can observe an orphan.
pub(crate) fn publish_link(object: BpfLinkObject, cloexec: bool) -> axerrno::AxResult<i32> {
    // Keep the registry closed until the creator's first FD exists.  This
    // prevents a by-ID opener from racing a failing initial publication.
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| entry.object.upgrade().is_some());
    if entries.try_reserve(1).is_err() {
        drop(entries);
        object.abort_publication();
        return Err(AxError::NoMemory);
    }
    let id = loop {
        let candidate = NEXT_LINK_ID.fetch_add(1, Ordering::Relaxed);
        if candidate != 0 && !entries.iter().any(|entry| entry.id == candidate) {
            break candidate;
        }
    };
    match object.add_to_fd_table(cloexec) {
        Ok(fd) => {
            entries.push(LinkIdEntry {
                id,
                object: object.downgrade(),
            });
            Ok(fd)
        }
        Err(error) => {
            drop(entries);
            object.abort_publication();
            Err(error)
        }
    }
}
pub(crate) fn next_link_id(start: u32) -> Option<u32> {
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| entry.object.upgrade().is_some());
    entries
        .iter()
        .filter(|entry| entry.id > start)
        .map(|entry| entry.id)
        .min()
}
pub(crate) fn link_by_id(id: u32) -> Option<BpfLinkObject> {
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| entry.object.upgrade().is_some());
    entries
        .iter()
        .find(|entry| entry.id == id)
        .and_then(|entry| entry.object.upgrade())
}
pub(crate) fn link_id_for_network(link: &crate::file::bpf::BpfNetworkLink) -> Option<u32> {
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| entry.object.upgrade().is_some());
    entries
        .iter()
        .find_map(|entry| match entry.object.upgrade() {
            Some(BpfLinkObject::Network(object)) if core::ptr::eq(Arc::as_ptr(&object), link) => {
                Some(entry.id)
            }
            _ => None,
        })
}
pub(crate) fn unregister_network_link(link: &crate::file::bpf::BpfNetworkLink) {
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| match entry.object.upgrade() {
        Some(BpfLinkObject::Network(object)) => !core::ptr::eq(Arc::as_ptr(&object), link),
        Some(_) => true,
        None => false,
    });
}
pub(crate) fn link_id_for_iter(link: &crate::file::bpf::BpfIterLink) -> Option<u32> {
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| entry.object.upgrade().is_some());
    entries
        .iter()
        .find_map(|entry| match entry.object.upgrade() {
            Some(BpfLinkObject::Iter(object)) if core::ptr::eq(Arc::as_ptr(&object), link) => {
                Some(entry.id)
            }
            _ => None,
        })
}
pub(crate) fn link_id_for_lsm(link: &crate::file::bpf::BpfLsmLink) -> Option<u32> {
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| entry.object.upgrade().is_some());
    entries
        .iter()
        .find_map(|entry| match entry.object.upgrade() {
            Some(BpfLinkObject::Lsm(object)) if core::ptr::eq(Arc::as_ptr(&object), link) => {
                Some(entry.id)
            }
            _ => None,
        })
}
pub(crate) fn link_id_for_perf(link: &crate::file::bpf::BpfPerfEventLink) -> Option<u32> {
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| entry.object.upgrade().is_some());
    entries
        .iter()
        .find_map(|entry| match entry.object.upgrade() {
            Some(BpfLinkObject::PerfEvent(object)) if core::ptr::eq(Arc::as_ptr(&object), link) => {
                Some(entry.id)
            }
            _ => None,
        })
}
pub(crate) fn link_id_for_raw_tracepoint(
    link: &crate::file::bpf::BpfRawTracepointLink,
) -> Option<u32> {
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| entry.object.upgrade().is_some());
    entries
        .iter()
        .find_map(|entry| match entry.object.upgrade() {
            Some(BpfLinkObject::RawTracepoint(object))
                if core::ptr::eq(Arc::as_ptr(&object), link) =>
            {
                Some(entry.id)
            }
            _ => None,
        })
}

/// Explicit detach ends the link object's public identity even when a caller
/// still holds its descriptor or a pin.  Unlike final-close pruning, this
/// removes the live weak entry immediately so by-ID operations cannot revive a
/// detached attachment.
pub(crate) fn unregister_raw_tracepoint_link(link: &crate::file::bpf::BpfRawTracepointLink) {
    let mut entries = LINK_IDS.lock();
    entries.retain(|entry| match entry.object.upgrade() {
        Some(BpfLinkObject::RawTracepoint(object)) => !core::ptr::eq(Arc::as_ptr(&object), link),
        Some(_) => true,
        None => false,
    });
}

const MAX_ACTIVE_RAW_TRACEPOINT_LINKS: usize = 64;
static ACTIVE_RAW_TRACEPOINT_LINKS: SpinNoIrq<Vec<Weak<crate::file::bpf::BpfRawTracepointLink>>> =
    SpinNoIrq::new(Vec::new());

pub(crate) fn activate_raw_tracepoint_link(
    link: &Arc<crate::file::bpf::BpfRawTracepointLink>,
) -> axerrno::AxResult<()> {
    let mut active = ACTIVE_RAW_TRACEPOINT_LINKS.lock();
    active.retain(|candidate| candidate.upgrade().is_some_and(|link| !link.detached()));
    if active
        .iter()
        .any(|candidate| candidate.ptr_eq(&Arc::downgrade(link)))
    {
        return Ok(());
    }
    if active.len() == MAX_ACTIVE_RAW_TRACEPOINT_LINKS {
        return Err(AxError::StorageFull);
    }
    active.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    active.push(Arc::downgrade(link));
    Ok(())
}
pub(crate) fn deactivate_raw_tracepoint_link(link: &crate::file::bpf::BpfRawTracepointLink) {
    let mut active = ACTIVE_RAW_TRACEPOINT_LINKS.lock();
    active.retain(|candidate| {
        candidate
            .upgrade()
            .is_some_and(|entry| !entry.detached() && !core::ptr::eq(Arc::as_ptr(&entry), link))
    });
}

/// Runs raw-tracepoint programs without allocating in syscall/scheduler
/// producer context.  Strong references are materialized in a bounded stack
/// array before releasing the registry lock, so close/detach cannot race an
/// observed callback into freed storage.
pub(crate) fn run_raw_tracepoint_links(tracepoint_id: u64, context: &mut [u8]) {
    run_raw_tracepoint_links_with_regs(tracepoint_id, context, None)
}

pub(crate) fn run_raw_tracepoint_links_with_regs(
    tracepoint_id: u64,
    context: &mut [u8],
    regs: Option<&[u8]>,
) {
    let mut snapshot: [Option<Arc<crate::file::bpf::BpfRawTracepointLink>>;
        MAX_ACTIVE_RAW_TRACEPOINT_LINKS] = [const { None }; MAX_ACTIVE_RAW_TRACEPOINT_LINKS];
    let count = {
        let mut active = ACTIVE_RAW_TRACEPOINT_LINKS.lock();
        active.retain(|candidate| candidate.upgrade().is_some_and(|link| !link.detached()));
        let mut count = 0;
        for link in active.iter().filter_map(Weak::upgrade) {
            if count == snapshot.len() {
                break;
            }
            snapshot[count] = Some(link);
            count += 1;
        }
        count
    };
    for link in snapshot[..count].iter().flatten() {
        let _ = link.run(tracepoint_id, context, regs);
    }
}

/// Activated struct_ops maps are weakly registered at real kernel producer
/// hooks.  The map itself owns the callback program, so closing its last FD
/// (and unpinning it) removes the hook without a stale strong global object.
static ACTIVE_STRUCT_OPS: SpinNoIrq<Vec<Weak<dyn map::BpfMap>>> = SpinNoIrq::new(Vec::new());

/// Namespace packet links are weakly published.  Their strong ownership is
/// the link FD or bpffs pin, never the packet producer, so final close is a
/// real detach even if no packet arrives afterwards.
static ACTIVE_NETWORK_LINKS: SpinNoIrq<Vec<Weak<crate::file::bpf::BpfNetworkLink>>> =
    SpinNoIrq::new(Vec::new());

const BPF_F_ALLOW_OVERRIDE: u32 = 1;
const BPF_F_ALLOW_MULTI: u32 = 2;

pub(crate) fn activate_network_link(
    link: &Arc<crate::file::bpf::BpfNetworkLink>,
) -> axerrno::AxResult<()> {
    let mut active = ACTIVE_NETWORK_LINKS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    if active
        .iter()
        .any(|candidate| candidate.ptr_eq(&Arc::downgrade(link)))
    {
        return Ok(());
    }
    active.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    active.push(Arc::downgrade(link));
    Ok(())
}

/// Publish an XDP device attachment as one registry transaction.  Linux's
/// normal XDP mode permits one dispatcher program for a netdev, so accepting
/// a second live `(netns, ifindex)` link would make packet ownership depend
/// on an arbitrary registry iteration order.
pub(crate) fn activate_xdp_link(
    link: &Arc<crate::file::bpf::BpfNetworkLink>,
) -> axerrno::AxResult<()> {
    let (namespace, ifindex) = link
        .xdp_attachment_identity()
        .ok_or(AxError::InvalidInput)?;
    let mut active = ACTIVE_NETWORK_LINKS.lock();
    active.retain(|candidate| candidate.upgrade().is_some_and(|entry| !entry.detached()));
    if active
        .iter()
        .filter_map(Weak::upgrade)
        .any(|entry| entry.xdp_attachment_matches(&namespace, ifindex))
    {
        return Err(AxError::AlreadyExists);
    }
    active.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    active.push(Arc::downgrade(link));
    Ok(())
}

pub(crate) fn deactivate_network_link(link: &crate::file::bpf::BpfNetworkLink) {
    let mut active = ACTIVE_NETWORK_LINKS.lock();
    active.retain(|candidate| {
        candidate
            .upgrade()
            .is_some_and(|entry| !entry.detached() && !core::ptr::eq(Arc::as_ptr(&entry), link))
    });
}

/// Snapshot the one live XDP program attached to `ifindex` in `namespace`.
/// The namespace object, not a globally reused numeric netns ID, is compared
/// so a destroyed namespace cannot leak its policy into a later one.
pub(crate) fn xdp_program_snapshot(
    namespace: &Arc<crate::task::NetworkNamespace>,
    ifindex: u32,
) -> Option<Arc<crate::bpf::prog::BpfProgram>> {
    let mut active = ACTIVE_NETWORK_LINKS.lock();
    active.retain(|candidate| candidate.upgrade().is_some_and(|entry| !entry.detached()));
    active
        .iter()
        .filter_map(Weak::upgrade)
        .find(|link| link.xdp_attachment_matches(namespace, ifindex))
        .and_then(|link| link.xdp_program_snapshot())
}

/// Runs the snapshot selected by `xdp_program_snapshot`.  `None` means that
/// no XDP link is installed; a program's numeric XDP action is intentionally
/// returned intact so the packet router owns PASS/DROP/TX/REDIRECT policy.
pub(crate) fn run_xdp_program(
    namespace: &Arc<crate::task::NetworkNamespace>,
    ifindex: u32,
    packet: &mut [u8],
) -> axerrno::AxResult<Option<u32>> {
    let Some(program) = xdp_program_snapshot(namespace, ifindex) else {
        return Ok(None);
    };
    let stats = crate::bpf::prog::BpfStatsRunGuard::begin();
    let result = crate::bpf::helpers::BpfExecution::new(packet, &program.maps, u64::MAX)
        .with_streams(&program.streams)
        .execute(&program.mechanism);
    program.account_run(&stats);
    result.map(|(action, _)| Some(action as u32))
}

pub(crate) fn run_network_packet_links(
    namespace: &Arc<crate::task::NetworkNamespace>,
    hook: crate::file::bpf::BpfNetworkHook,
    packet: &mut [u8],
) -> axerrno::AxResult<()> {
    let mut active = ACTIVE_NETWORK_LINKS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    let mut links = Vec::new();
    links
        .try_reserve(active.len())
        .map_err(|_| AxError::NoMemory)?;
    for link in active.iter().filter_map(Weak::upgrade) {
        links.push(link);
    }
    drop(active);
    // NF priority is signed and lower values run first. `sort_by_key` is
    // stable, so same-priority links retain publication order just like an
    // ordered netfilter hook list.
    let mut ordinary = Vec::new();
    ordinary
        .try_reserve(links.len())
        .map_err(|_| AxError::NoMemory)?;
    for link in links.iter() {
        if link.cgroup_attachment().is_none() {
            ordinary.push(link.clone());
        }
    }
    ordinary.sort_by_key(|link| link.packet_priority());
    for link in ordinary {
        link.run_packet(namespace, hook, packet)?;
    }

    // Cgroup attachment is an inheritance policy, not a flat packet hook
    // list.  Walk the task's actual root-to-leaf membership once and choose
    // the effective programs at each level.  An overriding parent is
    // replaced by the first descendant attachment; a multi parent remains in
    // the chain.  Admission below guarantees that an unflagged parent cannot
    // have a reachable child attachment.
    let hierarchy = crate::pseudofs::cgroup::bpf_current_cgroup_hierarchy()?;
    let mut effective = Vec::new();
    let mut parent_flags = 0;
    for identity in hierarchy {
        let mut level = Vec::new();
        for link in links.iter() {
            if let Some((target, target_hook, flags)) = link.cgroup_attachment()
                && target == identity
                && target_hook == hook
            {
                level.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                level.push((link.clone(), flags));
            }
        }
        if level.is_empty() {
            continue;
        }
        // All links on one cgroup/attach-type share a policy.  The defensive
        // check keeps an externally pinned stale/malformed link from making
        // hierarchy dispatch depend on insertion order.
        let level_flags = level[0].1;
        if level.iter().any(|(_, flags)| *flags != level_flags) {
            return Err(axerrno::LinuxError::EBUSY.into());
        }
        if !effective.is_empty() {
            if parent_flags & BPF_F_ALLOW_MULTI != 0 {
                effective
                    .try_reserve(level.len())
                    .map_err(|_| AxError::NoMemory)?;
                effective.extend(level.iter().map(|(link, _)| link.clone()));
            } else if parent_flags & BPF_F_ALLOW_OVERRIDE != 0 {
                effective.clear();
                effective
                    .try_reserve(level.len())
                    .map_err(|_| AxError::NoMemory)?;
                effective.extend(level.iter().map(|(link, _)| link.clone()));
            } else {
                return Err(axerrno::LinuxError::EBUSY.into());
            }
        } else {
            effective
                .try_reserve(level.len())
                .map_err(|_| AxError::NoMemory)?;
            effective.extend(level.iter().map(|(link, _)| link.clone()));
        }
        parent_flags = level_flags;
    }
    for link in effective {
        link.run_packet(namespace, hook, packet)?;
    }
    Ok(())
}

/// Side-effect-free producer query for per-link IP defragmentation.  The
/// packet router invokes this before the corresponding NF seam; expired weak
/// links are pruned under the same registry discipline as normal dispatch.
pub(crate) fn network_packet_defrag_required(
    namespace: &Arc<crate::task::NetworkNamespace>,
    hook: crate::file::bpf::BpfNetworkHook,
    packet: &[u8],
) -> bool {
    let mut active = ACTIVE_NETWORK_LINKS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    active
        .iter()
        .filter_map(Weak::upgrade)
        .any(|link| link.requires_defrag(namespace, hook, packet))
}

/// Snapshot the attach policy at one cgroup/hook.  This covers both legacy
/// `BPF_PROG_ATTACH` and FD-backed `BPF_LINK_CREATE` links, whose only shared
/// authority is the live weak producer registry.
pub(crate) fn cgroup_attachment_flags(
    identity: u64,
    hook: crate::file::bpf::BpfNetworkHook,
) -> axerrno::AxResult<Vec<u32>> {
    let mut active = ACTIVE_NETWORK_LINKS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    let mut flags = Vec::new();
    for link in active.iter().filter_map(Weak::upgrade) {
        if let Some((target, target_hook, attach_flags)) = link.cgroup_attachment()
            && target == identity
            && target_hook == hook
        {
            flags.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            flags.push(attach_flags);
        }
    }
    Ok(flags)
}

/// Take a strong, point-in-time snapshot of live cgroup packet links.  The
/// link descriptor (or a legacy attachment owner), not this registry, owns
/// the reference; upgrading while the registry lock is held prevents a
/// closing link from being reported after it has detached.
pub(crate) fn cgroup_network_links(
    identity: u64,
    hook: crate::file::bpf::BpfNetworkHook,
) -> axerrno::AxResult<Vec<Arc<crate::file::bpf::BpfNetworkLink>>> {
    let mut active = ACTIVE_NETWORK_LINKS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    let mut links = Vec::new();
    for link in active.iter().filter_map(Weak::upgrade) {
        if link
            .cgroup_attachment()
            .is_some_and(|(target, target_hook, _)| target == identity && target_hook == hook)
        {
            links.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            links.push(link);
        }
    }
    Ok(links)
}

/// Equivalent live snapshot for retained socket-filter links.
pub(crate) fn socket_network_links(
    socket: &Arc<crate::file::Socket>,
) -> axerrno::AxResult<Vec<Arc<crate::file::bpf::BpfNetworkLink>>> {
    let mut active = ACTIVE_NETWORK_LINKS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    let mut links = Vec::new();
    for link in active.iter().filter_map(Weak::upgrade) {
        if link.socket_attachment_matches(socket) {
            links.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            links.push(link);
        }
    }
    Ok(links)
}

/// LSM links are held weakly by the producer side.  The link FD (or a bpffs
/// pin) is the lifetime owner, so closing the final reference cannot leave a
/// policy callback reachable through a stale global registration.
static ACTIVE_LSM_LINKS: SpinNoIrq<Vec<Weak<crate::file::bpf::BpfLsmLink>>> =
    SpinNoIrq::new(Vec::new());

pub(crate) fn register_lsm_link(link: &Arc<crate::file::bpf::BpfLsmLink>) -> axerrno::AxResult<()> {
    let mut active = ACTIVE_LSM_LINKS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    if active
        .iter()
        .any(|candidate| candidate.ptr_eq(&Arc::downgrade(link)))
    {
        return Ok(());
    }
    active.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    active.push(Arc::downgrade(link));
    Ok(())
}

/// Runs BPF-LSM links at an already frozen typed-security boundary.  The
/// context intentionally carries only a hook selector and the current task
/// ID: BPF programs never receive a raw kernel pointer into security state.
/// A non-zero result is a denial, matching BPF-LSM's MAC return convention.
pub(crate) fn run_lsm_hook(hook: u32) -> axerrno::AxResult<()> {
    let mut context = [0u8; 16];
    context[..4].copy_from_slice(&hook.to_ne_bytes());
    context[8..16].copy_from_slice(&axtask::current().id().as_u64().to_ne_bytes());
    let mut active = ACTIVE_LSM_LINKS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    for link in active.iter().filter_map(Weak::upgrade) {
        if link.run(hook, &mut context)? != 0 {
            return Err(AxError::PermissionDenied);
        }
    }
    Ok(())
}

pub(crate) fn activate_struct_ops(map: &Arc<dyn map::BpfMap>) -> axerrno::AxResult<()> {
    let mut active = ACTIVE_STRUCT_OPS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    if active
        .iter()
        .any(|candidate| candidate.ptr_eq(&Arc::downgrade(map)))
    {
        return Ok(());
    }
    active.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    active.push(Arc::downgrade(map));
    Ok(())
}

/// Called by VFS/network/perf producer paths with their stable byte context.
/// Callback errors are contained to the individual table exactly like a
/// failing BPF program at a normal hook; one table cannot suppress teardown
/// or prevent later tables from observing the event.
pub(crate) fn run_struct_ops(context: &mut [u8]) {
    let mut active = ACTIVE_STRUCT_OPS.lock();
    active.retain(|candidate| candidate.strong_count() != 0);
    let mut maps = Vec::new();
    if maps.try_reserve(active.len()).is_err() {
        return;
    }
    for map in active.iter().filter_map(Weak::upgrade) {
        maps.push(map);
    }
    drop(active);
    for map in maps {
        let _ = map.run_struct_ops(context);
    }
}

/// A bpffs dentry owns one strong BPF object reference.  The key is the VFS
/// object's generation-aware identity, so rename never changes a pin and a
/// recycled inode can never resurrect an old object.
#[derive(Clone)]
pub(crate) enum PinnedObject {
    Map {
        map: Arc<dyn map::BpfMap>,
        id: u32,
        name: [u8; defs::BPF_OBJ_NAME_LEN],
        btf: Option<Arc<btf::BpfBtf>>,
        charge: Arc<crate::bpf_security::BpfMemoryCharge>,
    },
    Program(Arc<prog::BpfProgram>),
    Btf(Arc<btf::BpfBtf>),
    /// A pinned link retains the exact link object; reopening it creates a
    /// second descriptor for that one lifecycle, not a detached copy.
    PerfEventLink(Arc<crate::file::bpf::BpfPerfEventLink>),
    IterLink(Arc<crate::file::bpf::BpfIterLink>),
    LsmLink(Arc<crate::file::bpf::BpfLsmLink>),
    NetworkLink(Arc<crate::file::bpf::BpfNetworkLink>),
    RawTracepointLink(Arc<crate::file::bpf::BpfRawTracepointLink>),
}

struct PinEntry {
    key: ObjectKey,
    object: PinnedObject,
}

static PINNED_OBJECTS: SpinNoIrq<Vec<PinEntry>> = SpinNoIrq::new(Vec::new());
/// Reservations are made before the filesystem dentry is created.  This
/// makes a failed allocation leave no inert file behind at the requested pin
/// path, while not holding a BPF lock across a VFS transaction.
static PIN_RESERVATIONS: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct PinReservation {
    consumed: bool,
}

impl Drop for PinReservation {
    fn drop(&mut self) {
        if !self.consumed {
            PIN_RESERVATIONS.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

pub(crate) fn reserve_pin_slot() -> axerrno::AxResult<PinReservation> {
    let mut pins = PINNED_OBJECTS.lock();
    let pending = PIN_RESERVATIONS.load(Ordering::Acquire);
    pins.try_reserve(pending.saturating_add(1))
        .map_err(|_| AxError::NoMemory)?;
    PIN_RESERVATIONS.fetch_add(1, Ordering::AcqRel);
    Ok(PinReservation { consumed: false })
}

pub(crate) fn publish_pin(
    reservation: &mut PinReservation,
    location: &Location,
    object: PinnedObject,
) -> axerrno::AxResult<()> {
    let key = location.entry().object_key();
    let mut pins = PINNED_OBJECTS.lock();
    if pins.iter().any(|entry| entry.key == key) {
        return Err(AxError::AlreadyExists);
    }
    // `reserve_pin_slot` already made this append allocation-free.  Keep the
    // check as a corruption guard rather than silently publishing an
    // unreserved dentry.
    if PIN_RESERVATIONS.load(Ordering::Acquire) == 0 {
        return Err(AxError::BadState);
    }
    pins.push(PinEntry { key, object });
    PIN_RESERVATIONS.fetch_sub(1, Ordering::AcqRel);
    reservation.consumed = true;
    Ok(())
}

pub(crate) fn pinned_object(location: &Location) -> Option<PinnedObject> {
    let key = location.entry().object_key();
    PINNED_OBJECTS
        .lock()
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.object.clone())
}

/// Called only after a VFS unlink has committed its final-link transition.
/// The pin's object reference then disappears exactly with its bpffs dentry;
/// non-final hard-link removal deliberately leaves it intact.
pub(crate) fn forget_pinned_object(location: &Location) {
    let key = location.entry().object_key();
    PINNED_OBJECTS.lock().retain(|entry| entry.key != key);
}

pub(crate) fn register_map_id(
    id: u32,
    map: &Arc<dyn map::BpfMap>,
    charge: &Arc<crate::bpf_security::BpfMemoryCharge>,
    name: [u8; defs::BPF_OBJ_NAME_LEN],
    btf: Option<Arc<btf::BpfBtf>>,
) -> axerrno::AxResult<()> {
    let mut entries = MAP_IDS.lock();
    entries.retain(|entry| entry.map.strong_count() != 0);
    entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    entries.push(MapIdEntry {
        id,
        map: Arc::downgrade(map),
        charge: Arc::downgrade(charge),
        name,
        btf,
    });
    Ok(())
}
pub(crate) fn register_prog_id(id: u32, program: &Arc<prog::BpfProgram>) -> axerrno::AxResult<()> {
    let mut entries = PROG_IDS.lock();
    entries.retain(|entry| entry.program.strong_count() != 0);
    entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    entries.push(ProgIdEntry {
        id,
        program: Arc::downgrade(program),
    });
    Ok(())
}
pub(crate) fn next_map_id(start: u32) -> Option<u32> {
    let mut entries = MAP_IDS.lock();
    entries.retain(|entry| entry.map.strong_count() != 0);
    entries
        .iter()
        .filter(|entry| entry.id > start)
        .map(|entry| entry.id)
        .min()
}
pub(crate) fn next_prog_id(start: u32) -> Option<u32> {
    let mut entries = PROG_IDS.lock();
    entries.retain(|entry| entry.program.strong_count() != 0);
    entries
        .iter()
        .filter(|entry| entry.id > start)
        .map(|entry| entry.id)
        .min()
}
pub(crate) fn map_by_id(
    id: u32,
) -> Option<(
    Arc<dyn map::BpfMap>,
    Arc<crate::bpf_security::BpfMemoryCharge>,
    [u8; defs::BPF_OBJ_NAME_LEN],
    Option<Arc<btf::BpfBtf>>,
)> {
    let mut entries = MAP_IDS.lock();
    entries.retain(|entry| entry.map.strong_count() != 0);
    let entry = entries.iter().find(|entry| entry.id == id)?;
    Some((
        entry.map.upgrade()?,
        entry.charge.upgrade()?,
        entry.name,
        entry.btf.clone(),
    ))
}
pub(crate) fn prog_by_id(id: u32) -> Option<Arc<prog::BpfProgram>> {
    let mut entries = PROG_IDS.lock();
    entries.retain(|entry| entry.program.strong_count() != 0);
    entries
        .iter()
        .find(|entry| entry.id == id)?
        .program
        .upgrade()
}

pub fn alloc_map_id() -> u32 {
    NEXT_MAP_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn alloc_prog_id() -> u32 {
    NEXT_PROG_ID.fetch_add(1, Ordering::Relaxed)
}

/// Read bpf attr from user space. Reads `min(attr_size, size_of::<T>())` bytes,
/// zero-fills the rest. This provides forward/backward compatibility.
pub fn read_bpf_attr<M: UserMemory + ?Sized, T: AnyBitPattern>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> axerrno::AxResult<T> {
    let attr_size = attr_size as usize;
    let want = core::mem::size_of::<T>();
    if attr_size > BPF_ATTR_MAX_SIZE {
        return Err(AxError::ArgumentListTooLong);
    }

    let copy_len = attr_size.min(want);
    if copy_len == 0 {
        return Err(AxError::InvalidInput);
    }

    let src = read_user_bytes(memory, attr_ptr, copy_len)?;
    if attr_size > want {
        let tail_ptr = attr_ptr.checked_add(want).ok_or(AxError::InvalidInput)?;
        let tail = read_user_bytes(memory, tail_ptr, attr_size - want)?;
        if tail.iter().any(|&byte| byte != 0) {
            return Err(AxError::ArgumentListTooLong);
        }
    }
    let mut buf = vec![0u8; want];
    buf[..copy_len].copy_from_slice(&src);
    Ok(bytemuck::pod_read_unaligned(&buf))
}

pub fn require_bpf_attr_range<T>(attr_size: u32, end: usize) -> axerrno::AxResult<()> {
    use axerrno::AxError;

    if end > core::mem::size_of::<T>() || (attr_size as usize) < end {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

pub fn write_bpf_attr_value<TAttr, TValue: bytemuck::NoUninit, M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
    offset: usize,
    value: &TValue,
) -> axerrno::AxResult<()> {
    use axerrno::AxError;

    let end = offset
        .checked_add(core::mem::size_of::<TValue>())
        .ok_or(AxError::InvalidInput)?;
    require_bpf_attr_range::<TAttr>(attr_size, end)?;
    let destination = attr_ptr.checked_add(offset).ok_or(AxError::InvalidInput)?;
    memory
        .write_bytes(destination, bytemuck::bytes_of(value))
        .map_err(map_usercopy_error)?;
    Ok(())
}

/// Copies a byte range from the address space bound to this operation.
pub fn read_user_bytes<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    len: usize,
) -> axerrno::AxResult<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`; the provider
    // initializes every byte before this function returns successfully.
    let destination = unsafe {
        core::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<MaybeUninit<u8>>(), len)
    };
    memory
        .read_bytes(ptr, destination)
        .map_err(map_usercopy_error)?;
    Ok(bytes)
}

/// Copies a typed slice from the address space bound to this operation.
pub fn read_user_slice<M: UserMemory + ?Sized, T: AnyBitPattern>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    len: usize,
) -> axerrno::AxResult<Vec<T>> {
    let mut values = vec![T::zeroed(); len];
    // SAFETY: `MaybeUninit<T>` has the same layout as `T`; the provider
    // initializes every element before this function returns successfully.
    let destination = unsafe {
        core::slice::from_raw_parts_mut(values.as_mut_ptr().cast::<MaybeUninit<T>>(), len)
    };
    memory
        .read_slice(ptr as *const T, destination)
        .map_err(map_usercopy_error)?;
    Ok(values)
}

/// Copies a byte range into the address space bound to this operation.
pub fn write_user_bytes<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: usize,
    bytes: &[u8],
) -> axerrno::AxResult<()> {
    memory.write_bytes(ptr, bytes).map_err(map_usercopy_error)
}
