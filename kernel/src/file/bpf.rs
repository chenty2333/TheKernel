//! BPF file descriptor types (BpfMapFd, BpfProgFd).
//!
//! These are thin wrappers that implement `FileLike + Pollable` so BPF objects
//! can be managed through the standard fd table and used with close(), dup(), etc.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    cmp,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;

use crate::{
    bpf::{defs::BPF_OBJ_NAME_LEN, map::BpfMap, prog::BpfProgram},
    file::{FileLike, IoDst, Kstat, anon_inode_stat},
};

static BPF_RUN_TIME_STATS_LEASES: AtomicU32 = AtomicU32::new(0);
pub(crate) fn bpf_run_time_stats_enabled() -> bool {
    BPF_RUN_TIME_STATS_LEASES.load(Ordering::Acquire) != 0
}
pub struct BpfStatsFd;
impl BpfStatsFd {
    pub fn new() -> Self {
        BPF_RUN_TIME_STATS_LEASES.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for BpfStatsFd {
    fn drop(&mut self) {
        BPF_RUN_TIME_STATS_LEASES.fetch_sub(1, Ordering::AcqRel);
    }
}
impl FileLike for BpfStatsFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-stats",
        )))
    }
    fn set_nonblocking(&self, _: bool) -> AxResult {
        Ok(())
    }
}
impl Pollable for BpfStatsFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

/// The packet boundary selected by a namespace/cgroup BPF link.  This is
/// intentionally separate from nft's rule representation: a BPF link is an
/// independently owned object and must survive nft table replacement.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum BpfNetworkHook {
    Prerouting,
    Input,
    Forward,
    Output,
    Postrouting,
}

enum BpfNetworkLinkTarget {
    Socket {
        socket: Arc<crate::file::Socket>,
        generation: u64,
    },
    /// `pf`, `hook` and `priority` are the complete netfilter attachment
    /// identity.  In particular, a v4 hook must never observe a v6 frame
    /// merely because the namespace has a shared packet pipeline.
    Netfilter {
        namespace: Arc<crate::task::NetworkNamespace>,
        hook: BpfNetworkHook,
        pf: u32,
        priority: i32,
        defrag: bool,
    },
    Cgroup {
        directory: Arc<crate::file::Directory>,
        identity: u64,
        hook: BpfNetworkHook,
        /// `BPF_F_ALLOW_{OVERRIDE,MULTI}` belongs to the cgroup attachment,
        /// not to the program.  Retain it with the link so packet dispatch
        /// can resolve the live cgroup hierarchy without consulting a
        /// userspace FD or a parallel policy table.
        attach_flags: u32,
    },
    /// An XDP attachment is owned by the network namespace and by the
    /// device's stable ifindex within that namespace.  It deliberately does
    /// not retain a route/socket descriptor: an interface can disappear and
    /// be recreated while the link's namespace identity remains meaningful.
    Xdp {
        namespace: Arc<crate::task::NetworkNamespace>,
        ifindex: u32,
    },
}

struct BpfNetworkLinkState {
    program: Arc<BpfProgram>,
    target: BpfNetworkLinkTarget,
    detached: bool,
}

/// A typed BPF link whose target is a real socket, network namespace, or
/// cgroup.  Links retain both endpoint and program; registry entries are weak
/// so closing/unpinning the final link cannot leave a packet-path callback.
pub struct BpfNetworkLink {
    state: Mutex<BpfNetworkLinkState>,
}
pub enum BpfNetworkLinkInfo {
    Cgroup {
        id: u64,
        attach_type: u32,
    },
    Netfilter {
        pf: u32,
        hook: u32,
        priority: i32,
        flags: u32,
    },
    Socket,
    Xdp {
        ifindex: u32,
    },
}

impl BpfNetworkLink {
    pub fn socket(socket: Arc<crate::file::Socket>, program: Arc<BpfProgram>) -> AxResult<Self> {
        Ok(Self {
            state: Mutex::new(BpfNetworkLinkState {
                program,
                target: BpfNetworkLinkTarget::Socket {
                    socket,
                    generation: 0,
                },
                detached: false,
            }),
        })
    }

    /// Attach only after all fallible object allocations are complete.
    pub fn activate_socket_filter(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        let program = state.program.clone();
        let BpfNetworkLinkTarget::Socket { socket, generation } = &mut state.target else {
            return Err(AxError::InvalidInput);
        };
        *generation = socket.attach_bpf_filter_link(program)?;
        Ok(())
    }

