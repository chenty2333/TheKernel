use alloc::sync::Arc;
use core::{ffi::c_char, mem};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::FS_CONTEXT;
use axhal::paging::MappingFlags;
use axtask::{AxTaskRef, current};
use linux_raw_sys::{
    general::{
        __user_cap_data_struct, __user_cap_header_struct, _LINUX_CAPABILITY_VERSION_1,
        _LINUX_CAPABILITY_VERSION_2, _LINUX_CAPABILITY_VERSION_3, CAP_SYS_ADMIN, CAP_SYS_NICE,
        CLONE_FILES, CLONE_FS, CLONE_NEWCGROUP, CLONE_NEWIPC, CLONE_NEWNET, CLONE_NEWNS,
        CLONE_NEWPID, CLONE_NEWTIME, CLONE_NEWUSER, CLONE_NEWUTS, CLONE_SYSVSEM,
    },
    mempolicy::*,
};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use starry_vm::{VmMutPtr, VmPtr, vm_write_slice};
use thekernel_linux_cred::{
    CAPABILITY_VALID_MASK, CAPABILITY_WORDS, CapabilitySets, CapsetRequest,
};

use crate::{
    file::{FD_TABLE, File, FileDescription, FileLike, replace_process_fd_table},
    mm::vm_load_string,
    pseudofs::{
        ProcNamespaceKind, ProcNamespaceObject, ProcNamespaceTarget,
        namespace_target_from_proc_file,
    },
    task::{
        AsThread, Cred, Dumpability, Mempolicy, ProcessData, PtraceAccessMode,
        check_current_process_ptrace_access, cred_error, get_process_data, get_visible_task,
        linux_pid_from_task_id, ns_capable,
    },
};

const NO_ID_CHANGE: u32 = u32::MAX;
const ALLOWED_NODEMASK: usize = 0b1;
const DEFAULT_NUMA_NODE: i32 = 0;
const SUPPORTED_GET_MEMPOLICY_FLAGS: usize =
    (MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED) as usize;
const SUPPORTED_MODE_FLAGS: usize = MPOL_MODE_FLAGS as usize;
const SUPPORTED_MBIND_FLAGS: usize = MPOL_MF_VALID as usize;
const SUPPORTED_MOVE_PAGES_FLAGS: usize = (MPOL_MF_MOVE | MPOL_MF_MOVE_ALL) as usize;
const MAX_NODEMASK_BITS: usize = 4096;
const KCMP_FILE: i32 = 0;
const KCMP_VM: i32 = 1;
const KCMP_FILES: i32 = 2;
const KCMP_FS: i32 = 3;
const KCMP_SIGHAND: i32 = 4;
const KCMP_IO: i32 = 5;
const KCMP_SYSVSEM: i32 = 6;
const KCMP_EPOLL_TFD: i32 = 7;
const UNSHARE_SUPPORTED_FLAGS: u32 = CLONE_FILES | CLONE_FS | CLONE_NEWUTS | CLONE_NEWTIME;
const UNSHARE_RECOGNIZED_FLAGS: u32 = UNSHARE_SUPPORTED_FLAGS
    | CLONE_NEWNS
    | CLONE_NEWIPC
    | CLONE_NEWNET
    | CLONE_NEWPID
    | CLONE_NEWUSER
    | CLONE_NEWCGROUP
    | CLONE_NEWTIME
    | CLONE_SYSVSEM;
fn mempolicy_mode(mode_with_flags: usize) -> u32 {
    (mode_with_flags & !SUPPORTED_MODE_FLAGS) as u32
}

