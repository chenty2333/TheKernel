//! BPF attachment and link commands for the concrete perf-event hook.

use alloc::{sync::Arc, vec::Vec};
use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::spin::SpinNoIrq;
use linux_raw_sys::general::CAP_NET_ADMIN;
use thekernel_linux_bpf::{
    BPF_PERF_EVENT, BPF_PROG_TYPE_PERF_EVENT, BpfAttrLinkCreate, BpfAttrRawTracepointOpen,
};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::{
    bpf::{
        BpfLinkObject, read_bpf_attr, require_bpf_attr_range, write_bpf_attr_value,
        write_user_bytes,
    },
    bpf_security::{authorize_link_create, authorize_perf_link_create},
    file::{
        bpf::{
            BpfIterLink, BpfIterSource, BpfLsmLink, BpfMapFd, BpfNetworkHook, BpfNetworkLink,
            BpfPerfEventLink, BpfProgFd, BpfRawTracepointLink,
        },
        get_file_like, get_typed_file,
    },
    task::{AsThread, ns_capable},
};

const BPF_F_REPLACE: u32 = 1;
/// `prog_attach` has a distinct flag namespace from `link_update`.
const BPF_F_ALLOW_OVERRIDE: u32 = 1;
const BPF_F_ALLOW_MULTI: u32 = 2;
const BPF_F_ATTACH_REPLACE: u32 = 4;
const BPF_F_NETFILTER_IP_DEFRAG: u32 = 1;

/// Legacy attachments have no link FD, so the kernel itself is their lifetime
/// owner.  This registry holds the actual link rather than a copied program
/// or numeric target FD: close-and-reuse cannot redirect detach/query and a
/// successful detach immediately removes the packet callback.
enum LegacyNetworkTarget {
    Cgroup { identity: u64 },
    Socket { socket: Arc<crate::file::Socket> },
}

struct LegacyNetworkAttachment {
    target: LegacyNetworkTarget,
    attach_type: u32,
    attach_flags: u32,
    link: Arc<BpfNetworkLink>,
}

static LEGACY_NETWORK_ATTACHMENTS: SpinNoIrq<Vec<LegacyNetworkAttachment>> =
    SpinNoIrq::new(Vec::new());
/// Serializes hierarchy admission with publication.  Packet dispatch only
/// observes published links, while attachers must not both decide that an
/// unflagged cgroup has no child attachment and then publish incompatible
/// descendants.
static CGROUP_ATTACH_TRANSACTION: SpinNoIrq<()> = SpinNoIrq::new(());