    pub fn netfilter(
        namespace: Arc<crate::task::NetworkNamespace>,
        hook: BpfNetworkHook,
        pf: u32,
        priority: i32,
        defrag: bool,
        program: Arc<BpfProgram>,
    ) -> Self {
        Self {
            state: Mutex::new(BpfNetworkLinkState {
                program,
                target: BpfNetworkLinkTarget::Netfilter {
                    namespace,
                    hook,
                    pf,
                    priority,
                    defrag,
                },
                detached: false,
            }),
        }
    }

    pub fn cgroup(
        directory: Arc<crate::file::Directory>,
        identity: u64,
        hook: BpfNetworkHook,
        attach_flags: u32,
        program: Arc<BpfProgram>,
    ) -> Self {
        Self {
            state: Mutex::new(BpfNetworkLinkState {
                program,
                target: BpfNetworkLinkTarget::Cgroup {
                    directory,
                    identity,
                    hook,
                    attach_flags,
                },
                detached: false,
            }),
        }
    }

    pub fn xdp(
        namespace: Arc<crate::task::NetworkNamespace>,
        ifindex: u32,
        program: Arc<BpfProgram>,
    ) -> AxResult<Self> {
        if ifindex == 0 {
            return Err(AxError::InvalidInput);
        }
        if program.prog_type != crate::bpf::defs::BPF_PROG_TYPE_XDP
            || (program.expected_attach_type != 0
                && program.expected_attach_type != crate::bpf::prog::BPF_XDP)
        {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            state: Mutex::new(BpfNetworkLinkState {
                program,
                target: BpfNetworkLinkTarget::Xdp { namespace, ifindex },
                detached: false,
            }),
        })
    }

    /// Identity predicate used only while the producer registry lock is
    /// held.  It makes a netdev attachment namespace-local even when two
    /// namespaces happen to allocate the same numeric interface index.
    pub(crate) fn xdp_attachment_matches(
        &self,
        namespace: &Arc<crate::task::NetworkNamespace>,
        ifindex: u32,
    ) -> bool {
        let state = self.state.lock();
        matches!(&state.target,
            BpfNetworkLinkTarget::Xdp { namespace: target, ifindex: target_ifindex }
                if !state.detached && *target_ifindex == ifindex && Arc::ptr_eq(target, namespace))
    }

    pub(crate) fn xdp_attachment_identity(
        &self,
    ) -> Option<(Arc<crate::task::NetworkNamespace>, u32)> {
        let state = self.state.lock();
        if state.detached {
            return None;
        }
        match &state.target {
            BpfNetworkLinkTarget::Xdp { namespace, ifindex } => Some((namespace.clone(), *ifindex)),
            _ => None,
        }
    }

    /// Retain a verified program while holding the link state lock, then
    /// release the lock before a caller dispatches it.  A concurrent detach
    /// either wins before this snapshot (and returns `None`) or after it,
    /// where the already-admitted packet is allowed to finish on its stable
    /// program reference.
    pub(crate) fn xdp_program_snapshot(&self) -> Option<Arc<BpfProgram>> {
        let state = self.state.lock();
        if state.detached {
            return None;
        }
        matches!(&state.target, BpfNetworkLinkTarget::Xdp { .. }).then(|| state.program.clone())
    }

    /// Snapshot the cgroup producer identity.  The registry only keeps weak
    /// links, so this is deliberately copied while holding the link state
    /// lock; a detach cannot leave a stale hierarchy policy behind.
    pub(crate) fn cgroup_attachment(&self) -> Option<(u64, BpfNetworkHook, u32)> {
        let state = self.state.lock();
        if state.detached {
            return None;
        }
        match &state.target {
            BpfNetworkLinkTarget::Cgroup {
                identity,
                hook,
                attach_flags,
                ..
            } => Some((*identity, *hook, *attach_flags)),
            _ => None,
        }
    }

    /// Match a socket-filter link by the retained socket object, never by a
    /// descriptor number which could have been closed and reused.  This is
    /// used by `BPF_PROG_QUERY` to enumerate both FD-backed and legacy links
    /// from the same producer registry.
    pub(crate) fn socket_attachment_matches(&self, socket: &Arc<crate::file::Socket>) -> bool {
        let state = self.state.lock();
        matches!(&state.target,
            BpfNetworkLinkTarget::Socket { socket: target, .. }
                if !state.detached && Arc::ptr_eq(target, socket))
    }

    pub fn update(&self, program: Arc<BpfProgram>, expected_old: Option<u32>) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.detached {
            return Err(AxError::NotFound);
        }
        if expected_old.is_some_and(|id| state.program.prog_id != id) {
            return Err(AxError::NotFound);
        }
        let valid = match &state.target {
            BpfNetworkLinkTarget::Socket { .. } => {
                program.prog_type == crate::bpf::defs::BPF_PROG_TYPE_SOCKET_FILTER
            }
            BpfNetworkLinkTarget::Netfilter { .. } => {
                program.prog_type == crate::bpf::defs::BPF_PROG_TYPE_NETFILTER
            }
            BpfNetworkLinkTarget::Cgroup { .. } => {
                program.prog_type == crate::bpf::defs::BPF_PROG_TYPE_CGROUP_SKB
            }
            BpfNetworkLinkTarget::Xdp { .. } => {
                program.prog_type == crate::bpf::defs::BPF_PROG_TYPE_XDP
            }
        };
        let expected_attach_type = match &state.target {
            BpfNetworkLinkTarget::Socket { .. } => 0,
            BpfNetworkLinkTarget::Netfilter { .. } => crate::bpf::prog::BPF_NETFILTER,
            BpfNetworkLinkTarget::Cgroup {
                hook: BpfNetworkHook::Input,
                ..
            } => crate::bpf::prog::BPF_CGROUP_INET_INGRESS,
            BpfNetworkLinkTarget::Cgroup {
                hook: BpfNetworkHook::Output,
                ..
            } => crate::bpf::prog::BPF_CGROUP_INET_EGRESS,
            BpfNetworkLinkTarget::Cgroup { .. } => return Err(AxError::BadState),
            BpfNetworkLinkTarget::Xdp { .. } => crate::bpf::prog::BPF_XDP,
        };
        if !valid
            || (program.expected_attach_type != 0
                && program.expected_attach_type != expected_attach_type)
        {
            return Err(AxError::InvalidInput);
        }
        if let BpfNetworkLinkTarget::Socket { socket, generation } = &mut state.target {
            *generation =
                socket.replace_bpf_filter_link_if_current(*generation, program.clone())?;
        }
        state.program = program;
        Ok(())
    }

    pub fn detach(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.detached {
            return Err(AxError::NotFound);
        }
        if let BpfNetworkLinkTarget::Socket { socket, generation } = &state.target {
            socket.detach_bpf_filter_link_if_current(*generation);
        }
        state.detached = true;
        drop(state);
        crate::bpf::deactivate_network_link(self);
        crate::bpf::unregister_network_link(self);
        Ok(())
    }

    pub(crate) fn detached(&self) -> bool {
        self.state.lock().detached
    }

    pub fn program_id(&self) -> AxResult<u32> {
        let state = self.state.lock();
        (!state.detached)
            .then_some(state.program.prog_id)
            .ok_or(AxError::NotFound)
    }

    pub fn link_type(&self) -> u32 {
        match &self.state.lock().target {
            BpfNetworkLinkTarget::Cgroup { .. } => 3, // BPF_LINK_TYPE_CGROUP
            BpfNetworkLinkTarget::Netfilter { .. } => 10, // BPF_LINK_TYPE_NETFILTER
            BpfNetworkLinkTarget::Xdp { .. } => 6,    // BPF_LINK_TYPE_XDP
            BpfNetworkLinkTarget::Socket { .. } => 0,
        }
    }
    pub fn link_info(&self) -> BpfNetworkLinkInfo {
        match &self.state.lock().target {
            BpfNetworkLinkTarget::Cgroup { identity, hook, .. } => BpfNetworkLinkInfo::Cgroup {
                id: *identity,
                attach_type: match hook {
                    BpfNetworkHook::Input => crate::bpf::prog::BPF_CGROUP_INET_INGRESS,
                    BpfNetworkHook::Output => crate::bpf::prog::BPF_CGROUP_INET_EGRESS,
                    _ => 0,
                },
            },
            BpfNetworkLinkTarget::Netfilter {
                pf,
                hook,
                priority,
                defrag,
                ..
            } => BpfNetworkLinkInfo::Netfilter {
                pf: *pf,
                hook: match hook {
                    BpfNetworkHook::Prerouting => 0,
                    BpfNetworkHook::Input => 1,
                    BpfNetworkHook::Forward => 2,
                    BpfNetworkHook::Output => 3,
                    BpfNetworkHook::Postrouting => 4,
                },
                priority: *priority,
                flags: if *defrag { 1 } else { 0 },
            },
            BpfNetworkLinkTarget::Socket { .. } => BpfNetworkLinkInfo::Socket,
            BpfNetworkLinkTarget::Xdp { ifindex, .. } => {
                BpfNetworkLinkInfo::Xdp { ifindex: *ifindex }
            }
        }
    }

    /// Netfilter invokes hooks in ascending signed priority order.  Non-NF
    /// links deliberately use priority zero; stable sorting in the producer
    /// keeps their creation ordering deterministic without affecting NF.
    pub(crate) fn packet_priority(&self) -> i32 {
        let state = self.state.lock();
        match &state.target {
            BpfNetworkLinkTarget::Netfilter { priority, .. } => *priority,
            _ => 0,
        }
    }

    /// Whether this concrete NF link requests IP reassembly before the given
    /// hook.  This is intentionally separate from `run_packet`: a query does
    /// not execute BPF on a partial packet.
    pub(crate) fn requires_defrag(
        &self,
        namespace: &Arc<crate::task::NetworkNamespace>,
        hook: BpfNetworkHook,
        packet: &[u8],
    ) -> bool {
        let state = self.state.lock();
        match &state.target {
            BpfNetworkLinkTarget::Netfilter {
                namespace: target,
                hook: target_hook,
                pf,
                defrag,
                ..
            } => {
                !state.detached
                    && *defrag
                    && Arc::ptr_eq(target, namespace)
                    && *target_hook == hook
                    && match *pf {
                        2 => packet.first().is_some_and(|b| b >> 4 == 4),
                        10 => packet.first().is_some_and(|b| b >> 4 == 6),
                        _ => false,
                    }
            }
            _ => false,
        }
    }

    /// Invoked only from the namespace packet pipeline.  A zero BPF return
    /// is the Linux cgroup/netfilter drop verdict; any positive value accepts
    /// the packet after allowing the program's in-place edits.
    pub(crate) fn run_packet(
        &self,
        namespace: &Arc<crate::task::NetworkNamespace>,
        hook: BpfNetworkHook,
        packet: &mut [u8],
    ) -> AxResult<()> {
        let state = self.state.lock();
        if state.detached {
            return Ok(());
        }
        let applies = match &state.target {
            BpfNetworkLinkTarget::Socket { .. } => false,
            BpfNetworkLinkTarget::Netfilter {
                namespace: target,
                hook: target_hook,
                pf,
                ..
            } => {
                let family_matches = match *pf {
                    // AF_INET / AF_INET6.  A malformed frame is not accepted
                    // as either family and is left to the normal packet
                    // parser, exactly as an NF protocol-family hook is.
                    2 => packet.first().is_some_and(|byte| byte >> 4 == 4),
                    10 => packet.first().is_some_and(|byte| byte >> 4 == 6),
                    _ => false,
                };
                family_matches && Arc::ptr_eq(target, namespace) && *target_hook == hook
            }
            BpfNetworkLinkTarget::Cgroup {
                directory,
                identity,
                hook: target_hook,
                ..
            } => {
                // Keep the directory OFD alive as the target's namespace and
                // hierarchy anchor, while matching packet attribution through
                // the authoritative cgroup membership index.
                let _ = directory;
                *target_hook == hook && crate::pseudofs::cgroup::bpf_current_in_cgroup(*identity)
            }
            BpfNetworkLinkTarget::Xdp { .. } => false,
        };
        if !applies {
            return Ok(());
        }
        let stats = crate::bpf::prog::BpfStatsRunGuard::begin();
        let result = crate::bpf::helpers::BpfExecution::new(packet, &state.program.maps, u64::MAX)
            .with_streams(&state.program.streams)
            .execute(&state.program.mechanism);
        state.program.account_run(&stats);
        let (verdict, _) = result?;
        if verdict == 0 {
            return Err(AxError::PermissionDenied);
        }
        Ok(())
    }
}