fn unshare_namespace_owner(flags: u32, actor: &Cred) -> AxResult<Arc<crate::task::UserNamespace>> {
    let owner = actor.user_ns().clone();
    if flags & (CLONE_NEWUTS | CLONE_NEWTIME) != 0 && !ns_capable(actor, &owner, CAP_SYS_ADMIN) {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(owner)
}

fn mempolicy_mode_flags(mode_with_flags: usize) -> usize {
    mode_with_flags & SUPPORTED_MODE_FLAGS
}

fn read_nodemask(nodemask: *const usize, maxnode: usize) -> AxResult<usize> {
    if nodemask.is_null() || maxnode == 0 {
        return Ok(0);
    }
    if maxnode > MAX_NODEMASK_BITS {
        return Err(AxError::InvalidInput);
    }

    let words = maxnode.div_ceil(usize::BITS as usize);
    let mut result = 0usize;
    for index in 0..words {
        let word = nodemask.wrapping_add(index).vm_read()?;
        if index == 0 {
            result = word;
        } else if word != 0 {
            return Err(AxError::InvalidInput);
        }
    }

    if maxnode < usize::BITS as usize {
        let mask = (1usize << maxnode) - 1;
        if result & !mask != 0 {
            return Err(AxError::InvalidInput);
        }
        result &= mask;
    }

    Ok(result)
}

fn write_nodemask(nodemask: *mut usize, maxnode: usize, value: usize) -> AxResult<()> {
    if nodemask.is_null() || maxnode == 0 {
        return Ok(());
    }

    let words = maxnode.div_ceil(usize::BITS as usize);
    for index in 0..words {
        let word = if index == 0 { value } else { 0 };
        nodemask.wrapping_add(index).vm_write(word)?;
    }
    Ok(())
}

fn validate_mempolicy(
    mode_with_flags: usize,
    nodemask: usize,
    allowed_nodemask: usize,
) -> AxResult<Mempolicy> {
    let mode = mempolicy_mode(mode_with_flags);
    if mempolicy_mode_flags(mode_with_flags) & MPOL_F_NUMA_BALANCING as usize != 0
        && mode != MPOL_BIND as u32
    {
        return Err(AxError::InvalidInput);
    }
    let allowed_nodemask = allowed_nodemask & ALLOWED_NODEMASK;
    let effective_nodemask = nodemask & allowed_nodemask;

    let needs_nodes = matches!(mode, mode if mode == MPOL_BIND as u32 || mode == MPOL_INTERLEAVE as u32 || mode == MPOL_PREFERRED_MANY as u32);
    if needs_nodes && effective_nodemask == 0 {
        return Err(AxError::InvalidInput);
    }

    match mode {
        mode if mode == MPOL_DEFAULT as u32 => {
            if nodemask != 0 {
                return Err(AxError::InvalidInput);
            }
            Ok(Mempolicy::new(mode, 0))
        }
        mode if mode == MPOL_PREFERRED as u32 => {
            if nodemask != 0 && effective_nodemask == 0 {
                return Err(AxError::InvalidInput);
            }
            Ok(Mempolicy::new(mode, effective_nodemask))
        }
        mode if mode == MPOL_BIND as u32
            || mode == MPOL_INTERLEAVE as u32
            || mode == MPOL_LOCAL as u32
            || mode == MPOL_PREFERRED_MANY as u32 =>
        {
            if mode == MPOL_LOCAL as u32 && nodemask != 0 {
                return Err(AxError::InvalidInput);
            }
            let stored_nodemask = if mode == MPOL_LOCAL as u32 {
                0
            } else {
                effective_nodemask
            };
            Ok(Mempolicy::new(mode, stored_nodemask))
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn current_allowed_nodemask() -> usize {
    ALLOWED_NODEMASK
}

fn validate_mapped_user_range(start: usize, size: usize) -> AxResult<(VirtAddr, usize)> {
    let start = VirtAddr::from(start);
    if size == 0 {
        return Ok((start, 0));
    }
    let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
    let aligned_start = start.align_down_4k();
    let aligned_end = VirtAddr::from(
        crate::mm::checked_align_up_4k(end.as_usize()).ok_or(AxError::InvalidInput)?,
    );
    let aligned_size = aligned_end.sub_addr(aligned_start);
    let curr = current();
    let aspace_handle = curr.as_thread().proc_data.aspace();
    let aspace = aspace_handle.lock();
    if !aspace.can_access_range(aligned_start, aligned_size, MappingFlags::USER) {
        return Err(AxError::BadAddress);
    }
    Ok((aligned_start, aligned_size))
}

fn current_has_capability(cap: u32) -> bool {
    current().as_thread().has_effective_capability(cap)
}

fn numa_target_process(pid: i32) -> AxResult<Arc<ProcessData>> {
    if pid < 0 {
        return Err(AxError::NoSuchProcess);
    }
    get_process_data(pid as u32)
}

fn check_numa_target_permission(target: &ProcessData) -> AxResult<()> {
    let curr = current();
    let actor = curr.as_thread();
    let actor_cred = actor.current_cred();
    if actor_cred.has_effective_capability(CAP_SYS_NICE) {
        return Ok(());
    }
    let actor_ids = actor_cred.ids();
    // NUMA process operations name a process, so Linux's persistent group
    // leader binding is the explicit credential subject.
    let target_cred = target.group_leader_cred();
    let target_ids = target_cred.ids();
    if actor.proc_data.proc.pid() == target.proc.pid()
        || actor_ids.euid == target_ids.ruid
        || actor_ids.euid == target_ids.euid
    {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn kcmp_target_process(pid: i32) -> AxResult<Arc<ProcessData>> {
    if pid <= 0 {
        return Err(AxError::NoSuchProcess);
    }
    get_process_data(pid as u32)
}

fn kcmp_result(equal: bool) -> isize {
    if equal { 0 } else { 1 }
}

struct KcmpAuthorizedImage<T> {
    pinned: T,
}

impl<T> KcmpAuthorizedImage<T> {
    fn new(pinned: T) -> Self {
        Self { pinned }
    }

    fn pinned(&self) -> &T {
        &self.pinned
    }
}

fn validate_kcmp_fd_image<T>(
    authorized: &KcmpAuthorizedImage<T>,
    exec_in_progress: bool,
    image_matches: impl FnOnce(&T) -> bool,
) -> AxResult<()> {
    if exec_in_progress || !image_matches(authorized.pinned()) {
        Err(AxError::OperationNotPermitted)
    } else {
        Ok(())
    }
}

fn kcmp_file_description(proc_data: &ProcessData, fd: usize) -> AxResult<Arc<FileDescription>> {
    FD_TABLE
        .scope(&proc_data.scope.read())
        .get_description_number(u32::try_from(fd).map_err(|_| AxError::BadFileDescriptor)?)
}

pub fn sys_kcmp(pid1: i32, pid2: i32, type_: i32, idx1: usize, idx2: usize) -> AxResult<isize> {
    debug!("sys_kcmp <= pid1: {pid1}, pid2: {pid2}, type: {type_}, idx1: {idx1}, idx2: {idx2}");

    let proc1 = kcmp_target_process(pid1)?;
    let proc2 = kcmp_target_process(pid2)?;
    let image1 = KcmpAuthorizedImage::new(
        check_current_process_ptrace_access(&proc1, PtraceAccessMode::ReadReal)?.into_aspace(),
    );
    let image2 = KcmpAuthorizedImage::new(
        check_current_process_ptrace_access(&proc2, PtraceAccessMode::ReadReal)?.into_aspace(),
    );

    match type_ {
        KCMP_FILE => {
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            let file1 = kcmp_file_description(&proc1, idx1);
            let file2 = kcmp_file_description(&proc2, idx2);
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            let file1 = file1?;
            let file2 = file2?;
            Ok(kcmp_result(Arc::ptr_eq(&file1, &file2)))
        }
        KCMP_VM => Ok(kcmp_result(Arc::ptr_eq(image1.pinned(), image2.pinned()))),
        KCMP_FILES => {
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            let files1 = Arc::clone(&*FD_TABLE.scope(&proc1.scope.read()));
            let files2 = Arc::clone(&*FD_TABLE.scope(&proc2.scope.read()));
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            Ok(kcmp_result(Arc::ptr_eq(&files1, &files2)))
        }
        KCMP_FS => {
            let scope1 = proc1.scope.read();
            let scope2 = proc2.scope.read();
            Ok(kcmp_result(Arc::ptr_eq(
                &*FS_CONTEXT.scope(&scope1),
                &*FS_CONTEXT.scope(&scope2),
            )))
        }
        KCMP_SIGHAND => Ok(kcmp_result(Arc::ptr_eq(
            &proc1.signal.actions,
            &proc2.signal.actions,
        ))),
        KCMP_IO | KCMP_SYSVSEM => Err(LinuxError::EOPNOTSUPP.into()),
        KCMP_EPOLL_TFD => Err(LinuxError::EOPNOTSUPP.into()),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_unshare(flags: u32) -> AxResult<isize> {
    debug!("sys_unshare <= flags: {flags:#x}");

    if flags & !UNSHARE_RECOGNIZED_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & !UNSHARE_SUPPORTED_FLAGS != 0 {
        return Err(AxError::OperationNotSupported);
    }
    let curr = current();
    let thread = curr.as_thread();
    let actor_cred = thread.current_cred();
    let namespace_owner = unshare_namespace_owner(flags, &actor_cred)?;
    if flags & UNSHARE_SUPPORTED_FLAGS != 0 {
        // Every currently supported resource still lives in ProcessData or its
        // process-wide Scope. Replacing one while siblings exist would unshare
        // it for the whole thread group instead of only for the caller.
        // Atomically gate CLONE_THREAD against the single-thread test, then
        // prepare every fallible replacement before committing any of them.
        let curr_tid = linux_pid_from_task_id(curr.id().as_u64())?;
        if !thread.proc_data.begin_single_thread_scope_change(curr_tid) {
            return Err(AxError::OperationNotSupported);
        }

        let result = (|| -> AxResult<()> {
            let private_fd_table = if flags & CLONE_FILES != 0 && Arc::strong_count(&*FD_TABLE) > 1
            {
                Some(Arc::try_new(FD_TABLE.fork_copy()?).map_err(|_| AxError::NoMemory)?)
            } else {
                None
            };
            let private_fs_context = if flags & CLONE_FS != 0 && Arc::strong_count(&*FS_CONTEXT) > 1
            {
                let cloned = FS_CONTEXT.lock().clone();
                Some(Arc::try_new(axsync::Mutex::new(cloned)).map_err(|_| AxError::NoMemory)?)
            } else {
                None
            };
            let private_uts_ns = if flags & CLONE_NEWUTS != 0 {
                Some(
                    thread
                        .proc_data
                        .uts_ns()
                        .try_fork(namespace_owner.clone())?,
                )
            } else {
                None
            };
            let private_time_ns = if flags & CLONE_NEWTIME != 0 {
                Some(
                    thread
                        .proc_data
                        .try_unshared_time_ns(namespace_owner.clone())?,
                )
            } else {
                None
            };

            let (old_fd_table, old_fs_context) = thread.with_mut_scope(|scope| {
                let old_fd_table = private_fd_table
                    .map(|replacement| replace_process_fd_table(scope, replacement));
                let old_fs_context = private_fs_context.map(|replacement| {
                    mem::replace(&mut *FS_CONTEXT.scope_mut(scope), replacement)
                });
                (old_fd_table, old_fs_context)
            });
            // Arc destructors can cascade into filesystem or file-description
            // cleanup. Keep all such work outside the IRQ/preempt-off scope gate.
            drop(old_fd_table);
            drop(old_fs_context);
            if let Some(uts_ns) = private_uts_ns {
                thread.proc_data.replace_uts_ns(uts_ns);
            }
            if let Some(time_ns) = private_time_ns {
                thread.proc_data.replace_time_ns_for_children(time_ns);
            }
            Ok(())
        })();
        thread.proc_data.end_exec(curr_tid);
        result?;
    }

    Ok(0)
}

pub fn sys_setns(fd: i32, nstype: u32) -> AxResult<isize> {
    debug!("sys_setns <= fd: {fd}, nstype: {nstype:#x}");

    let file = File::from_fd(fd)?;
    let target = match namespace_target_from_proc_file(file.inner().location()) {
        ProcNamespaceTarget::Live(kind, object) => (kind, object),
        ProcNamespaceTarget::NotNamespace => return Err(AxError::InvalidInput),
    };
    let (kind, object) = target;
    let expected_type = match kind {
        ProcNamespaceKind::Pid => CLONE_NEWPID,
        ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren => CLONE_NEWTIME,
        ProcNamespaceKind::User => CLONE_NEWUSER,
        ProcNamespaceKind::Uts => CLONE_NEWUTS,
    };
    if nstype != 0 && nstype != expected_type {
        return Err(AxError::InvalidInput);
    }
    enum Replacement {
        Uts(Arc<crate::task::UtsNamespace>),
        Time(Arc<crate::task::TimeNamespace>),
    }

    let replacement = match (kind, object) {
        (ProcNamespaceKind::Uts, ProcNamespaceObject::Uts(uts_ns)) => Replacement::Uts(uts_ns),
        (
            ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren,
            ProcNamespaceObject::Time(time_ns),
        ) => Replacement::Time(time_ns),
        (ProcNamespaceKind::Pid | ProcNamespaceKind::User, _) => {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        _ => return Err(AxError::InvalidInput),
    };
    let owner_user_ns = match &replacement {
        Replacement::Uts(uts_ns) => uts_ns.owner_user_ns(),
        Replacement::Time(time_ns) => time_ns.owner_user_ns(),
    };
    let curr = current();
    let thread = curr.as_thread();
    let actor_cred = thread.current_cred();
    if !ns_capable(&actor_cred, owner_user_ns, CAP_SYS_ADMIN) {
        return Err(AxError::OperationNotPermitted);
    }

    let curr_tid = linux_pid_from_task_id(curr.id().as_u64())?;
    if !thread.proc_data.begin_single_thread_scope_change(curr_tid) {
        return Err(AxError::OperationNotSupported);
    }
    match replacement {
        Replacement::Uts(uts_ns) => thread.proc_data.replace_uts_ns(uts_ns),
        Replacement::Time(time_ns) => thread.proc_data.replace_time_ns(time_ns),
    }
    thread.proc_data.end_exec(curr_tid);
    Ok(0)
}

fn validate_movable_node(node: i32) -> AxResult<()> {
    if node < 0 {
        return Err(AxError::NoSuchDevice);
    }
    if ALLOWED_NODEMASK & (1usize.checked_shl(node as u32).unwrap_or(0)) == 0 {
        return Err(AxError::NoSuchDevice);
    }
    Ok(())
}

fn validate_migration_nodes(mask: usize) -> AxResult<()> {
    if mask & !ALLOWED_NODEMASK == 0 {
        Ok(())
    } else {
        Err(AxError::InvalidInput)
    }
}

fn mempolicy_preferred_node(policy: Mempolicy) -> i32 {
    if policy.nodemask == 0 {
        return DEFAULT_NUMA_NODE;
    }
    policy.nodemask.trailing_zeros() as i32
}

fn mempolicy_target_mask(policy: Mempolicy) -> usize {
    if policy.nodemask != 0 {
        policy.nodemask
    } else {
        1usize << DEFAULT_NUMA_NODE
    }
}

fn nth_numa_node(mask: usize, ordinal: usize) -> Option<i32> {
    let mut seen = 0usize;
    for candidate in 0..usize::BITS as usize {
        if mask & (1usize.checked_shl(candidate as u32).unwrap_or(0)) == 0 {
            continue;
        }
        if seen == ordinal {
            return Some(candidate as i32);
        }
        seen += 1;
    }
    None
}

fn mempolicy_page_node(policy: Mempolicy, addr: usize) -> i32 {
    let mask = mempolicy_target_mask(policy) & ALLOWED_NODEMASK;
    if mask == 0 {
        return DEFAULT_NUMA_NODE;
    }

    if policy.mode == MPOL_INTERLEAVE as u32 {
        let ordinal = (addr / PAGE_SIZE_4K) % mask.count_ones() as usize;
        return nth_numa_node(mask, ordinal).unwrap_or(DEFAULT_NUMA_NODE);
    }

    mask.trailing_zeros() as i32
}

fn numa_page_node(target: &ProcessData, page: usize) -> AxResult<i32> {
    let start = VirtAddr::from(page).align_down_4k();
    let aspace_handle = target.aspace();
    let aspace = aspace_handle.lock();
    if !aspace.can_access_range(start, 1, MappingFlags::USER) {
        return Err(AxError::BadAddress);
    }
    aspace
        .page_table()
        .query(start)
        .map_err(|_| AxError::BadAddress)?;
    let policy = target
        .mempolicy_for_addr(start.as_usize())
        .unwrap_or_else(|| target.mempolicy());
    Ok(mempolicy_page_node(policy, start.as_usize()))
}

fn numa_page_is_shareable(target: &ProcessData, page: usize) -> bool {
    let start = VirtAddr::from(page).align_down_4k();
    let aspace_handle = target.aspace();
    let aspace = aspace_handle.lock();
    aspace
        .find_area(start)
        .is_some_and(|area| area.backend().is_shareable())
}

fn check_mbind_strict_resident(start: VirtAddr, len: usize, policy: Mempolicy) -> AxResult<()> {
    let target_mask = mempolicy_target_mask(policy);
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let mut offset = 0usize;

    while offset < len {
        let addr = start.as_usize() + offset;
        if let Ok(node) = numa_page_node(proc_data, addr) {
            let node_mask = 1usize.checked_shl(node as u32).unwrap_or(0);
            if target_mask & node_mask == 0 {
                return Err(LinuxError::EIO.into());
            }
        }
        offset += 4096;
    }

    Ok(())
}

fn cap_data_words(version: u32) -> usize {
    match version {
        _LINUX_CAPABILITY_VERSION_1 => 1,
        _ => 2,
    }
}

fn validate_cap_version(
    header_ptr: *mut __user_cap_header_struct,
) -> AxResult<__user_cap_header_struct> {
    // FIXME: AnyBitPattern
    let mut header = unsafe { header_ptr.vm_read_uninit()?.assume_init() };
    if !matches!(
        header.version,
        _LINUX_CAPABILITY_VERSION_1 | _LINUX_CAPABILITY_VERSION_2 | _LINUX_CAPABILITY_VERSION_3
    ) {
        header.version = _LINUX_CAPABILITY_VERSION_3;
        header_ptr.vm_write(header)?;
        return Err(AxError::InvalidInput);
    }
    Ok(header)
}

fn resolve_cap_task(header: __user_cap_header_struct) -> AxResult<AxTaskRef> {
    if header.pid < 0 {
        return Err(AxError::InvalidInput);
    }
    if header.pid == 0 {
        Ok(current().clone())
    } else {
        get_visible_task(header.pid as u32)
    }
}

fn validate_cap_header(
    header_ptr: *mut __user_cap_header_struct,
) -> AxResult<(__user_cap_header_struct, AxTaskRef)> {
    let header = validate_cap_version(header_ptr)?;
    let task = resolve_cap_task(header)?;
    Ok((header, task))
}

fn write_cap_data(
    data: *mut __user_cap_data_struct,
    version: u32,
    state: CapabilitySets,
) -> AxResult<()> {
    let effective = state.effective();
    let permitted = state.permitted();
    let inheritable = state.inheritable();
    for index in 0..cap_data_words(version) {
        data.wrapping_add(index).vm_write(__user_cap_data_struct {
            effective: effective[index],
            permitted: permitted[index],
            inheritable: inheritable[index],
        })?;
    }
    Ok(())
}

fn read_cap_data(data: *mut __user_cap_data_struct, version: u32) -> AxResult<CapsetRequest> {
    let mut effective = [0; CAPABILITY_WORDS];
    let mut permitted = [0; CAPABILITY_WORDS];
    let mut inheritable = [0; CAPABILITY_WORDS];

    for index in 0..cap_data_words(version) {
        let entry: __user_cap_data_struct =
            unsafe { data.wrapping_add(index).vm_read_uninit()?.assume_init() };
        let valid = CAPABILITY_VALID_MASK[index];
        effective[index] = entry.effective & valid;
        permitted[index] = entry.permitted & valid;
        inheritable[index] = entry.inheritable & valid;
    }
    CapsetRequest::try_new(effective, permitted, inheritable).map_err(cred_error)
}

pub fn sys_capget(
    header: *mut __user_cap_header_struct,
    data: *mut __user_cap_data_struct,
) -> AxResult<isize> {
    let header = match validate_cap_version(header) {
        Ok(header) => header,
        Err(err) if data.is_null() && err == AxError::InvalidInput => return Ok(0),
        Err(err) => return Err(err),
    };
    if data.is_null() {
        return Ok(0);
    }

    let task = resolve_cap_task(header)?;
    let cred = task.as_thread().current_cred();
    write_cap_data(data, header.version, cred.capabilities())?;
    Ok(0)
}

pub fn sys_capset(
    header: *mut __user_cap_header_struct,
    data: *mut __user_cap_data_struct,
) -> AxResult<isize> {
    let curr = current();
    let current_tid = curr.as_thread().tid();
    let (header, task) = validate_cap_header(header)?;
    if header.pid != 0 && header.pid as u32 != current_tid {
        return Err(AxError::OperationNotPermitted);
    }

    let request = read_cap_data(data, header.version)?;
    task.as_thread().apply_capset(request)?;

    Ok(0)
}

pub fn sys_umask(mask: u32) -> AxResult<isize> {
    let curr = current();
    let old = curr.as_thread().proc_data.replace_umask(mask & 0o777);
    Ok(old as isize)
}

pub fn sys_setreuid(ruid: u32, euid: u32) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let cred = thread.current_cred();
    let map = |uid| {
        (uid != NO_ID_CHANGE)
            .then(|| cred.user_ns().make_kuid(uid).ok_or(AxError::InvalidInput))
            .transpose()
    };
    thread.setreuid(map(ruid)?, map(euid)?)?;
    Ok(0)
}

pub fn sys_setregid(rgid: u32, egid: u32) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let cred = thread.current_cred();
    let map = |gid| {
        (gid != NO_ID_CHANGE)
            .then(|| cred.user_ns().make_kgid(gid).ok_or(AxError::InvalidInput))
            .transpose()
    };
    thread.setregid(map(rgid)?, map(egid)?)?;
    Ok(0)
}

pub fn sys_setresuid(ruid: u32, euid: u32, suid: u32) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let cred = thread.current_cred();
    let map = |uid| {
        (uid != NO_ID_CHANGE)
            .then(|| cred.user_ns().make_kuid(uid).ok_or(AxError::InvalidInput))
            .transpose()
    };
    thread.setresuid(map(ruid)?, map(euid)?, map(suid)?)?;
    Ok(0)
}

pub fn sys_setresgid(rgid: u32, egid: u32, sgid: u32) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let cred = thread.current_cred();
    let map = |gid| {
        (gid != NO_ID_CHANGE)
            .then(|| cred.user_ns().make_kgid(gid).ok_or(AxError::InvalidInput))
            .transpose()
    };
    thread.setresgid(map(rgid)?, map(egid)?, map(sgid)?)?;
    Ok(0)
}

pub fn sys_get_mempolicy(
    policy: *mut i32,
    nodemask: *mut usize,
    maxnode: usize,
    addr: usize,
    flags: usize,
) -> AxResult<isize> {
    if flags & !SUPPORTED_GET_MEMPOLICY_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & MPOL_F_MEMS_ALLOWED as usize != 0 {
        if flags & (MPOL_F_ADDR | MPOL_F_NODE) as usize != 0 {
            return Err(AxError::InvalidInput);
        }
        write_nodemask(nodemask, maxnode, ALLOWED_NODEMASK)?;
        return Ok(0);
    }
    if addr != 0 && flags & MPOL_F_ADDR as usize == 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let selected = if flags & MPOL_F_ADDR as usize != 0 {
        let addr = VirtAddr::from(addr);
        let aspace_handle = proc_data.aspace();
        if aspace_handle.lock().find_area(addr).is_none() {
            return Err(AxError::BadAddress);
        }
        proc_data
            .mempolicy_for_addr(addr.as_usize())
            .unwrap_or_else(|| Mempolicy::new(MPOL_DEFAULT as u32, 0))
    } else {
        proc_data.mempolicy()
    };

    if flags & MPOL_F_NODE as usize != 0 {
        if !nodemask.is_null() {
            return Err(AxError::InvalidInput);
        }
        if flags & MPOL_F_ADDR as usize != 0 {
            if !policy.is_null() {
                policy.vm_write(numa_page_node(proc_data, addr)?)?;
            }
            return Ok(0);
        }
        if !policy.is_null() {
            policy.vm_write(mempolicy_preferred_node(selected))?;
        }
        return Ok(0);
    }

    if !policy.is_null() {
        policy.vm_write(selected.mode as i32)?;
    }
    let returned_nodemask = if flags & MPOL_F_ADDR as usize == 0 && selected.nodemask == 0 {
        ALLOWED_NODEMASK
    } else {
        selected.nodemask
    };
    write_nodemask(nodemask, maxnode, returned_nodemask)?;
    Ok(0)
}

pub fn sys_set_mempolicy(mode: usize, nodemask: *const usize, maxnode: usize) -> AxResult<isize> {
    if mempolicy_mode_flags(mode) & !SUPPORTED_MODE_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    let nodemask = read_nodemask(nodemask, maxnode)?;
    let policy = validate_mempolicy(mode, nodemask, current_allowed_nodemask())?;
    current().as_thread().proc_data.set_mempolicy(policy);
    Ok(0)
}

pub fn sys_mbind(
    start: usize,
    len: usize,
    mode: usize,
    nodemask: *const usize,
    maxnode: usize,
    flags: usize,
) -> AxResult<isize> {
    if flags & !SUPPORTED_MBIND_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    let (start, len) = validate_mapped_user_range(start, len)?;
    let nodemask = read_nodemask(nodemask, maxnode)?;
    let policy = validate_mempolicy(mode, nodemask, current_allowed_nodemask())?;
    if flags & MPOL_MF_STRICT as usize != 0
        && flags & (MPOL_MF_MOVE | MPOL_MF_MOVE_ALL) as usize == 0
    {
        check_mbind_strict_resident(start, len, policy)?;
    }
    current()
        .as_thread()
        .proc_data
        .bind_mempolicy_range(start.as_usize(), len, policy);
    Ok(0)
}

pub fn sys_move_pages(
    pid: i32,
    nr_pages: usize,
    pages: *const usize,
    nodes: *const i32,
    status: *mut i32,
    flags: usize,
) -> AxResult<isize> {
    if flags & !SUPPORTED_MOVE_PAGES_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & MPOL_MF_MOVE_ALL as usize != 0 && !current_has_capability(CAP_SYS_NICE) {
        return Err(AxError::OperationNotPermitted);
    }
    if nr_pages == 0 {
        return Ok(0);
    }
    if pages.is_null() || status.is_null() {
        return Err(AxError::BadAddress);
    }

    let target = numa_target_process(pid)?;
    check_numa_target_permission(&target)?;

    for index in 0..nr_pages {
        let page = pages.wrapping_add(index).vm_read()?;
        let status_value = match numa_page_node(&target, page) {
            Ok(current_node) if nodes.is_null() => current_node,
            Ok(_) => {
                let node = nodes.wrapping_add(index).vm_read()?;
                validate_movable_node(node)?;
                if flags & MPOL_MF_MOVE_ALL as usize == 0 && numa_page_is_shareable(&target, page) {
                    -(LinuxError::EACCES.code() as i32)
                } else {
                    target.bind_mempolicy_range(
                        VirtAddr::from(page).align_down_4k().as_usize(),
                        4096,
                        Mempolicy::new(MPOL_BIND as u32, 1usize << node),
                    );
                    node
                }
            }
            Err(_) => -(LinuxError::EFAULT.code() as i32),
        };
        status.wrapping_add(index).vm_write(status_value)?;
    }

    Ok(0)
}

pub fn sys_migrate_pages(
    pid: i32,
    maxnode: usize,
    old_nodes: *const usize,
    new_nodes: *const usize,
) -> AxResult<isize> {
    let target = numa_target_process(pid)?;
    check_numa_target_permission(&target)?;

    let old_nodes = read_nodemask(old_nodes, maxnode)?;
    let new_nodes = read_nodemask(new_nodes, maxnode)?;
    validate_migration_nodes(old_nodes)?;
    validate_migration_nodes(new_nodes)?;
    target.migrate_mempolicy_ranges(old_nodes, new_nodes);
    Ok(0)
}

fn parse_pr_set_dumpable_args(
    arg2: usize,
    _arg3: usize,
    _arg4: usize,
    _arg5: usize,
) -> AxResult<Dumpability> {
    Dumpability::try_from(arg2)
}

fn pr_get_dumpable_value(
    dumpability: Dumpability,
    _arg2: usize,
    _arg3: usize,
    _arg4: usize,
    _arg5: usize,
) -> isize {
    dumpability as isize
}

/// prctl() is called with a first argument describing what to do, and further
/// arguments with a significance depending on the first one.
/// The first argument can be:
/// - PR_SET_NAME: set the name of the calling thread, using the value pointed to by `arg2`
/// - PR_GET_NAME: get the name of the calling
/// - PR_SET_MM options: set various memory management options (start/end code/data/brk/stack)
pub fn sys_prctl(
    option: u32,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> AxResult<isize> {
    use linux_raw_sys::prctl::*;

    debug!("sys_prctl <= option: {option}, args: {arg2}, {arg3}, {arg4}, {arg5}");

    match option {
        PR_SET_NAME => {
            let s = vm_load_string(arg2 as *const c_char)?;
            drop(current().replace_name(s));
        }
        PR_GET_NAME => {
            let name = current().try_name().map_err(|error| match error {
                axtask::TaskNameError::OutOfMemory => AxError::NoMemory,
                axtask::TaskNameError::ConcurrentMutation => AxError::ResourceBusy,
            })?;
            let len = name.len().min(15);
            let mut buf = [0; 16];
            buf[..len].copy_from_slice(&name.as_bytes()[..len]);
            vm_write_slice(arg2 as _, &buf)?;
        }
        PR_SET_DUMPABLE => {
            current()
                .as_thread()
                .proc_data
                .set_dumpability(parse_pr_set_dumpable_args(arg2, arg3, arg4, arg5)?);
        }
        PR_GET_DUMPABLE => {
            return Ok(pr_get_dumpable_value(
                current().as_thread().proc_data.dumpability(),
                arg2,
                arg3,
                arg4,
                arg5,
            ));
        }
        PR_SET_PDEATHSIG => {
            if arg2 > 64 {
                return Err(AxError::InvalidInput);
            }
            current().as_thread().set_pdeath_signal(arg2 as u32);
        }
        PR_GET_PDEATHSIG => {
            (arg2 as *mut i32).vm_write(current().as_thread().pdeath_signal() as i32)?;
        }
        PR_SET_CHILD_SUBREAPER => {
            current()
                .as_thread()
                .proc_data
                .proc
                .set_child_subreaper(arg2 != 0);
        }
        PR_GET_CHILD_SUBREAPER => {
            (arg2 as *mut i32)
                .vm_write(current().as_thread().proc_data.proc.is_child_subreaper() as i32)?;
        }
        PR_SET_TIMERSLACK => {
            current().as_thread().proc_data.set_timerslack_ns(arg2);
        }
        PR_GET_TIMERSLACK => {
            return Ok(current().as_thread().proc_data.timerslack_ns() as isize);
        }
        PR_SET_NO_NEW_PRIVS => {
            if arg2 != 1 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            current().as_thread().set_no_new_privs()?;
        }
        PR_GET_NO_NEW_PRIVS => {
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            return Ok(current().as_thread().no_new_privs() as isize);
        }
        PR_GET_KEEPCAPS => {
            return Ok(current().as_thread().keep_caps() as isize);
        }
        PR_SET_KEEPCAPS => {
            if arg2 > 1 {
                return Err(AxError::InvalidInput);
            }
            current().as_thread().set_keep_caps(arg2 != 0)?;
        }
        PR_GET_SECCOMP | PR_SET_SECCOMP | PR_MCE_KILL | PR_SET_THP_DISABLE | PR_GET_THP_DISABLE => {
            return Err(AxError::InvalidInput);
        }
        PR_CAP_AMBIENT => {
            let curr = current();
            let thread = curr.as_thread();
            match arg2 as u32 {
                PR_CAP_AMBIENT_CLEAR_ALL => {
                    if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                        return Err(AxError::InvalidInput);
                    }
                    thread.clear_ambient_capabilities()?;
                }
                PR_CAP_AMBIENT_IS_SET => {
                    if arg4 != 0 || arg5 != 0 {
                        return Err(AxError::InvalidInput);
                    }
                    return Ok(thread.ambient_capability_enabled(arg3 as u32)? as isize);
                }
                PR_CAP_AMBIENT_RAISE => {
                    if arg4 != 0 || arg5 != 0 {
                        return Err(AxError::InvalidInput);
                    }
                    thread.raise_ambient_capability(arg3 as u32)?;
                }
                PR_CAP_AMBIENT_LOWER => {
                    if arg4 != 0 || arg5 != 0 {
                        return Err(AxError::InvalidInput);
                    }
                    thread.lower_ambient_capability(arg3 as u32)?;
                }
                _ => return Err(AxError::InvalidInput),
            }
        }
        PR_CAPBSET_DROP => {
            let curr = current();
            curr.as_thread().drop_bounding_capability(arg2 as u32)?;
        }
        PR_CAPBSET_READ => {
            return Ok(current()
                .as_thread()
                .bounding_capability_enabled(arg2 as u32)? as isize);
        }
        PR_SET_SECUREBITS => {
            if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            let curr = current();
            curr.as_thread().set_securebits(arg2 as u32)?;
        }
        PR_GET_SECUREBITS => {
            return Ok(current().as_thread().securebits() as isize);
        }
        PR_SET_MM => {
            return Err(AxError::OperationNotSupported);
        }
        _ => {
            warn!("sys_prctl: unsupported option {option}");
            return Err(AxError::InvalidInput);
        }
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::*;
    use crate::task::{Cred, Kgid, Kuid, UserNamespace};

    #[test]
    fn process_access_prctl_dumpable_validates_value_only() {
        assert_eq!(
            parse_pr_set_dumpable_args(0, 7, 8, 9),
            Ok(Dumpability::NotDumpable)
        );
        assert_eq!(
            parse_pr_set_dumpable_args(1, usize::MAX, 8, 9),
            Ok(Dumpability::UserDumpable)
        );
        assert_eq!(
            parse_pr_set_dumpable_args(2, 0, 0, 0),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            parse_pr_set_dumpable_args(usize::MAX, 0, 0, 0),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            pr_get_dumpable_value(Dumpability::UserDumpable, 1, 2, 3, 4),
            1
        );
    }

    #[test]
    fn process_access_kcmp_keeps_and_validates_the_authorized_image() {
        let authorized = Arc::new(());
        let old_image = authorized.clone();
        let replacement = Arc::new(());
        let authorized = KcmpAuthorizedImage::new(authorized);

        assert!(Arc::ptr_eq(authorized.pinned(), &old_image));
        assert_eq!(
            validate_kcmp_fd_image(&authorized, false, |image| {
                Arc::ptr_eq(image, &old_image)
            }),
            Ok(())
        );
        assert_eq!(
            validate_kcmp_fd_image(&authorized, false, |image| {
                Arc::ptr_eq(image, &replacement)
            }),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            validate_kcmp_fd_image(&authorized, true, |_| true),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn namespace_owner_unshare_freezes_actor_user_namespace() {
        let root = UserNamespace::try_new_root().unwrap();
        let root_cred = Cred::try_root(root.clone()).unwrap();
        let child = root
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let actor = Cred::try_with_user_namespace(&root_cred, child.clone()).unwrap();

        let owner = unshare_namespace_owner(CLONE_NEWUTS | CLONE_NEWTIME, &actor).unwrap();
        assert!(Arc::ptr_eq(&owner, &child));
        assert!(!Arc::ptr_eq(&owner, &root));
    }
}