fn same_legacy_target(left: &LegacyNetworkTarget, right: &LegacyNetworkTarget) -> bool {
    match (left, right) {
        (
            LegacyNetworkTarget::Cgroup { identity: left },
            LegacyNetworkTarget::Cgroup { identity: right },
        ) => left == right,
        (
            LegacyNetworkTarget::Socket { socket: left },
            LegacyNetworkTarget::Socket { socket: right },
        ) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

fn publish_legacy_network_attachment(
    target: LegacyNetworkTarget,
    attach_type: u32,
    attach_flags: u32,
    link: Arc<BpfNetworkLink>,
) -> AxResult<()> {
    let mut attachments = LEGACY_NETWORK_ATTACHMENTS.lock();
    attachments.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    crate::bpf::activate_network_link(&link)?;
    attachments.push(LegacyNetworkAttachment {
        target,
        attach_type,
        attach_flags,
        link,
    });
    Ok(())
}

fn legacy_program_ids(target: &LegacyNetworkTarget, attach_type: u32) -> (Vec<u32>, u32) {
    let attachments = LEGACY_NETWORK_ATTACHMENTS.lock();
    let flags = attachments
        .iter()
        .find(|entry| entry.attach_type == attach_type && same_legacy_target(&entry.target, target))
        .map_or(0, |entry| entry.attach_flags);
    let ids = attachments
        .iter()
        .filter(|entry| {
            entry.attach_type == attach_type && same_legacy_target(&entry.target, target)
        })
        .filter_map(|entry| entry.link.program_id().ok())
        .collect();
    (ids, flags)
}

/// `BPF_PROG_QUERY` observes all live packet producer links, including
/// FD-backed `BPF_LINK_CREATE` links and the legacy no-FD owners.  Legacy
/// links are also weak-published in the common producer registry, so merge
/// by object identity rather than producing duplicate program IDs.
fn queried_network_programs(
    target: &LegacyNetworkTarget,
    attach_type: u32,
) -> AxResult<(Vec<u32>, u32)> {
    let expected_cgroup_hook = match target {
        LegacyNetworkTarget::Cgroup { .. } => Some(cgroup_hook(attach_type)?),
        LegacyNetworkTarget::Socket { .. } => None,
    };
    let mut links = match target {
        LegacyNetworkTarget::Cgroup { identity } => crate::bpf::cgroup_network_links(
            *identity,
            expected_cgroup_hook.ok_or(AxError::InvalidInput)?,
        )?,
        // Socket-filter attachment uses the generic attach-type zero.  Do
        // not let a cgroup-egress query accidentally enumerate the same
        // retained socket links merely because both are network targets.
        LegacyNetworkTarget::Socket { socket }
            if attach_type == crate::bpf::prog::BPF_CGROUP_INET_INGRESS =>
        {
            crate::bpf::socket_network_links(socket)?
        }
        LegacyNetworkTarget::Socket { .. } => Vec::new(),
    };
    let attachments = LEGACY_NETWORK_ATTACHMENTS.lock();
    for entry in attachments.iter().filter(|entry| {
        entry.attach_type == attach_type && same_legacy_target(&entry.target, target)
    }) {
        if !links.iter().any(|live| Arc::ptr_eq(live, &entry.link)) {
            links.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            links.push(entry.link.clone());
        }
    }
    drop(attachments);

    let mut ids = Vec::new();
    let mut attach_flags = None;
    for link in links {
        let flags = match target {
            LegacyNetworkTarget::Cgroup { .. } => {
                let Some((_, hook, flags)) = link.cgroup_attachment() else {
                    continue;
                };
                if hook != expected_cgroup_hook.ok_or(AxError::InvalidInput)? {
                    continue;
                }
                flags
            }
            LegacyNetworkTarget::Socket { .. } => 0,
        };
        if let Some(existing) = attach_flags {
            if existing != flags {
                return Err(LinuxError::EBUSY.into());
            }
        } else {
            attach_flags = Some(flags);
        }
        if let Ok(id) = link.program_id() {
            ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            ids.push(id);
        }
    }
    Ok((ids, attach_flags.unwrap_or(0)))
}

fn detach_legacy_network(
    target: &LegacyNetworkTarget,
    attach_type: u32,
    expected_program: Option<u32>,
) -> AxResult<()> {
    let detached = {
        let mut attachments = LEGACY_NETWORK_ATTACHMENTS.lock();
        let mut detached = Vec::new();
        let mut index = 0;
        while index < attachments.len() {
            let matches_target = attachments[index].attach_type == attach_type
                && same_legacy_target(&attachments[index].target, target)
                && expected_program.map_or(true, |id| {
                    attachments[index].link.program_id().ok() == Some(id)
                });
            if matches_target {
                detached.push(attachments.swap_remove(index).link);
            } else {
                index += 1;
            }
        }
        detached
    };
    if detached.is_empty() {
        return Err(AxError::NotFound);
    }
    for link in detached {
        link.detach()?;
    }
    Ok(())
}

/// `union bpf_attr` view shared by BPF_PROG_{ATTACH,DETACH}.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct BpfAttrProgAttach {
    target_fd: u32,
    attach_bpf_fd: u32,
    attach_type: u32,
    attach_flags: u32,
    replace_bpf_fd: u32,
}

/// `union bpf_attr.link_update`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct BpfAttrLinkUpdate {
    link_fd: u32,
    new_prog_fd: u32,
    flags: u32,
    old_prog_fd: u32,
}

/// `union bpf_attr.link_detach`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct BpfAttrLinkDetach {
    link_fd: u32,
}

/// Linux's `link_create.netfilter` tail.  The common prefix is exactly
/// `BpfAttrLinkCreate`; a separate view prevents a truncated request from
/// attaching at an uninitialised protocol or hook.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct BpfAttrNetfilterLinkCreate {
    common: BpfAttrLinkCreate,
    pf: u32,
    hooknum: u32,
    priority: i32,
    netfilter_flags: u32,
}

/// `union bpf_attr.query`.  The program-ID array is output-only and is
/// copied only after validating the entire fixed request prefix.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct BpfAttrProgQuery {
    target_fd: u32,
    attach_type: u32,
    query_flags: u32,
    attach_flags: u32,
    prog_ids: u64,
    prog_cnt: u32,
    _pad: u32,
}

fn perf_program_from_fd(fd: u32) -> AxResult<alloc::sync::Arc<crate::bpf::prog::BpfProgram>> {
    let program = get_typed_file::<BpfProgFd>(fd as i32)?.prog.clone();
    if program.prog_type != BPF_PROG_TYPE_PERF_EVENT {
        return Err(AxError::InvalidInput);
    }
    Ok(program)
}