impl FileLike for BpfNetworkLink {
    fn final_close(&self) {
        let _ = self.detach();
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-network-link",
        )))
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}
impl Pollable for BpfNetworkLink {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

/// An immutable BTF object.  Keeping the `Arc` in the descriptor makes BTF
/// IDs disappear exactly when their last FD/user (such as a typed map) drops.
pub struct BpfBtfFd {
    pub object: Arc<crate::bpf::btf::BpfBtf>,
}

impl BpfBtfFd {
    pub fn new(object: Arc<crate::bpf::btf::BpfBtf>) -> Self {
        Self { object }
    }
}

impl FileLike for BpfBtfFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-btf",
        )))
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}
impl Pollable for BpfBtfFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

/// Delegatable BPF authority.  The token is intentionally an actual FD-owned
/// capability rather than a numeric flag: close/exec/SOCK_RIGHTS all retain
/// the same kernel object and there is no ambient token lookup.
pub struct BpfTokenFd {
    /// Immutable, namespace-anchored capability snapshot.  The directory is
    /// retained separately below so the bpffs anchor cannot disappear while
    /// delegated authority remains usable.
    pub grant: crate::bpf_security::BpfTokenGrant,
    pub bpffs: Arc<crate::file::Directory>,
}
impl BpfTokenFd {
    pub fn new(
        authority: crate::bpf_security::BpfAuthority,
        bpffs: Arc<crate::file::Directory>,
    ) -> Self {
        Self {
            grant: crate::bpf_security::BpfTokenGrant::from_current(authority),
            bpffs,
        }
    }
}
impl FileLike for BpfTokenFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-token",
        )))
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}
impl Pollable for BpfTokenFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

