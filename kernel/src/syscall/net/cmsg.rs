use alloc::{
    alloc::{Layout, alloc},
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::{MaybeUninit, size_of},
    sync::atomic::{AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axnet::CMsgData;
use linux_raw_sys::net::{SCM_CREDENTIALS, SCM_RIGHTS, SOL_SOCKET, cmsghdr, ucred};
use spin::{Lazy, Mutex};

use crate::{
    file::{FileDescription, ScmDescriptorCustody, get_file_description_for_scm, try_reserve_fd},
    mm::{UserMemoryCapability, UserPtr, map_usercopy_error},
};

/// Linux's per-message hard limit from `net/core/scm.c`.
pub const SCM_MAX_FD: usize = 253;

// Conservatively charge more than just the Arc slot. This represents the
// retained OFD reference plus queue/control metadata and prevents empty Unix
// datagrams from turning fd references into an unmetered resource.
const SCM_RIGHTS_FD_QUEUE_CHARGE: usize = 64;

// Until Credential v2 can key this ledger by user namespace and real kuid,
// impose a hard system-wide ceiling. This is deliberately independent of a
// destination socket's byte budget: blocking sendmsg callers may hold their
// owned SCM_RIGHTS snapshot while waiting to obtain that socket admission.
const SCM_RIGHTS_INFLIGHT_LIMIT: usize = 16_384;
static SCM_RIGHTS_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
static SCM_RIGHTS_GRAPH: Lazy<Mutex<Vec<Weak<RightsGraphNode>>>> =
    Lazy::new(|| Mutex::new(Vec::new()));
static UNIX_ENDPOINT_OWNERS: Lazy<Mutex<Vec<(usize, Weak<FileDescription>)>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

/// One queued SCM_RIGHTS edge set. The retained Arc values are deliberately
/// behind a short mutex so the collector can sever only unreachable SCCs;
/// ordinary receive/queue-drop keeps the same exact object and cannot race a
/// partially copied user control message.
pub(crate) struct RightsGraphNode {
    pub(crate) fds: Mutex<Vec<ScmDescriptorCustody>>,
    owner: Mutex<Weak<FileDescription>>,
}

fn register_rights_graph(fds: Vec<ScmDescriptorCustody>) -> AxResult<Arc<RightsGraphNode>> {
    let node = Arc::try_new(RightsGraphNode {
        fds: Mutex::new(fds),
        owner: Mutex::new(Weak::new()),
    })
    .map_err(|_| AxError::NoMemory)?;
    let mut graph = SCM_RIGHTS_GRAPH.lock();
    graph.retain(|entry| entry.strong_count() != 0);
    graph.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    graph.push(Arc::downgrade(&node));
    Ok(node)
}

/// Mark/sweep the SCM_RIGHTS ownership graph. A FileDescription whose strong
/// count exceeds the number of queued SCM edges entering it is rooted by an
/// fd table, VMA, asynchronous operation, or another non-SCM owner. Marking
/// then follows every queued edge. Unmarked edge sets are precisely the
/// unreachable Unix socket/epoll cycles and are severed together.
pub(crate) fn collect_scm_rights_cycles() {
    let mut graph = SCM_RIGHTS_GRAPH.lock();
    graph.retain(|entry| entry.strong_count() != 0);
    let mut incoming: Vec<(u64, usize)> = Vec::new();
    let mut marked: Vec<u64> = Vec::new();
    for weak in graph.iter() {
        let Some(node) = weak.upgrade() else {
            continue;
        };
        for fd in node.fds.lock().iter() {
            let id = fd.description().id().get();
            if let Some((_, count)) = incoming.iter_mut().find(|(known, _)| *known == id) {
                *count += 1;
            } else if incoming.try_reserve(1).is_err() {
                return;
            } else {
                incoming.push((id, 1));
            }
        }
    }
    for weak in graph.iter() {
        let Some(node) = weak.upgrade() else {
            continue;
        };
        for fd in node.fds.lock().iter() {
            let description = fd.description();
            let id = description.id().get();
            let edges = incoming
                .iter()
                .find(|(known, _)| *known == id)
                .map(|(_, count)| *count)
                .unwrap_or(0);
            // The borrowed value below does not clone the Arc. Therefore an
            // incoming SCM edge accounts for exactly one strong reference and
            // any surplus is a real fd-table/VMA/in-flight-operation root.
            if description.has_live_descriptor_references()
                || Arc::strong_count(description) > edges
            {
                if marked.try_reserve(1).is_err() {
                    return;
                }
                marked.push(id);
            }
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for weak in graph.iter() {
            let Some(node) = weak.upgrade() else {
                continue;
            };
            if let Some(owner) = node.owner.lock().upgrade()
                && marked.contains(&owner.id().get())
            {
                for fd in node.fds.lock().iter() {
                    let id = fd.description().id().get();
                    if !marked.contains(&id) {
                        if marked.try_reserve(1).is_err() {
                            return;
                        }
                        marked.push(id);
                        changed = true;
                    }
                }
            }
            // Do not follow a target back through its message to sibling
            // targets. A rooted transferred fd does not make a dead receiver
            // or its other queued rights reachable; the only graph edge is
            // live owner -> message -> each target.
        }
    }
    for weak in graph.iter() {
        let Some(node) = weak.upgrade() else {
            continue;
        };
        let should_sweep = {
            let fds = node.fds.lock();
            let owner_reachable = node
                .owner
                .lock()
                .upgrade()
                .is_some_and(|owner| marked.contains(&owner.id().get()));
            !owner_reachable
                && !fds.is_empty()
                && !fds
                    .iter()
                    .any(|fd| marked.contains(&fd.description().id().get()))
        };
        if should_sweep {
            node.fds.lock().clear();
        }
    }
}

pub(crate) fn set_scm_rights_owner(cmsgs: &mut [CMsgData], owner: &Arc<FileDescription>) {
    for cmsg in cmsgs {
        if let Some(CMsg::Rights { graph, .. }) = cmsg.downcast_mut::<CMsg>() {
            *graph.owner.lock() = Arc::downgrade(owner);
        }
    }
}

pub(crate) fn register_unix_endpoint_owner(
    endpoint: usize,
    owner: &Arc<FileDescription>,
) -> AxResult<()> {
    let mut owners = UNIX_ENDPOINT_OWNERS.lock();
    owners.retain(|(_, weak)| weak.strong_count() != 0);
    if let Some((_, known)) = owners.iter_mut().find(|(known, _)| *known == endpoint) {
        *known = Arc::downgrade(owner);
        return Ok(());
    }
    owners.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    owners.push((endpoint, Arc::downgrade(owner)));
    Ok(())
}

pub(crate) fn set_scm_rights_endpoint_owner(cmsgs: &mut [CMsgData], endpoint: usize) {
    let owner = UNIX_ENDPOINT_OWNERS
        .lock()
        .iter()
        .find(|(known, _)| *known == endpoint)
        .and_then(|(_, owner)| owner.upgrade());
    if let Some(owner) = owner {
        set_scm_rights_owner(cmsgs, &owner);
    }
}

fn try_acquire_scm_rights(count: usize) -> AxResult<()> {
    SCM_RIGHTS_INFLIGHT
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current
                .checked_add(count)
                .filter(|next| *next <= SCM_RIGHTS_INFLIGHT_LIMIT)
        })
        .map_err(|_| AxError::from(LinuxError::ENOBUFS))?;
    Ok(())
}

/// RAII ownership of the global inflight charge. Every fallible construction
/// path retains this guard until the final CMsg value owns it, so graph-node,
/// clone callback, and type-erasure allocation failures cannot leak quota.
pub struct ScmRightsReservation {
    count: usize,
}

impl ScmRightsReservation {
    fn acquire(count: usize) -> AxResult<Self> {
        try_acquire_scm_rights(count)?;
        Ok(Self { count })
    }
}

impl Drop for ScmRightsReservation {
    fn drop(&mut self) {
        SCM_RIGHTS_INFLIGHT.fetch_sub(self.count, Ordering::AcqRel);
    }
}

const fn cmsg_align(len: usize) -> Option<usize> {
    match len.checked_add(size_of::<usize>() - 1) {
        Some(len) => Some(len & !(size_of::<usize>() - 1)),
        None => None,
    }
}

const fn cmsg_header_len() -> usize {
    // `cmsghdr` has native size/alignment on every supported Linux ABI.
    size_of::<cmsghdr>()
}

const fn cmsg_len(body_len: usize) -> Option<usize> {
    match cmsg_align(cmsg_header_len()) {
        Some(header_len) => header_len.checked_add(body_len),
        None => None,
    }
}

const fn cmsg_space(body_len: usize) -> Option<usize> {
    match (cmsg_align(cmsg_header_len()), cmsg_align(body_len)) {
        (Some(header_len), Some(body_len)) => header_len.checked_add(body_len),
        _ => None,
    }
}

fn credentials_cmsg(pid: u32, uid: u32, gid: u32) -> Option<(cmsghdr, ucred)> {
    Some((
        cmsghdr {
            cmsg_len: cmsg_len(size_of::<ucred>())?,
            cmsg_level: SOL_SOCKET as _,
            cmsg_type: SCM_CREDENTIALS as _,
        },
        ucred {
            pid: pid as _,
            uid,
            gid,
        },
    ))
}

fn try_box<T>(value: T) -> AxResult<Box<T>> {
    let raw = unsafe { alloc(Layout::new::<T>()) }.cast::<T>();
    if raw.is_null() {
        return Err(AxError::NoMemory);
    }
    unsafe {
        raw.write(value);
        Ok(Box::from_raw(raw))
    }
}

pub enum CMsg {
    Rights {
        graph: Arc<RightsGraphNode>,
        inflight: ScmRightsReservation,
    },
    /// Kernel-generated `SCM_CREDENTIALS`; this owns no inflight resource.
    Credentials { pid: u32, uid: u32, gid: u32 },
    /// Per-message SCTP send metadata.  It deliberately has no resource
    /// ownership, unlike SCM_RIGHTS, and is consumed exactly once by the
    /// sequenced-record transport.
    SctpSend {
        stream: u16,
        flags: u16,
        ppid: u32,
        context: u32,
        pr_policy: u16,
        pr_value: u32,
    },
    /// Linux `SOL_DCCP`/`DCCP_SCM_PRIORITY` metadata.  It is consumed by one
    /// DCCP record and must never be queued as generic ancillary data.
    DccpPriority(u32),
}

impl Drop for CMsg {
    fn drop(&mut self) {
        if let Self::Rights { .. } = self {
            collect_scm_rights_cycles();
        }
    }
}

impl CMsg {
    pub fn sctp_send(
        stream: u16,
        flags: u16,
        ppid: u32,
        context: u32,
        pr_policy: u16,
        pr_value: u32,
    ) -> AxResult<CMsgData> {
        CMsgData::new_peekable(
            try_box(Self::SctpSend {
                stream,
                flags,
                ppid,
                context,
                pr_policy,
                pr_value,
            })?,
            size_of::<Self>(),
            Self::clone_for_peek,
        )
    }
    pub fn dccp_priority(priority: u32) -> AxResult<CMsgData> {
        CMsgData::new_peekable(
            try_box(Self::DccpPriority(priority))?,
            size_of::<Self>(),
            Self::clone_for_peek,
        )
    }
    /// Appends one SCM_RIGHTS header to a pre-reserved aggregate list.
    /// Multiple user headers are coalesced into one queued object, matching
    /// Linux's single `scm_fp_list` and keeping metadata bounded by
    /// `SCM_MAX_FD` rather than by arbitrary header fragmentation.
    pub fn append_rights(
        capability: &UserMemoryCapability,
        hdr_addr: usize,
        hdr: &cmsghdr,
        fds: &mut Vec<ScmDescriptorCustody>,
    ) -> AxResult<()> {
        let header_len = cmsg_header_len();
        if hdr.cmsg_len < header_len {
            return Err(AxError::InvalidInput);
        }
        if (hdr.cmsg_level as u32, hdr.cmsg_type as u32) != (SOL_SOCKET, SCM_RIGHTS) {
            return Err(AxError::InvalidInput);
        }

        let data_len = hdr.cmsg_len - header_len;
        // Linux's scm_fp_copy consumes the complete fd integers and ignores a
        // final 1-3 data bytes.
        let fd_count = data_len / size_of::<i32>();
        if fds
            .len()
            .checked_add(fd_count)
            .is_none_or(|count| count > SCM_MAX_FD)
        {
            return Err(AxError::InvalidInput);
        }

        let data_addr = hdr_addr
            .checked_add(header_len)
            .ok_or(AxError::InvalidInput)?;
        let data_bytes = fd_count
            .checked_mul(size_of::<i32>())
            .ok_or(AxError::InvalidInput)?;
        let mut raw_fds = Vec::new();
        raw_fds
            .try_reserve_exact(fd_count)
            .map_err(|_| AxError::NoMemory)?;
        raw_fds.resize(fd_count, 0_i32);
        if data_bytes != 0 {
            // Never expose userspace as a Rust slice. A sibling sharing this
            // address space may mutate or unmap the control buffer while the
            // syscall runs; take one bounded owned snapshot, then parse only
            // kernel memory.
            capability
                .read_slice(data_addr as *const u8, unsafe {
                    core::slice::from_raw_parts_mut(
                        raw_fds.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                        data_bytes,
                    )
                })
                .map_err(map_usercopy_error)?;
        }
        for fd in raw_fds {
            if fd < 0 {
                return Err(AxError::BadFileDescriptor);
            }
            let description = get_file_description_for_scm(fd)?;

            fds.push(description);
        }
        Ok(())
    }

    pub fn from_rights(fds: Vec<ScmDescriptorCustody>) -> AxResult<Option<CMsgData>> {
        if fds.is_empty() {
            return Ok(None);
        }
        let charge = size_of::<Self>()
            .checked_add(
                fds.len()
                    .checked_mul(SCM_RIGHTS_FD_QUEUE_CHARGE)
                    .ok_or(AxError::NoMemory)?,
            )
            .and_then(|charge| {
                fds.capacity()
                    .checked_mul(size_of::<ScmDescriptorCustody>())
                    .and_then(|storage| charge.checked_add(storage))
            })
            .ok_or(AxError::NoMemory)?;
        let inflight = ScmRightsReservation::acquire(fds.len())?;
        let graph = register_rights_graph(fds)?;
        collect_scm_rights_cycles();
        Ok(Some(CMsgData::new_peekable(
            try_box(Self::Rights { graph, inflight })?,
            charge,
            Self::clone_for_peek,
        )?))
    }

    fn clone_for_peek(value: &Self) -> AxResult<Self> {
        match value {
            Self::Rights { graph, .. } => {
                let source = graph.fds.lock();
                let mut fds = Vec::new();
                fds.try_reserve_exact(source.len())
                    .map_err(|_| AxError::NoMemory)?;
                for fd in source.iter() {
                    fds.push(fd.description().acquire_scm_custody());
                }
                let inflight = ScmRightsReservation::acquire(fds.len())?;
                Ok(Self::Rights {
                    graph: register_rights_graph(fds)?,
                    inflight,
                })
            }
            Self::Credentials { pid, uid, gid } => Ok(Self::Credentials {
                pid: *pid,
                uid: *uid,
                gid: *gid,
            }),
            Self::SctpSend {
                stream,
                flags,
                ppid,
                context,
                pr_policy,
                pr_value,
            } => Ok(Self::SctpSend {
                stream: *stream,
                flags: *flags,
                ppid: *ppid,
                context: *context,
                pr_policy: *pr_policy,
                pr_value: *pr_value,
            }),
            Self::DccpPriority(priority) => Ok(Self::DccpPriority(*priority)),
        }
    }

    /// Builds an automatic `SCM_CREDENTIALS` control message for SO_PASSCRED.
    pub fn credentials(pid: u32, uid: u32, gid: u32) -> AxResult<CMsgData> {
        CMsgData::new_peekable(
            try_box(Self::Credentials { pid, uid, gid })?,
            size_of::<Self>(),
            Self::clone_for_peek,
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RightsPushResult {
    pub installed: usize,
    pub published: bool,
}

pub struct CMsgBuilder<'a> {
    capability: UserMemoryCapability,
    hdr: UserPtr<cmsghdr>,
    len: &'a mut usize,
    capacity: usize,
}

impl<'a> CMsgBuilder<'a> {
    pub fn new(
        capability: UserMemoryCapability,
        msg: UserPtr<cmsghdr>,
        len: &'a mut usize,
    ) -> Self {
        let capacity = *len;
        *len = 0;
        Self {
            capability,
            hdr: msg,
            len,
            capacity,
        }
    }

    /// Publishes the longest SCM_RIGHTS prefix that fits both the control
    /// buffer and the receiver's fd-number limit.
    ///
    /// Once the socket payload has been consumed, Linux treats user-control
    /// faults, fd exhaustion, and ancillary allocation failure as control
    /// truncation. Accordingly this method never propagates those failures;
    /// it either publishes a fully described non-empty cmsg or publishes
    /// nothing. The returned count tells the caller when to set MSG_CTRUNC.
    pub fn push_rights(&mut self, fds: &[ScmDescriptorCustody], cloexec: bool) -> RightsPushResult {
        let Some(remaining) = self.capacity.checked_sub(*self.len) else {
            return RightsPushResult::default();
        };
        let Some(header_len) = cmsg_align(cmsg_header_len()) else {
            return RightsPushResult::default();
        };
        let Some(body_capacity) = remaining.checked_sub(header_len) else {
            return RightsPushResult::default();
        };
        let count = fds.len().min(body_capacity / size_of::<i32>());
        if count == 0 {
            return RightsPushResult::default();
        }

        let base = self.hdr.address().as_usize();
        let Some(data_addr) = base.checked_add(header_len) else {
            return RightsPushResult::default();
        };

        // Linux copies each number before fd_install. Publish one alias at a
        // time so a fault in a later integer preserves the visible prefix,
        // while CLONE_FILES siblings cannot guess any not-yet-copied slot.
        let mut installed = 0usize;
        for (index, description) in fds[..count].iter().enumerate() {
            let Ok(Some(reservation)) = try_reserve_fd(cloexec) else {
                break;
            };
            let fd = reservation.fd();
            let Some(offset) = index.checked_mul(size_of::<i32>()) else {
                break;
            };
            let Some(dst) = data_addr.checked_add(offset) else {
                break;
            };
            if self.capability.write_value(dst as *mut i32, fd).is_err()
                || reservation
                    .publish(description.description().clone())
                    .is_err()
            {
                break;
            }
            installed += 1;
        }
        if installed == 0 {
            return RightsPushResult::default();
        }

        let body_len = installed * size_of::<i32>();
        let Some(message_len) = cmsg_len(body_len) else {
            return RightsPushResult {
                installed,
                published: false,
            };
        };
        let Some(message_space) = cmsg_space(body_len) else {
            return RightsPushResult {
                installed,
                published: false,
            };
        };
        // Linux may publish a final cmsg in CMSG_LEN bytes when the caller did
        // not provide the trailing CMSG_SPACE padding.
        let used = message_space.min(remaining);
        let Some(next) = base.checked_add(used) else {
            return RightsPushResult {
                installed,
                published: false,
            };
        };

        let hdr = cmsghdr {
            cmsg_len: message_len,
            cmsg_level: SOL_SOCKET as _,
            cmsg_type: SCM_RIGHTS as _,
        };
        // `cmsghdr` has no Rust padding on the supported ABI; the source is
        // fully initialized above, so use the audited byte-copy entry point.
        if unsafe {
            self.capability
                .write_value_unchecked(base as *mut cmsghdr, hdr)
        }
        .is_err()
        {
            // Linux has already installed the fd prefix at this point. Keep it
            // even though the control header itself could not be published;
            // msg_controllen remains zero.
            return RightsPushResult {
                installed,
                published: false,
            };
        }

        self.hdr = UserPtr::from(next);
        *self.len += used;
        RightsPushResult {
            installed,
            published: true,
        }
    }

    /// Publishes one fixed-size SCM_CREDENTIALS message when it fits.  Unlike
    /// SCM_RIGHTS, credentials allocate neither descriptors nor queued state:
    /// a short or unreadable control buffer is simply reported as MSG_CTRUNC
    /// by the caller after the payload has been consumed.
    pub fn push_credentials(&mut self, pid: u32, uid: u32, gid: u32) -> bool {
        let Some(remaining) = self.capacity.checked_sub(*self.len) else {
            return false;
        };
        let Some(message_len) = cmsg_len(size_of::<ucred>()) else {
            return false;
        };
        let Some(message_space) = cmsg_space(size_of::<ucred>()) else {
            return false;
        };
        if remaining < message_len {
            return false;
        }
        let base = self.hdr.address().as_usize();
        let Some(header_len) = cmsg_align(cmsg_header_len()) else {
            return false;
        };
        let Some(data_addr) = base.checked_add(header_len) else {
            return false;
        };
        let Some((header, credentials)) = credentials_cmsg(pid, uid, gid) else {
            return false;
        };
        debug_assert_eq!(header.cmsg_len, message_len);
        // Copy the payload before the header.  A fault leaves msg_controllen
        // at its previous value and therefore cannot advertise an incomplete
        // ancillary message.
        if unsafe {
            self.capability
                .write_value_unchecked(data_addr as *mut ucred, credentials)
        }
        .is_err()
            || unsafe {
                self.capability
                    .write_value_unchecked(base as *mut cmsghdr, header)
            }
            .is_err()
        {
            return false;
        }
        let used = message_space.min(remaining);
        let Some(next) = base.checked_add(used) else {
            return false;
        };
        self.hdr = UserPtr::from(next);
        *self.len += used;
        true
    }

    /// Publish an already initialized fixed-size ancillary payload.  SCTP
    /// receive metadata has no resource-transfer side effect, so it follows
    /// the same post-payload truncation contract as SCM_CREDENTIALS.
    pub fn push_fixed(&mut self, level: u32, kind: u32, payload: &[u8]) -> bool {
        let Some(remaining) = self.capacity.checked_sub(*self.len) else {
            return false;
        };
        let Some(message_len) = cmsg_len(payload.len()) else {
            return false;
        };
        let Some(message_space) = cmsg_space(payload.len()) else {
            return false;
        };
        if remaining < message_len {
            return false;
        }
        let base = self.hdr.address().as_usize();
        let Some(header_len) = cmsg_align(cmsg_header_len()) else {
            return false;
        };
        let Some(data_addr) = base.checked_add(header_len) else {
            return false;
        };
        if self.capability.write_bytes(data_addr, payload).is_err()
            || unsafe {
                self.capability.write_value_unchecked(
                    base as *mut cmsghdr,
                    cmsghdr {
                        cmsg_len: message_len,
                        cmsg_level: level as _,
                        cmsg_type: kind as _,
                    },
                )
            }
            .is_err()
        {
            return false;
        }
        let used = message_space.min(remaining);
        let Some(next) = base.checked_add(used) else {
            return false;
        };
        self.hdr = UserPtr::from(next);
        *self.len += used;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SCM_ACCOUNT_TEST_LOCK: spin::Mutex<()> = spin::Mutex::new(());

    #[test]
    fn cmsg_lengths_match_linux_native_alignment() {
        assert_eq!(cmsg_len(0), Some(size_of::<cmsghdr>()));
        assert_eq!(cmsg_len(size_of::<i32>()), Some(size_of::<cmsghdr>() + 4));
        assert_eq!(
            cmsg_space(size_of::<i32>()),
            cmsg_align(size_of::<cmsghdr>()).and_then(|header| header.checked_add(8))
        );
    }

    #[test]
    fn rights_limit_is_linux_scm_max_fd() {
        assert_eq!(SCM_MAX_FD, 253);
    }

    #[test]
    fn credentials_cmsg_has_linux_header_and_preserves_kernel_identity() {
        let (header, credentials) = credentials_cmsg(0, 0, 0).unwrap();
        assert_eq!(header.cmsg_len, cmsg_len(size_of::<ucred>()).unwrap());
        assert_eq!(header.cmsg_level as u32, SOL_SOCKET);
        assert_eq!(header.cmsg_type as u32, SCM_CREDENTIALS);
        assert_eq!(credentials.pid, 0);
        assert_eq!(credentials.uid, 0);
        assert_eq!(credentials.gid, 0);
    }

    #[test]
    fn inflight_rights_admission_saturates_and_drop_restores_the_ledger() {
        let _guard = SCM_ACCOUNT_TEST_LOCK.lock();
        let baseline = SCM_RIGHTS_INFLIGHT.load(Ordering::Acquire);
        let available = SCM_RIGHTS_INFLIGHT_LIMIT - baseline;
        assert!(available > 0);
        try_acquire_scm_rights(available).unwrap();
        let admitted = CMsg::Rights {
            graph: register_rights_graph(Vec::new()).unwrap(),
            inflight: ScmRightsReservation { count: available },
        };
        assert_eq!(
            try_acquire_scm_rights(1),
            Err(AxError::from(LinuxError::ENOBUFS))
        );
        drop(admitted);
        assert_eq!(SCM_RIGHTS_INFLIGHT.load(Ordering::Acquire), baseline);
    }
}