fn trace_program_from_fd(
    fd: u32,
    attach_type: u32,
) -> AxResult<alloc::sync::Arc<crate::bpf::prog::BpfProgram>> {
    use crate::bpf::prog::*;
    let program = get_typed_file::<BpfProgFd>(fd as i32)?.prog.clone();
    let valid = match attach_type {
        BPF_PERF_EVENT => matches!(
            program.prog_type,
            BPF_PROG_TYPE_PERF_EVENT
                | crate::bpf::defs::BPF_PROG_TYPE_TRACEPOINT
                | crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT
                | crate::bpf::defs::BPF_PROG_TYPE_KPROBE
                | BPF_PROG_TYPE_TRACING
        ),
        BPF_TRACE_RAW_TP => matches!(
            program.prog_type,
            crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT | BPF_PROG_TYPE_TRACING
        ),
        BPF_TRACE_FENTRY | BPF_TRACE_FEXIT | BPF_MODIFY_RETURN => {
            program.prog_type == BPF_PROG_TYPE_TRACING
        }
        BPF_LSM_MAC => program.prog_type == BPF_PROG_TYPE_LSM,
        _ => false,
    };
    if !valid || (program.expected_attach_type != 0 && program.expected_attach_type != attach_type)
    {
        return Err(AxError::InvalidInput);
    }
    Ok(program)
}

fn perf_event_from_fd(fd: u32) -> AxResult<alloc::sync::Arc<crate::file::PerfEventFile>> {
    get_typed_file::<crate::file::PerfEventFile>(fd as i32).map(|file| file.clone_object())
}

fn network_program_from_fd(
    fd: u32,
    prog_type: u32,
    attach_type: u32,
) -> AxResult<alloc::sync::Arc<crate::bpf::prog::BpfProgram>> {
    let program = get_typed_file::<BpfProgFd>(fd as i32)?.prog.clone();
    if program.prog_type != prog_type
        || (program.expected_attach_type != 0 && program.expected_attach_type != attach_type)
    {
        return Err(AxError::InvalidInput);
    }
    Ok(program)
}

fn install_network_link(link: BpfNetworkLink) -> AxResult<isize> {
    let link = alloc::sync::Arc::try_new(link).map_err(|_| AxError::NoMemory)?;
    crate::bpf::activate_network_link(&link)?;
    crate::bpf::publish_link(BpfLinkObject::Network(link), true).map(|fd| fd as isize)
}

fn install_xdp_link(link: BpfNetworkLink) -> AxResult<isize> {
    let link = alloc::sync::Arc::try_new(link).map_err(|_| AxError::NoMemory)?;
    crate::bpf::activate_xdp_link(&link)?;
    crate::bpf::publish_link(BpfLinkObject::Network(link), true).map(|fd| fd as isize)
}

fn cgroup_hook(attach_type: u32) -> AxResult<BpfNetworkHook> {
    match attach_type {
        crate::bpf::prog::BPF_CGROUP_INET_INGRESS => Ok(BpfNetworkHook::Input),
        crate::bpf::prog::BPF_CGROUP_INET_EGRESS => Ok(BpfNetworkHook::Output),
        _ => Err(AxError::InvalidInput),
    }
}

fn cgroup_policy_flags(attach_flags: u32) -> AxResult<u32> {
    let policy = attach_flags & (BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI);
    if policy == BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI {
        return Err(AxError::InvalidInput);
    }
    Ok(policy)
}

/// Checks the Linux cgroup-BPF inheritance contract before a new attachment
/// becomes visible.  A descendant may exist only below an overriding or
/// multi-attach ancestor; programs at one cgroup/hook have one shared policy.
/// The producer registry includes FD-backed links as well as legacy ones.
fn admit_cgroup_attachment(
    directory: &crate::file::Directory,
    hook: BpfNetworkHook,
    attach_flags: u32,
    replacing: bool,
) -> AxResult<()> {
    let policy = cgroup_policy_flags(attach_flags)?;
    let hierarchy = crate::pseudofs::cgroup::bpf_cgroup_hierarchy(directory)?;
    let identity = *hierarchy.last().ok_or(AxError::InvalidInput)?;
    let local = crate::bpf::cgroup_attachment_flags(identity, hook)?;
    if let Some(existing_policy) = local.first().copied() {
        if local.iter().any(|candidate| *candidate != existing_policy) || existing_policy != policy
        {
            return Err(LinuxError::EBUSY.into());
        }
        if replacing || existing_policy & BPF_F_ALLOW_MULTI != 0 {
            return Ok(());
        }
        return Err(LinuxError::EBUSY.into());
    }
    if replacing {
        return Err(AxError::NotFound);
    }

    // The closest populated ancestor controls whether this attachment can be
    // reached.  Empty cgroups do not interrupt inheritance.
    for ancestor in hierarchy[..hierarchy.len().saturating_sub(1)].iter().rev() {
        let inherited = crate::bpf::cgroup_attachment_flags(*ancestor, hook)?;
        let Some(parent_policy) = inherited.first().copied() else {
            continue;
        };
        if inherited
            .iter()
            .any(|candidate| *candidate != parent_policy)
        {
            return Err(LinuxError::EBUSY.into());
        }
        if parent_policy & (BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI) == 0 {
            return Err(LinuxError::EBUSY.into());
        }
        break;
    }
    Ok(())
}