/// Object held by a BPF iterator link.  An iterator link retains the verified
/// program and an actual kernel object, not an FD number that may be reused.
pub enum BpfIterSource {
    /// The current mount namespace is sampled at `BPF_ITER_CREATE` time.
    Mounts,
    /// Map iteration retains the map and therefore its charge through a live
    /// iterator even when userspace closes the original map descriptor.
    Map { map: Arc<dyn BpfMap>, id: u32 },
    /// Socket and perf targets are represented by their retained OFD object.
    Object(Arc<dyn FileLike>),
}

pub struct BpfIterLink {
    program: Arc<BpfProgram>,
    source: BpfIterSource,
    detached: AtomicBool,
}

impl BpfIterLink {
    pub fn new(program: Arc<BpfProgram>, source: BpfIterSource) -> Self {
        Self {
            program,
            source,
            detached: AtomicBool::new(false),
        }
    }

    pub fn detach(&self) -> AxResult<()> {
        if self.detached.swap(true, Ordering::AcqRel) {
            return Err(AxError::NotFound);
        }
        Ok(())
    }
    pub fn program_id(&self) -> AxResult<u32> {
        (!self.detached.load(Ordering::Acquire))
            .then_some(self.program.prog_id)
            .ok_or(AxError::NotFound)
    }
    pub fn map_id(&self) -> u32 {
        match &self.source {
            BpfIterSource::Map { id, .. } => *id,
            _ => 0,
        }
    }

    fn append_row(out: &mut Vec<u8>, row: &mut [u8], program: &BpfProgram) -> AxResult<bool> {
        // Iterator programs receive a writable private record.  Helper side
        // effects therefore use the ordinary verifier/map lifetime path, and
        // a non-zero result terminates this per-open traversal.
        let stats = crate::bpf::prog::BpfStatsRunGuard::begin();
        let execution = crate::bpf::helpers::BpfExecution::new(row, &program.maps, 4096)
            .with_streams(&program.streams)
            .execute(&program.mechanism);
        program.account_run(&stats);
        let (result, _) = execution?;
        if result != 0 {
            return Ok(false);
        }
        let len = u32::try_from(row.len()).map_err(|_| AxError::OutOfRange)?;
        out.try_reserve(4usize.checked_add(row.len()).ok_or(AxError::NoMemory)?)
            .map_err(|_| AxError::NoMemory)?;
        out.extend_from_slice(&len.to_ne_bytes());
        out.extend_from_slice(row);
        Ok(true)
    }

    fn snapshot(&self) -> AxResult<Vec<u8>> {
        if self.detached.load(Ordering::Acquire) {
            return Err(AxError::NotFound);
        }
        let mut out = Vec::new();
        match &self.source {
            BpfIterSource::Mounts => {
                for mount in crate::mounts::snapshot()? {
                    let mut row = [0u8; 32];
                    row[..8].copy_from_slice(&mount.mount_id.to_ne_bytes());
                    row[8..16].copy_from_slice(&mount.parent_id.to_ne_bytes());
                    row[16..24].copy_from_slice(&mount.dev.to_ne_bytes());
                    row[24..28].copy_from_slice(&mount.flags.to_ne_bytes());
                    row[28..32].copy_from_slice(&mount.mount_id_old.to_ne_bytes());
                    if !Self::append_row(&mut out, &mut row, &self.program)? {
                        break;
                    }
                }
            }
            BpfIterSource::Map { map, .. } => {
                let mut previous = None;
                while let Some(key) = map.get_next_key(previous.as_deref()) {
                    let Some(value) = map.lookup(&key) else {
                        previous = Some(key);
                        continue;
                    };
                    let length = key
                        .len()
                        .checked_add(value.len())
                        .ok_or(AxError::NoMemory)?;
                    let mut row = Vec::new();
                    row.try_reserve(length).map_err(|_| AxError::NoMemory)?;
                    row.extend_from_slice(&key);
                    row.extend_from_slice(&value);
                    previous = Some(key);
                    if !Self::append_row(&mut out, &mut row, &self.program)? {
                        break;
                    }
                }
            }
            BpfIterSource::Object(object) => {
                // A retained socket/perf OFD is sampled by its stable anon
                // inode path.  This is deliberately a byte record: no UTF-8
                // conversion can corrupt a namespace object name.
                let path = object.path()?;
                let mut row = Vec::new();
                row.try_reserve(path.as_ref().as_bytes().len())
                    .map_err(|_| AxError::NoMemory)?;
                row.extend_from_slice(path.as_ref().as_bytes());
                let _ = Self::append_row(&mut out, &mut row, &self.program)?;
            }
        }
        Ok(out)
    }
}