fn netfilter_hook(hooknum: u32) -> AxResult<BpfNetworkHook> {
    match hooknum {
        0 => Ok(BpfNetworkHook::Prerouting),
        1 => Ok(BpfNetworkHook::Input),
        2 => Ok(BpfNetworkHook::Forward),
        3 => Ok(BpfNetworkHook::Output),
        4 => Ok(BpfNetworkHook::Postrouting),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn bpf_raw_tracepoint_open<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrRawTracepointOpen>(
        attr_size,
        size_of::<BpfAttrRawTracepointOpen>(),
    )?;
    let attr: BpfAttrRawTracepointOpen = read_bpf_attr(memory, attr_ptr, attr_size)?;
    if attr.name == 0 {
        return Err(AxError::BadAddress);
    }
    let program = get_typed_file::<BpfProgFd>(attr.prog_fd as i32)?
        .prog
        .clone();
    if program.prog_type != crate::bpf::defs::BPF_PROG_TYPE_RAW_TRACEPOINT
        || program.expected_attach_type != 0
    {
        return Err(AxError::InvalidInput);
    }
    let name = memory
        .load_until_nul_bounded(attr.name as *const u8, 128)
        .map_err(crate::mm::map_usercopy_error)?;
    if name.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let tracepoint = crate::perf_sources::raw_tracepoint(&name).ok_or(AxError::NotFound)?;
    let available = u32::from(tracepoint.raw_arg_count)
        .checked_mul(8)
        .ok_or(AxError::InvalidInput)?;
    if program.mechanism.required_context_bytes() > available {
        return Err(AxError::InvalidInput);
    }
    let link = Arc::try_new(BpfRawTracepointLink::new(
        program,
        tracepoint.id,
        tracepoint.name,
        attr.cookie,
    ))
    .map_err(|_| AxError::NoMemory)?;
    crate::bpf::activate_raw_tracepoint_link(&link)?;
    crate::bpf::publish_link(BpfLinkObject::RawTracepoint(link), true).map(|fd| fd as isize)
}

pub fn bpf_link_create<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrLinkCreate>(
        attr_size,
        offset_of!(BpfAttrLinkCreate, flags) + size_of::<u32>(),
    )?;
    let attr: BpfAttrLinkCreate = read_bpf_attr(memory, attr_ptr, attr_size)?;
    authorize_link_create()?;
    if attr.bpf_cookie != 0 {
        return Err(AxError::InvalidInput);
    }
    if attr.attach_type == crate::bpf::prog::BPF_TRACE_ITER {
        let program = get_typed_file::<BpfProgFd>(attr.prog_fd as i32)?
            .prog
            .clone();
        if program.prog_type != crate::bpf::prog::BPF_PROG_TYPE_TRACING
            || program.expected_attach_type != crate::bpf::prog::BPF_TRACE_ITER
        {
            return Err(AxError::InvalidInput);
        }
        // The current mount namespace is the target when target_fd is zero.
        // A non-zero map/socket/perf FD is retained as the concrete traversal
        // object, never revisited by numeric descriptor after publication.
        let source = if attr.target_fd == 0 {
            BpfIterSource::Mounts
        } else if let Ok(map) = get_typed_file::<BpfMapFd>(attr.target_fd as i32) {
            BpfIterSource::Map {
                map: map.map.clone(),
                id: map.map_id,
            }
        } else {
            BpfIterSource::Object(get_file_like(attr.target_fd as i32)?.clone_object())
        };
        let link =
            Arc::try_new(BpfIterLink::new(program, source)).map_err(|_| AxError::NoMemory)?;
        return crate::bpf::publish_link(BpfLinkObject::Iter(link), true).map(|fd| fd as isize);
    }
    if matches!(
        attr.attach_type,
        crate::bpf::prog::BPF_CGROUP_INET_INGRESS | crate::bpf::prog::BPF_CGROUP_INET_EGRESS
    ) {
        if attr.flags & !(BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI) != 0 {
            return Err(AxError::InvalidInput);
        }
        let attach_flags = cgroup_policy_flags(attr.flags)?;
        if attr.target_fd == 0 {
            return Err(AxError::InvalidInput);
        }
        let current = axtask::current();
        let thread = current.as_thread();
        if !ns_capable(
            &thread.current_cred(),
            thread.cgroup_ns().owner_user_ns(),
            CAP_NET_ADMIN,
        ) {
            return Err(AxError::OperationNotPermitted);
        }
        let program = network_program_from_fd(
            attr.prog_fd,
            crate::bpf::defs::BPF_PROG_TYPE_CGROUP_SKB,
            attr.attach_type,
        )?;
        let (identity, directory) =
            crate::pseudofs::cgroup::bpf_cgroup_fd_target(attr.target_fd as i32)?;
        let _transaction = CGROUP_ATTACH_TRANSACTION.lock();
        let hook = cgroup_hook(attr.attach_type)?;
        admit_cgroup_attachment(&directory, hook, attach_flags, false)?;
        return install_network_link(BpfNetworkLink::cgroup(
            directory,
            identity,
            hook,
            attach_flags,
            program,
        ));
    }
    if attr.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    if attr.attach_type == crate::bpf::prog::BPF_NETFILTER {
        require_bpf_attr_range::<BpfAttrNetfilterLinkCreate>(
            attr_size,
            size_of::<BpfAttrNetfilterLinkCreate>(),
        )?;
        let netfilter: BpfAttrNetfilterLinkCreate = read_bpf_attr(memory, attr_ptr, attr_size)?;
        if !matches!(netfilter.pf, 2 | 10) {
            return Err(LinuxError::EAFNOSUPPORT.into());
        }
        if netfilter.netfilter_flags & !BPF_F_NETFILTER_IP_DEFRAG != 0 {
            return Err(AxError::InvalidInput);
        }
        // `IP_DEFRAG` is retained on this link. The AX packet producer asks
        // the live link set before PREROUTING, instead of globally changing
        // every namespace packet merely because one link opted in.
        let program = network_program_from_fd(
            attr.prog_fd,
            crate::bpf::defs::BPF_PROG_TYPE_NETFILTER,
            attr.attach_type,
        )?;
        let namespace = if attr.target_fd == 0 {
            axtask::current().as_thread().net_ns()
        } else {
            crate::pseudofs::bpf_network_namespace_from_fd(attr.target_fd as i32)?
        };
        if !ns_capable(
            &axtask::current().as_thread().current_cred(),
            namespace.owner_user_ns(),
            CAP_NET_ADMIN,
        ) {
            return Err(AxError::OperationNotPermitted);
        }
        return install_network_link(BpfNetworkLink::netfilter(
            namespace,
            netfilter_hook(netfilter.hooknum)?,
            netfilter.pf,
            netfilter.priority,
            netfilter.netfilter_flags & BPF_F_NETFILTER_IP_DEFRAG != 0,
            program,
        ));
    }
    if attr.attach_type == crate::bpf::prog::BPF_XDP {
        // In the XDP arm of Linux's `union bpf_attr`, this common-prefix
        // field is `target_ifindex` (not an fd); the target network namespace
        // is the caller's current namespace.  The v6.18 prefix contains all
        // target identity required for a link-create request.
        if attr.flags != 0 || attr.target_fd == 0 {
            return Err(AxError::InvalidInput);
        }
        let namespace = axtask::current().as_thread().net_ns();
        if !ns_capable(
            &axtask::current().as_thread().current_cred(),
            namespace.owner_user_ns(),
            CAP_NET_ADMIN,
        ) {
            return Err(AxError::OperationNotPermitted);
        }
        if !namespace
            .stack()
            .interfaces()
            .iter()
            .any(|interface| interface.index == attr.target_fd)
        {
            return Err(AxError::NoSuchDevice);
        }
        let program = network_program_from_fd(
            attr.prog_fd,
            crate::bpf::defs::BPF_PROG_TYPE_XDP,
            attr.attach_type,
        )?;
        return install_xdp_link(BpfNetworkLink::xdp(namespace, attr.target_fd, program)?);
    }
    if attr.attach_type == crate::bpf::prog::BPF_LSM_MAC {
        if attr.target_fd != 0 {
            return Err(AxError::InvalidInput);
        }
        let link = alloc::sync::Arc::new(BpfLsmLink::new(trace_program_from_fd(
            attr.prog_fd,
            attr.attach_type,
        )?));
        crate::bpf::register_lsm_link(&link)?;
        return crate::bpf::publish_link(BpfLinkObject::Lsm(link), true).map(|fd| fd as isize);
    }
    authorize_perf_link_create()?;
    let program = trace_program_from_fd(attr.prog_fd, attr.attach_type)?;
    let event = perf_event_from_fd(attr.target_fd)?;
    let link = Arc::try_new(BpfPerfEventLink::new(
        event.clone(),
        program.clone(),
        0,
        attr.attach_type,
    ))
    .map_err(|_| AxError::NoMemory)?;
    let generation = event.attach_bpf_link(program)?;
    link.set_initial_generation(generation);
    crate::bpf::publish_link(BpfLinkObject::PerfEvent(link), true).map(|fd| fd as isize)
}