impl FileLike for BpfIterLink {
    fn final_close(&self) {
        let _ = self.detach();
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-iter-link",
        )))
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}
impl Pollable for BpfIterLink {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

/// Read-only iterator instance.  It owns a per-open immutable snapshot and
/// cursor; each `BPF_ITER_CREATE` therefore has independent read/close state.
pub struct BpfIterFd {
    bytes: Mutex<Vec<u8>>,
    cursor: Mutex<usize>,
}
impl BpfIterFd {
    pub fn from_link(link: &BpfIterLink) -> AxResult<Self> {
        Ok(Self {
            bytes: Mutex::new(link.snapshot()?),
            cursor: Mutex::new(0),
        })
    }
}
impl FileLike for BpfIterFd {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let bytes = self.bytes.lock();
        let mut cursor = self.cursor.lock();
        if *cursor >= bytes.len() || dst.remaining_mut() == 0 {
            return Ok(0);
        }
        let count = cmp::min(dst.remaining_mut(), bytes.len() - *cursor);
        dst.write(&bytes[*cursor..*cursor + count])?;
        *cursor += count;
        Ok(count)
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-iter",
        )))
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}
impl Pollable for BpfIterFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

// ---------------------------------------------------------------------------
// BpfMapFd
// ---------------------------------------------------------------------------

pub struct BpfMapFd {
    pub map: Arc<dyn BpfMap>,
    pub map_id: u32,
    pub name: [u8; BPF_OBJ_NAME_LEN],
    pub btf: Option<Arc<crate::bpf::btf::BpfBtf>>,
    pub(crate) memory_charge: Arc<crate::bpf_security::BpfMemoryCharge>,
    access: BpfMapAccess,
}

/// `BPF_OBJ_GET` creates a normal BPF descriptor with optional directional
/// map rights.  Keep those rights on the open descriptor, not in the shared
/// map, so one read-only reopen cannot weaken another caller's descriptor.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum BpfMapAccess {
    ReadWrite,
    ReadOnly,
    WriteOnly,
}

impl BpfMapFd {
    pub fn new(
        map: Arc<dyn BpfMap>,
        map_id: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        memory_charge: Arc<crate::bpf_security::BpfMemoryCharge>,
        btf: Option<Arc<crate::bpf::btf::BpfBtf>>,
    ) -> Self {
        Self {
            map,
            map_id,
            name,
            btf,
            memory_charge,
            access: BpfMapAccess::ReadWrite,
        }
    }

    pub fn new_with_file_flags(
        map: Arc<dyn BpfMap>,
        map_id: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        memory_charge: Arc<crate::bpf_security::BpfMemoryCharge>,
        file_flags: u32,
        btf: Option<Arc<crate::bpf::btf::BpfBtf>>,
    ) -> AxResult<Self> {
        let access = match file_flags {
            0 => BpfMapAccess::ReadWrite,
            thekernel_linux_bpf::BPF_F_RDONLY => BpfMapAccess::ReadOnly,
            thekernel_linux_bpf::BPF_F_WRONLY => BpfMapAccess::WriteOnly,
            _ => return Err(AxError::InvalidInput),
        };
        Ok(Self {
            map,
            map_id,
            name,
            btf,
            memory_charge,
            access,
        })
    }

    pub fn require_read(&self) -> AxResult<()> {
        if self.access == BpfMapAccess::WriteOnly {
            Err(AxError::PermissionDenied)
        } else {
            Ok(())
        }
    }

    pub fn require_write(&self) -> AxResult<()> {
        if self.access == BpfMapAccess::ReadOnly {
            Err(AxError::PermissionDenied)
        } else {
            Ok(())
        }
    }
}

impl FileLike for BpfMapFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-map",
        )))
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // The map fd has no blocking file operation; the OFD owns the flag.
        Ok(())
    }
}

impl Pollable for BpfMapFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

// ---------------------------------------------------------------------------
// BpfProgFd
// ---------------------------------------------------------------------------

pub struct BpfProgFd {
    pub prog: Arc<BpfProgram>,
}

/// Descriptor-owned attachment created by `BPF_RAW_TRACEPOINT_OPEN`.
/// The global producer registry retains only a weak reference, so final close
/// is the detach boundary even if that tracepoint never fires again.
pub struct BpfRawTracepointLink {
    state: Mutex<BpfRawTracepointLinkState>,
}

struct BpfRawTracepointLinkState {
    program: Arc<BpfProgram>,
    tracepoint_id: u64,
    tracepoint_name: &'static str,
    cookie: u64,
    detached: bool,
}

impl BpfRawTracepointLink {
    pub fn new(
        program: Arc<BpfProgram>,
        tracepoint_id: u64,
        tracepoint_name: &'static str,
        cookie: u64,
    ) -> Self {
        Self {
            state: Mutex::new(BpfRawTracepointLinkState {
                program,
                tracepoint_id,
                tracepoint_name,
                cookie,
                detached: false,
            }),
        }
    }

    pub fn detach(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.detached {
            return Err(AxError::NotFound);
        }
        state.detached = true;
        drop(state);
        crate::bpf::deactivate_raw_tracepoint_link(self);
        crate::bpf::unregister_raw_tracepoint_link(self);
        Ok(())
    }

    pub(crate) fn detached(&self) -> bool {
        self.state.lock().detached
    }

    pub fn program_id(&self) -> AxResult<u32> {
        let state = self.state.lock();
        (!state.detached)
            .then_some(state.program.prog_id)
            .ok_or(AxError::NotFound)
    }

    pub fn tracepoint_id(&self) -> AxResult<u64> {
        let state = self.state.lock();
        (!state.detached)
            .then_some(state.tracepoint_id)
            .ok_or(AxError::NotFound)
    }

    pub fn metadata(&self) -> AxResult<(&'static str, u64)> {
        let state = self.state.lock();
        (!state.detached)
            .then_some((state.tracepoint_name, state.cookie))
            .ok_or(AxError::NotFound)
    }

    /// Coherent descriptor-query snapshot used by `BPF_TASK_FD_QUERY`.
    pub(crate) fn task_fd_query(&self) -> AxResult<(u32, &'static str)> {
        let state = self.state.lock();
        (!state.detached)
            .then_some((state.program.prog_id, state.tracepoint_name))
            .ok_or(AxError::NotFound)
    }

    pub(crate) fn run(
        &self,
        tracepoint_id: u64,
        context: &mut [u8],
        regs: Option<&[u8]>,
    ) -> AxResult<()> {
        let state = self.state.lock();
        if state.detached || state.tracepoint_id != tracepoint_id {
            return Ok(());
        }
        let stats = crate::bpf::prog::BpfStatsRunGuard::begin();
        let execution = crate::bpf::helpers::BpfExecution::new(context, &state.program.maps, 4096)
            .with_streams(&state.program.streams);
        let execution = if let Some(regs) = regs {
            execution
                .with_raw_tracepoint_regs(regs)
                .execute(&state.program.mechanism)
        } else {
            execution.execute(&state.program.mechanism)
        };
        state.program.account_run(&stats);
        execution.map(|_| ())
    }
}

impl FileLike for BpfRawTracepointLink {
    fn final_close(&self) {
        let _ = self.detach();
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-raw-tracepoint",
        )))
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}

impl Pollable for BpfRawTracepointLink {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

/// An LSM link owns one tracing-class program published at the typed security
/// dispatch boundary.  Unlike a perf link it has no target FD: the security
/// hook registry is its concrete kernel endpoint and retains only a weak
/// reference to this descriptor-owned object.
pub struct BpfLsmLink {
    state: Mutex<BpfLsmLinkState>,
}

struct BpfLsmLinkState {
    program: Arc<BpfProgram>,
    hook: u32,
    detached: bool,
}

impl BpfLsmLink {
    pub fn new(program: Arc<BpfProgram>) -> Self {
        let hook = program.attach_btf_id;
        Self {
            state: Mutex::new(BpfLsmLinkState {
                program,
                hook,
                detached: false,
            }),
        }
    }

    pub fn update(&self, program: Arc<BpfProgram>, expected_old: Option<u32>) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.detached {
            return Err(AxError::NotFound);
        }
        if expected_old.is_some_and(|id| state.program.prog_id != id) {
            return Err(AxError::NotFound);
        }
        if program.attach_btf_id != state.hook {
            return Err(AxError::InvalidInput);
        }
        state.program = program;
        Ok(())
    }

    pub fn detach(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.detached {
            return Err(AxError::NotFound);
        }
        state.detached = true;
        Ok(())
    }
    pub fn program_id(&self) -> AxResult<u32> {
        let state = self.state.lock();
        (!state.detached)
            .then_some(state.program.prog_id)
            .ok_or(AxError::NotFound)
    }
    pub fn tracing_target(&self) -> u32 {
        self.state.lock().hook
    }