/// Legacy attachment remains useful to perf tooling.  It uses the same real
/// event hook as BPF_LINK_CREATE but has no link descriptor; unlike a link it
/// may be detached only by a matching BPF_PROG_DETACH request.
pub fn bpf_prog_attach<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrProgAttach>(
        attr_size,
        offset_of!(BpfAttrProgAttach, attach_flags) + size_of::<u32>(),
    )?;
    let attr: BpfAttrProgAttach = read_bpf_attr(memory, attr_ptr, attr_size)?;
    authorize_link_create()?;
    if attr.attach_type == BPF_PERF_EVENT {
        authorize_perf_link_create()?;
        if attr.attach_flags != 0 || attr.replace_bpf_fd != 0 {
            return Err(AxError::InvalidInput);
        }
        let program = perf_program_from_fd(attr.attach_bpf_fd)?;
        let event = perf_event_from_fd(attr.target_fd)?;
        event.attach_legacy_bpf_program(program)?;
        return Ok(0);
    }
    if attr.attach_type == crate::bpf::prog::BPF_NETFILTER {
        // Unlike LINK_CREATE, legacy `prog_attach` has no pf/hook/priority
        // tail.  There is no safe default attachment point to infer here.
        return Err(AxError::InvalidInput);
    }
    if !matches!(
        attr.attach_type,
        crate::bpf::prog::BPF_CGROUP_INET_INGRESS | crate::bpf::prog::BPF_CGROUP_INET_EGRESS
    ) {
        return Err(AxError::InvalidInput);
    }
    if attr.attach_flags & !(BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI | BPF_F_ATTACH_REPLACE) != 0 {
        return Err(AxError::InvalidInput);
    }
    cgroup_policy_flags(attr.attach_flags)?;
    let replace = attr.attach_flags & BPF_F_ATTACH_REPLACE != 0;
    if replace != (attr.replace_bpf_fd != 0) {
        return Err(AxError::InvalidInput);
    }

    // A socket-filter has attach type zero in the generic ABI, which happens
    // to share the cgroup-ingress numeric value.  The retained target's real
    // OFD type disambiguates it; do not manufacture a cgroup alias.
    if let Ok(socket) = get_typed_file::<crate::file::Socket>(attr.target_fd as i32) {
        if attr.attach_flags != 0 {
            return Err(AxError::InvalidInput);
        }
        let program = network_program_from_fd(
            attr.attach_bpf_fd,
            crate::bpf::defs::BPF_PROG_TYPE_SOCKET_FILTER,
            attr.attach_type,
        )?;
        let target = LegacyNetworkTarget::Socket {
            socket: socket.clone_object(),
        };
        let (ids, _) = legacy_program_ids(&target, attr.attach_type);
        if !ids.is_empty() {
            return Err(AxError::AlreadyExists);
        }
        let link = Arc::try_new(BpfNetworkLink::socket(socket.clone_object(), program)?)
            .map_err(|_| AxError::NoMemory)?;
        link.activate_socket_filter()?;
        if let Err(error) =
            publish_legacy_network_attachment(target, attr.attach_type, 0, link.clone())
        {
            let _ = link.detach();
            return Err(error);
        }
        return Ok(0);
    }

    let current = axtask::current();
    let thread = current.as_thread();
    if !ns_capable(
        &thread.current_cred(),
        thread.cgroup_ns().owner_user_ns(),
        CAP_NET_ADMIN,
    ) {
        return Err(AxError::OperationNotPermitted);
    }
    let program = network_program_from_fd(
        attr.attach_bpf_fd,
        crate::bpf::defs::BPF_PROG_TYPE_CGROUP_SKB,
        attr.attach_type,
    )?;
    let (identity, directory) =
        crate::pseudofs::cgroup::bpf_cgroup_fd_target(attr.target_fd as i32)?;
    let target = LegacyNetworkTarget::Cgroup { identity };
    let _transaction = CGROUP_ATTACH_TRANSACTION.lock();
    let (ids, _) = legacy_program_ids(&target, attr.attach_type);
    if replace {
        let expected = get_typed_file::<BpfProgFd>(attr.replace_bpf_fd as i32)?
            .prog
            .prog_id;
        let attachments = LEGACY_NETWORK_ATTACHMENTS.lock();
        let entry = attachments
            .iter()
            .find(|entry| {
                entry.attach_type == attr.attach_type
                    && same_legacy_target(&entry.target, &target)
                    && entry.link.program_id().ok() == Some(expected)
            })
            .ok_or(AxError::NotFound)?;
        let hook = cgroup_hook(attr.attach_type)?;
        admit_cgroup_attachment(&directory, hook, attr.attach_flags, true)?;
        entry.link.update(program, Some(expected))?;
        return Ok(0);
    }
    let hook = cgroup_hook(attr.attach_type)?;
    admit_cgroup_attachment(&directory, hook, attr.attach_flags, false)?;
    if !ids.is_empty() && attr.attach_flags & BPF_F_ALLOW_MULTI == 0 {
        return Err(LinuxError::EBUSY.into());
    }
    let link = Arc::try_new(BpfNetworkLink::cgroup(
        directory,
        identity,
        hook,
        cgroup_policy_flags(attr.attach_flags)?,
        program,
    ))
    .map_err(|_| AxError::NoMemory)?;
    publish_legacy_network_attachment(target, attr.attach_type, attr.attach_flags, link)?;
    Ok(0)
}