    pub(crate) fn run(&self, hook: u32, context: &mut [u8]) -> AxResult<u64> {
        let state = self.state.lock();
        if state.detached || (state.hook != 0 && state.hook != hook) {
            return Ok(0);
        }
        let stats = crate::bpf::prog::BpfStatsRunGuard::begin();
        let result = crate::bpf::helpers::BpfExecution::new(context, &state.program.maps, 4096)
            .with_streams(&state.program.streams)
            .execute(&state.program.mechanism);
        state.program.account_run(&stats);
        result.map(|result| result.0)
    }
}

impl FileLike for BpfLsmLink {
    fn final_close(&self) {
        let _ = self.detach();
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-lsm-link",
        )))
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}
impl Pollable for BpfLsmLink {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

/// A `BPF_LINK_CREATE` perf-event attachment.  The link owns references to
/// both endpoints; its final close removes the program only if no later
/// ioctl/link replacement has advanced the event generation.
pub struct BpfPerfEventLink {
    state: Mutex<BpfPerfEventLinkState>,
}

struct BpfPerfEventLinkState {
    event: Arc<crate::file::PerfEventFile>,
    program: Arc<BpfProgram>,
    attach_type: u32,
    prog_type: u32,
    generation: u64,
    detached: bool,
}

impl BpfPerfEventLink {
    pub fn new(
        event: Arc<crate::file::PerfEventFile>,
        program: Arc<BpfProgram>,
        generation: u64,
        attach_type: u32,
    ) -> Self {
        let prog_type = program.prog_type;
        Self {
            state: Mutex::new(BpfPerfEventLinkState {
                event,
                program,
                attach_type,
                prog_type,
                generation,
                detached: false,
            }),
        }
    }

    /// Complete publication after the fallible descriptor allocation has
    /// succeeded.  Keeping attachment out of construction prevents an
    /// unowned perf hook when allocation or FD publication later fails.
    pub fn set_initial_generation(&self, generation: u64) {
        self.state.lock().generation = generation;
    }

    /// Atomically changes this link's program, preserving the link's owning
    /// relationship with the perf event.  `expected_old` is the Linux
    /// BPF_F_REPLACE compare-and-swap guard.
    pub fn update(&self, program: Arc<BpfProgram>, expected_old: Option<u32>) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.detached {
            return Err(axerrno::AxError::NotFound);
        }
        if expected_old.is_some_and(|id| state.program.prog_id != id) {
            return Err(axerrno::AxError::NotFound);
        }
        if program.prog_type != state.prog_type {
            return Err(axerrno::AxError::InvalidInput);
        }
        let generation = state
            .event
            .replace_bpf_link_if_current(state.generation, program.clone())?;
        state.program = program;
        state.generation = generation;
        Ok(())
    }

    /// Explicit BPF_LINK_DETACH is idempotent only at final-close level; a
    /// second userspace detach reports that the link is already gone.
    pub fn detach(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.detached {
            return Err(axerrno::AxError::NotFound);
        }
        state.event.detach_bpf_link_if_current(state.generation);
        state.detached = true;
        Ok(())
    }

    pub fn program_id(&self) -> AxResult<u32> {
        let state = self.state.lock();
        (!state.detached)
            .then_some(state.program.prog_id)
            .ok_or(axerrno::AxError::NotFound)
    }

    pub fn attach_type(&self) -> AxResult<u32> {
        let state = self.state.lock();
        (!state.detached)
            .then_some(state.attach_type)
            .ok_or(axerrno::AxError::NotFound)
    }
    pub fn link_info_data(&self) -> AxResult<[u8; 48]> {
        let state = self.state.lock();
        (!state.detached)
            .then(|| state.event.bpf_link_info_data())
            .ok_or(AxError::NotFound)
    }
    pub fn link_info_name(&self) -> AxResult<Option<&'static str>> {
        let state = self.state.lock();
        (!state.detached)
            .then(|| state.event.bpf_link_info_name())
            .transpose()?
            .ok_or(AxError::NotFound)
    }
}

impl FileLike for BpfPerfEventLink {
    fn final_close(&self) {
        // A descriptor close is the lifetime terminator, not a second
        // BPF_LINK_DETACH request.  It therefore tolerates an earlier detach.
        let mut state = self.state.lock();
        if !state.detached {
            state.event.detach_bpf_link_if_current(state.generation);
            state.detached = true;
        }
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-perf-event-link",
        )))
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}

impl Pollable for BpfPerfEventLink {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

impl BpfProgFd {
    pub fn new(prog: Arc<BpfProgram>) -> Self {
        Self { prog }
    }
}

impl FileLike for BpfProgFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:bpf-prog",
        )))
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // The program fd has no blocking file operation; the OFD owns the flag.
        Ok(())
    }
}

impl Pollable for BpfProgFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}