pub fn bpf_prog_detach<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrProgAttach>(
        attr_size,
        offset_of!(BpfAttrProgAttach, attach_flags) + size_of::<u32>(),
    )?;
    let attr: BpfAttrProgAttach = read_bpf_attr(memory, attr_ptr, attr_size)?;
    authorize_link_create()?;
    if attr.attach_type == BPF_PERF_EVENT {
        authorize_perf_link_create()?;
        if attr.attach_flags != 0 || attr.replace_bpf_fd != 0 {
            return Err(AxError::InvalidInput);
        }
        let event = perf_event_from_fd(attr.target_fd)?;
        let expected = (attr.attach_bpf_fd != 0)
            .then(|| perf_program_from_fd(attr.attach_bpf_fd))
            .transpose()?
            .map(|program| program.prog_id);
        event.detach_legacy_bpf_program(expected)?;
        return Ok(0);
    }
    if !matches!(
        attr.attach_type,
        crate::bpf::prog::BPF_CGROUP_INET_INGRESS | crate::bpf::prog::BPF_CGROUP_INET_EGRESS
    ) {
        return Err(AxError::InvalidInput);
    }
    if attr.attach_flags != 0 || attr.replace_bpf_fd != 0 {
        return Err(AxError::InvalidInput);
    }
    let target = if let Ok(socket) = get_typed_file::<crate::file::Socket>(attr.target_fd as i32) {
        LegacyNetworkTarget::Socket {
            socket: socket.clone_object(),
        }
    } else {
        let current = axtask::current();
        let thread = current.as_thread();
        if !ns_capable(
            &thread.current_cred(),
            thread.cgroup_ns().owner_user_ns(),
            CAP_NET_ADMIN,
        ) {
            return Err(AxError::OperationNotPermitted);
        }
        let (identity, _) = crate::pseudofs::cgroup::bpf_cgroup_fd_target(attr.target_fd as i32)?;
        LegacyNetworkTarget::Cgroup { identity }
    };
    let expected = (attr.attach_bpf_fd != 0)
        .then(|| get_typed_file::<BpfProgFd>(attr.attach_bpf_fd as i32))
        .transpose()?
        .map(|program| program.prog.prog_id);
    detach_legacy_network(&target, attr.attach_type, expected)?;
    Ok(0)
}

pub fn bpf_prog_query<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrProgQuery>(attr_size, size_of::<BpfAttrProgQuery>())?;
    let attr: BpfAttrProgQuery = read_bpf_attr(memory, attr_ptr, attr_size)?;
    authorize_link_create()?;
    if attr.query_flags != 0 || attr.attach_flags != 0 || attr._pad != 0 {
        return Err(AxError::InvalidInput);
    }
    let (ids, attach_flags) = if attr.attach_type == BPF_PERF_EVENT {
        authorize_perf_link_create()?;
        let event = perf_event_from_fd(attr.target_fd)?;
        (event.bpf_program_id().into_iter().collect::<Vec<_>>(), 0)
    } else if matches!(
        attr.attach_type,
        crate::bpf::prog::BPF_CGROUP_INET_INGRESS | crate::bpf::prog::BPF_CGROUP_INET_EGRESS
    ) {
        let target =
            if let Ok(socket) = get_typed_file::<crate::file::Socket>(attr.target_fd as i32) {
                LegacyNetworkTarget::Socket {
                    socket: socket.clone_object(),
                }
            } else {
                let current = axtask::current();
                let thread = current.as_thread();
                if !ns_capable(
                    &thread.current_cred(),
                    thread.cgroup_ns().owner_user_ns(),
                    CAP_NET_ADMIN,
                ) {
                    return Err(AxError::OperationNotPermitted);
                }
                let (identity, _) =
                    crate::pseudofs::cgroup::bpf_cgroup_fd_target(attr.target_fd as i32)?;
                LegacyNetworkTarget::Cgroup { identity }
            };
        queried_network_programs(&target, attr.attach_type)?
    } else {
        return Err(AxError::InvalidInput);
    };
    let available = ids.len() as u32;
    if attr.prog_cnt != 0 && attr.prog_ids == 0 {
        return Err(AxError::BadAddress);
    }
    let copy_count = core::cmp::min(attr.prog_cnt, available) as usize;
    if copy_count != 0 {
        write_user_bytes(
            memory,
            attr.prog_ids as usize,
            &bytemuck::cast_slice(&ids)[..copy_count * size_of::<u32>()],
        )?;
    }
    write_bpf_attr_value::<BpfAttrProgQuery, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrProgQuery, prog_cnt),
        &available,
    )?;
    write_bpf_attr_value::<BpfAttrProgQuery, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrProgQuery, attach_flags),
        &attach_flags,
    )?;
    if attr.prog_cnt < available {
        return Err(LinuxError::ENOSPC.into());
    }
    Ok(0)
}

pub fn bpf_link_update<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrLinkUpdate>(attr_size, size_of::<BpfAttrLinkUpdate>())?;
    let attr: BpfAttrLinkUpdate = read_bpf_attr(memory, attr_ptr, attr_size)?;
    authorize_link_create()?;
    if attr.flags & !BPF_F_REPLACE != 0
        || (attr.flags == 0 && attr.old_prog_fd != 0)
        || (attr.flags == BPF_F_REPLACE && attr.old_prog_fd == 0)
    {
        return Err(AxError::InvalidInput);
    }
    let expected = if attr.flags == BPF_F_REPLACE {
        Some(
            get_typed_file::<BpfProgFd>(attr.old_prog_fd as i32)?
                .prog
                .prog_id,
        )
    } else {
        None
    };
    if let Ok(link) = get_typed_file::<BpfPerfEventLink>(attr.link_fd as i32) {
        authorize_perf_link_create()?;
        let program = trace_program_from_fd(attr.new_prog_fd, link.attach_type()?)?;
        link.update(program, expected)?;
    } else if let Ok(link) = get_typed_file::<BpfLsmLink>(attr.link_fd as i32) {
        let program = trace_program_from_fd(attr.new_prog_fd, crate::bpf::prog::BPF_LSM_MAC)?;
        link.update(program, expected)?;
    } else if let Ok(link) = get_typed_file::<BpfNetworkLink>(attr.link_fd as i32) {
        link.update(
            get_typed_file::<BpfProgFd>(attr.new_prog_fd as i32)?
                .prog
                .clone(),
            expected,
        )?;
    } else {
        return Err(AxError::InvalidInput);
    }
    Ok(0)
}

pub fn bpf_link_detach<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrLinkDetach>(attr_size, size_of::<BpfAttrLinkDetach>())?;
    let attr: BpfAttrLinkDetach = read_bpf_attr(memory, attr_ptr, attr_size)?;
    authorize_link_create()?;
    if let Ok(link) = get_typed_file::<BpfPerfEventLink>(attr.link_fd as i32) {
        link.detach()?;
    } else if let Ok(link) = get_typed_file::<BpfLsmLink>(attr.link_fd as i32) {
        link.detach()?;
    } else if let Ok(link) = get_typed_file::<BpfNetworkLink>(attr.link_fd as i32) {
        link.detach()?;
    } else if let Ok(link) = get_typed_file::<BpfRawTracepointLink>(attr.link_fd as i32) {
        link.detach()?;
    } else {
        get_typed_file::<BpfIterLink>(attr.link_fd as i32)?.detach()?;
    }
    Ok(0)
}
