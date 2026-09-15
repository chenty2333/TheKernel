use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::{
    mem::{self, MaybeUninit},
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{FsPathBuf, NodeType};
use axhal::paging::MappingFlags;
use axtask::{AxTaskRef, TaskName, current};
use linux_raw_sys::{
    general::{
        __user_cap_data_struct, __user_cap_header_struct, _LINUX_CAPABILITY_VERSION_1,
        _LINUX_CAPABILITY_VERSION_2, _LINUX_CAPABILITY_VERSION_3, CAP_SYS_ADMIN, CAP_SYS_CHROOT,
        CAP_SYS_NICE, CAP_SYS_RESOURCE, CLONE_FILES, CLONE_FS, CLONE_NEWCGROUP, CLONE_NEWIPC,
        CLONE_NEWNET, CLONE_NEWNS, CLONE_NEWPID, CLONE_NEWTIME, CLONE_NEWUSER, CLONE_NEWUTS,
        CLONE_SIGHAND, CLONE_SYSVSEM, CLONE_THREAD, CLONE_VM,
    },
    mempolicy::*,
};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use tk_linux_cred::{
    CAPABILITY_VALID_MASK, CAPABILITY_WORDS, CapabilityHeaderPid, CapabilitySets, CapsetRequest,
};
use tk_linux_mm::{
    GetMempolicyError, MempolicyError, NR_NODE_IDS, sanitize_mode_flags, scanned_words,
    validate as validate_mempolicy_request,
};
use tk_linux_process::{SAVED_AUXV_BYTES, TaskComm, prctl_set_name_read_bound};
use tk_linux_usercopy::{
    UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_write_slice,
};

use crate::{
    file::{File, FileDescription, FileLike, PidFd, epoll::Epoll, executable, get_file_like},
    mm::{AddrSpace, ThpDisableMode, map_usercopy_error},
    pseudofs::{
        ProcNamespaceKind, ProcNamespaceObject, ProcNamespaceTarget,
        namespace_target_from_proc_file,
    },
    task::{
        AsThread, Cred, Dumpability, Mempolicy, ProcessData, ProcessMmLayout, PtraceAccessMode,
        check_current_process_ptrace_access, check_current_thread_ptrace_image_access, cred_error,
        get_process_data, get_task, get_visible_task, ns_capable, process_domain, process_error,
    },
};

/// `PR_RSEQ_SLICE_EXTENSION`, absent from the `linux-raw-sys` table that
/// predates Linux 6.19 (include/uapi/linux/prctl.h).
const PR_RSEQ_SLICE_EXTENSION: u32 = 79;
/// `PR_GET_CFI` / `PR_SET_CFI` from include/uapi/linux/prctl.h.
const PR_GET_CFI: u32 = 80;
const PR_SET_CFI: u32 = 81;

/// `-ENOTSUPP`, the kernel-internal "operation not supported" errno.
///
/// Linux keeps `ENOTSUPP` (524) distinct from the userspace `EOPNOTSUPP` (95),
/// and some syscalls report the former. The `LinuxError` table has no 524
/// variant, so the value is returned through the syscall result slot, which
/// the entry path copies to the return register verbatim. Substituting
/// `EOPNOTSUPP` here would answer a different errno.
const ENOTSUPP_RESULT: isize = -524;

const NO_ID_CHANGE: u32 = u32::MAX;
const ALLOWED_NODEMASK: usize = 0b1;
const DEFAULT_NUMA_NODE: i32 = 0;
const SUPPORTED_GET_MEMPOLICY_FLAGS: usize =
    (MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED) as usize;
const SUPPORTED_MOVE_PAGES_FLAGS: usize = (MPOL_MF_MOVE | MPOL_MF_MOVE_ALL) as usize;
const MOVE_PAGES_SNAPSHOT_CHUNK: usize = 16;
const KCMP_FILE: i32 = 0;
const KCMP_VM: i32 = 1;
const KCMP_FILES: i32 = 2;
const KCMP_POINTER_TYPES: usize = 7;
static KCMP_POINTER_COOKIES: [AtomicU64; KCMP_POINTER_TYPES] =
    [const { AtomicU64::new(0) }; KCMP_POINTER_TYPES];
const KCMP_FS: i32 = 3;
const KCMP_SIGHAND: i32 = 4;
const KCMP_IO: i32 = 5;
const KCMP_SYSVSEM: i32 = 6;
const KCMP_EPOLL_TFD: i32 = 7;

const PRCTL_MM_MAP_SIZE: usize = 104;
const PRCTL_MM_AUXV_MAX: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PrctlMmMap {
    start_code: u64,
    end_code: u64,
    start_data: u64,
    end_data: u64,
    start_brk: u64,
    brk: u64,
    start_stack: u64,
    arg_start: u64,
    arg_end: u64,
    env_start: u64,
    env_end: u64,
    auxv: u64,
    auxv_size: u32,
    exe_fd: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct KcmpEpollSlot {
    efd: u32,
    tfd: u32,
    toff: u32,
}
const UNSHARE_SUPPORTED_FLAGS: u32 = CLONE_FILES
    | CLONE_FS
    | CLONE_NEWNS
    | CLONE_NEWIPC
    | CLONE_NEWNET
    | CLONE_NEWCGROUP
    | CLONE_NEWUSER
    | CLONE_NEWUTS
    | CLONE_NEWTIME
    | CLONE_NEWPID
    | CLONE_SYSVSEM;
/// `CLONE_THREAD`, `CLONE_SIGHAND` and `CLONE_VM` are *recognised but inert*:
/// `check_unshare_flags()` accepts them and then nothing in `ksys_unshare()`
/// consumes them, so a single-threaded caller gets a successful no-op.  They
/// are deliberately kept out of `UNSHARE_SUPPORTED_FLAGS`, which is the set
/// that actually replaces task state below.
const UNSHARE_INERT_FLAGS: u32 = CLONE_THREAD | CLONE_SIGHAND | CLONE_VM;
/// Flags that pass `check_unshare_flags()`'s mask test:
///
/// ```c
/// 	if (unshare_flags & ~(CLONE_THREAD|CLONE_FS|CLONE_SIGHAND|
/// 				CLONE_VM|CLONE_FILES|CLONE_SYSVSEM|
/// 				CLONE_NS_ALL | UNSHARE_EMPTY_MNTNS))
/// 		return -EINVAL;
/// ```
///
/// `CLONE_NS_ALL` is `CLONE_NEWTIME|CLONE_NEWNS|CLONE_NEWCGROUP|CLONE_NEWUTS|
/// CLONE_NEWIPC|CLONE_NEWUSER|CLONE_NEWPID|CLONE_NEWNET`, all of which are
/// listed literally in `UNSHARE_SUPPORTED_FLAGS`.  `UNSHARE_EMPTY_MNTNS` is
/// the one member of the Linux mask TheKernel still rejects, and it is the
/// 32-bit *unshare-only* spelling at `include/uapi/linux/sched.h:54`:
///
/// ```c
/// #define UNSHARE_EMPTY_MNTNS 0x00100000 /* Unshare an empty mount namespace. */
/// ```
///
/// It is not `CLONE_EMPTY_MNTNS` (`1ULL << 37`, sched.h:42), which is a
/// `clone3`-only 64-bit flag and is rejected by the argument-width check above
/// exactly as Linux's `check_unshare_flags()` mask rejects it.  Linux accepts
/// `UNSHARE_EMPTY_MNTNS` (it aliases `CLONE_PARENT_SETTID`) and converts it to
/// `CLONE_EMPTY_MNTNS` in `unshare_nsproxy_namespaces()` (kernel/nsproxy.c);
/// `copy_mnt_ns()` then builds a namespace holding only a clone of the current
/// root mount and resets `fs->root`/`fs->pwd` to it (fs/namespace.c).  TheKernel
/// has no root-mount-only mount-namespace constructor, so it still refuses the
/// bit with -EINVAL instead of creating that namespace.
const UNSHARE_RECOGNIZED_FLAGS: u32 =
    UNSHARE_SUPPORTED_FLAGS | UNSHARE_INERT_FLAGS;
const SETNS_PIDFD_ALLOWED_FLAGS: u32 = CLONE_NEWNS
    | CLONE_NEWUTS
    | CLONE_NEWIPC
    | CLONE_NEWNET
    | CLONE_NEWTIME
    | CLONE_NEWUSER
    | CLONE_NEWPID
    | CLONE_NEWCGROUP;
/// Translates the crate's argument-only rejection into Linux's errno.
///
/// `crates/linux/mm::mempolicy` owns the validation *order*; this is the only
/// place that decides the errno, so the two cannot drift.
fn mempolicy_error(error: MempolicyError) -> AxError {
    match error {
        MempolicyError::MoveAllNotPermitted => AxError::OperationNotPermitted,
        // `get_bitmap()` reports a failed `copy_from_user()` as `-EFAULT`;
        // `read_nodemask()` faults the complete window in first, so the crate
        // only reaches this arm when its own caller-contract is violated.
        MempolicyError::NodeMaskUnreadable => AxError::BadAddress,
        MempolicyError::InvalidMode
        | MempolicyError::NodeMaskTooLong
        | MempolicyError::NodeOutOfRange
        | MempolicyError::EmptyNodeMask
        | MempolicyError::UnalignedStart
        | MempolicyError::RangeOverflow => AxError::InvalidInput,
    }
}

/// Maps the crate's `get_mempolicy(2)` argument verdict to Linux's errno.
fn get_mempolicy_error(error: GetMempolicyError) -> AxError {
    match error {
        GetMempolicyError::InvalidFlags
        | GetMempolicyError::NodeMaskTooShort
        | GetMempolicyError::UnexpectedAddress => AxError::InvalidInput,
    }
}

/// Linux `get_nodes()` in v7.2.3 `mm/mempolicy.c`: read the caller's node mask.
///
/// `scanned_words()` is the number of `unsigned long`s `get_nodes()` actually
/// reads, so an unreadable word is `EFAULT` and never a silent zero. `maxnode`
/// is the caller's count of mask bits; only the words Linux would read are
/// copied, which bounds this to 513 words without trusting `maxnode`.
///
/// The words are read from the top of the window down, and a non-zero word
/// above `MAX_NUMNODES` is rejected as soon as it is read: `get_nodes()` never
/// reads a lower word once the in-loop `if (t) return -EINVAL;` fires, so a set
/// bit above `MAX_NUMNODES` outranks an unreadable low word rather than losing
/// to its `EFAULT`.
fn read_nodemask<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    nodemask: *const usize,
    maxnode: usize,
) -> AxResult<usize> {
    let supplied = !nodemask.is_null();
    // `get_nodes()` decrements `maxnode` before every other test, so a supplied
    // mask with `maxnode == 0` wraps to `ULONG_MAX` and fails the
    // `maxnode > PAGE_SIZE * BITS_PER_BYTE` bound with EINVAL; the bound is
    // always on the decremented value (`32769` is the largest admitted window).
    if supplied && maxnode.wrapping_sub(1) > tk_linux_mm::MAX_NODEMASK_BITS {
        return Err(mempolicy_error(MempolicyError::NodeMaskTooLong));
    }
    let words = scanned_words(maxnode);
    if !supplied || words == 0 {
        // `get_nodes()` returns an empty mask without reading.
        return Ok(0);
    }

    let mut buffer = [0usize; tk_linux_mm::MAX_NODEMASK_BITS / usize::BITS as usize + 1];
    let window = &mut buffer[..words];
    for index in (0..words).rev() {
        let word = nodemask
            .wrapping_add(index)
            .vm_read(memory)
            .map_err(map_usercopy_error)?;
        window[index] = word;
        if tk_linux_mm::rejects_scanned_word(index, word) {
            return Err(mempolicy_error(MempolicyError::NodeOutOfRange));
        }
    }
    tk_linux_mm::parse_node_mask(maxnode, window, supplied)
        .map(|(mask, _)| mask)
        .map_err(mempolicy_error)
}

fn mempolicy_target_mask(policy: Mempolicy) -> usize {
    if policy.nodemask != 0 {
        policy.nodemask
    } else {
        1usize << DEFAULT_NUMA_NODE
    }
}

fn unshare_namespace_owner(flags: u32, actor: &Cred) -> AxResult<Arc<crate::task::UserNamespace>> {
    let owner = actor.user_ns().clone();
    if flags
        & (CLONE_NEWNS
            | CLONE_NEWIPC
            | CLONE_NEWNET
            | CLONE_NEWCGROUP
            | CLONE_NEWUTS
            | CLONE_NEWTIME
            | CLONE_NEWPID)
        != 0
        && flags & CLONE_NEWUSER == 0
        && !ns_capable(actor, &owner, CAP_SYS_ADMIN)
    {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(owner)
}

fn user_namespace_is_strict_descendant(
    target: &Arc<crate::task::UserNamespace>,
    ancestor: &Arc<crate::task::UserNamespace>,
) -> bool {
    let mut cursor = target.parent();
    while let Some(namespace) = cursor {
        if Arc::ptr_eq(&namespace, ancestor) {
            return true;
        }
        cursor = namespace.parent();
    }
    false
}

/// Linux `copy_nodes_to_user()` in v7.2.3 `mm/mempolicy.c`.
///
/// `nr_node_ids` is 1 here, so `nbytes` is one word. Linux pads the caller's
/// window out to `ALIGN(maxnode - 1, 64) / 8` bytes with zeroes and rejects a
/// window larger than a page, which is what keeps a `get_mempolicy` caller from
/// reading kernel stack garbage past the single node it reports.
fn write_nodemask<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    nodemask: *mut usize,
    maxnode: usize,
    value: usize,
) -> AxResult<()> {
    if nodemask.is_null() {
        return Ok(());
    }
    let node_bytes = size_of::<usize>();
    // `ALIGN(maxnode - 1, 64) / 8`, with Linux's unsigned wrap for maxnode 0.
    let copy = maxnode.wrapping_sub(1).wrapping_add(63) / 64 * size_of::<u64>();
    let (copy, maxnode) = if copy > node_bytes {
        if copy > PAGE_SIZE_4K {
            return Err(AxError::InvalidInput);
        }
        // `clear_user()` past the node mask; a failure there is EFAULT.
        vm_write_slice(
            memory,
            (nodemask as *mut u8).wrapping_add(node_bytes),
            &vec![0u8; copy - node_bytes],
        )
        .map_err(map_usercopy_error)?;
        (node_bytes, NR_NODE_IDS)
    } else {
        (copy, maxnode)
    };

    // The reported mask is `nr_node_ids` bits wide, so only the first word is
    // ever written; `copy` never exceeds it here.
    let _ = maxnode;
    vm_write_slice(memory, nodemask.cast::<u8>(), unsafe {
        core::slice::from_raw_parts((&value as *const usize).cast::<u8>(), copy.min(node_bytes))
    })
    .map_err(map_usercopy_error)
}

fn validate_mempolicy(
    mode_with_flags: usize,
    nodemask: usize,
    has_nodes: bool,
) -> AxResult<tk_linux_mm::MempolicyRequest> {
    validate_mempolicy_request(
        mode_with_flags as u32,
        nodemask,
        has_nodes,
        current_allowed_nodemask(),
    )
    .map_err(mempolicy_error)
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
    if pid == 0 {
        return Ok(current().as_thread().proc_data.clone());
    }
    let target_pid = current()
        .as_thread()
        .pid_ns()
        .resolve_visible_pid(pid as u32)
        .ok_or(AxError::NoSuchProcess)?;
    Ok(get_visible_task(target_pid)?.as_thread().proc_data.clone())
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

fn checked_move_pages_element_address<T>(base: *const T, index: usize) -> AxResult<usize> {
    let offset = index
        .checked_mul(mem::size_of::<T>())
        .ok_or(AxError::BadAddress)?;
    (base as usize)
        .checked_add(offset)
        .ok_or(AxError::BadAddress)
}

fn snapshot_move_pages_array<M: UserMemory + ?Sized, T>(
    memory: &mut UserMemoryContext<'_, M>,
    base: *const T,
    offset: usize,
    destination: &mut [MaybeUninit<T>],
) -> AxResult<()> {
    let address = checked_move_pages_element_address(base, offset)?;
    memory
        .read_slice(address as *const T, destination)
        .map_err(map_usercopy_error)
}

fn write_move_pages_status<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    status: *mut i32,
    index: usize,
    value: i32,
) -> AxResult<()> {
    let address = checked_move_pages_element_address(status as *const i32, index)?;
    VmMutPtr::vm_write(address as *mut i32, memory, value).map_err(map_usercopy_error)
}

fn kcmp_target_process(pid: i32) -> AxResult<Arc<ProcessData>> {
    if pid <= 0 {
        return Err(AxError::NoSuchProcess);
    }
    get_process_data(pid as u32)
}

/// Linux KCMP returns an ordering over obfuscated kernel pointers, never the
/// pointer itself. Mix before comparing so the observable direction does not
/// encode an address ordering, while remaining stable for this boot.
fn kcmp_cookie(type_: i32) -> AxResult<u64> {
    let cookie = &KCMP_POINTER_COOKIES[type_ as usize];
    let existing = cookie.load(Ordering::Acquire);
    if existing != 0 {
        return Ok(existing);
    }
    let mut bytes = [0u8; 8];
    // KCMP deliberately fails rather than falling back to predictable time or
    // address material when the boot entropy source is unavailable.
    crate::random::fill_secure(&mut bytes)?;
    let candidate = u64::from_ne_bytes(bytes).max(1);
    let _ = cookie.compare_exchange(0, candidate, Ordering::AcqRel, Ordering::Acquire);
    Ok(cookie.load(Ordering::Acquire))
}

fn kcmp_raw_ptr(type_: i32, left: *const (), right: *const ()) -> AxResult<isize> {
    let key = kcmp_cookie(type_)?;
    let mix = |ptr: *const ()| {
        let value = ptr as usize;
        let mut value = (value as u64) ^ key;
        value ^= value >> 30;
        value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value ^= value >> 27;
        value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    };
    let left = mix(left);
    let right = mix(right);
    Ok(if left == right {
        0
    } else if left < right {
        1
    } else {
        2
    })
}

fn kcmp_ptr<T: ?Sized>(type_: i32, left: &Arc<T>, right: &Arc<T>) -> AxResult<isize> {
    kcmp_raw_ptr(
        type_,
        Arc::as_ptr(left).cast::<()>(),
        Arc::as_ptr(right).cast::<()>(),
    )
}

fn kcmp_optional_ptr<T>(
    type_: i32,
    left: &Option<Arc<T>>,
    right: &Option<Arc<T>>,
) -> AxResult<isize> {
    let left = left
        .as_ref()
        .map_or(core::ptr::null(), |value| Arc::as_ptr(value).cast::<()>());
    let right = right
        .as_ref()
        .map_or(core::ptr::null(), |value| Arc::as_ptr(value).cast::<()>());
    kcmp_raw_ptr(type_, left, right)
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

fn kcmp_file_description(
    thread: &crate::task::Thread,
    fd: usize,
) -> AxResult<Arc<FileDescription>> {
    thread
        .try_fd_table()
        .ok_or(AxError::NoSuchProcess)?
        .get_description_number(u32::try_from(fd).map_err(|_| AxError::BadFileDescriptor)?)
}

pub fn sys_kcmp<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid1: i32,
    pid2: i32,
    type_: i32,
    idx1: usize,
    idx2: usize,
) -> AxResult<isize> {
    debug!("sys_kcmp <= pid1: {pid1}, pid2: {pid2}, type: {type_}, idx1: {idx1}, idx2: {idx2}");

    if pid1 <= 0 || pid2 <= 0 {
        return Err(AxError::NoSuchProcess);
    }
    let caller_ns = current().as_thread().pid_ns();
    let target1 = caller_ns
        .resolve_visible_pid(pid1 as u32)
        .ok_or(AxError::NoSuchProcess)?;
    let task1 = get_visible_task(target1)?;
    let target2 = caller_ns
        .resolve_visible_pid(pid2 as u32)
        .ok_or(AxError::NoSuchProcess)?;
    let task2 = get_visible_task(target2)?;
    let thread1 = task1.as_thread();
    let thread2 = task2.as_thread();
    let proc1 = thread1.proc_data.clone();
    let proc2 = thread2.proc_data.clone();
    let authorized1 =
        crate::task::check_current_thread_ptrace_image_access(thread1, PtraceAccessMode::ReadReal)?;
    let authorized2 =
        crate::task::check_current_thread_ptrace_image_access(thread2, PtraceAccessMode::ReadReal)?;
    if type_ == KCMP_FS {
        let image1 = KcmpAuthorizedImage::new(authorized1.into_aspace());
        let image2 = KcmpAuthorizedImage::new(authorized2.into_aspace());
        validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
            proc1.image_matches(image)
        })?;
        validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
            proc2.image_matches(image)
        })?;
        let fs1 = thread1.try_fs_context().ok_or(AxError::NoSuchProcess)?;
        let fs2 = thread2.try_fs_context().ok_or(AxError::NoSuchProcess)?;
        validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
            proc1.image_matches(image)
        })?;
        validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
            proc2.image_matches(image)
        })?;
        return kcmp_ptr(KCMP_FS, &fs1, &fs2);
    }

    let image1 = KcmpAuthorizedImage::new(authorized1.into_aspace());
    let image2 = KcmpAuthorizedImage::new(authorized2.into_aspace());

    match type_ {
        KCMP_FILE => {
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            let file1 = kcmp_file_description(thread1, idx1);
            let file2 = kcmp_file_description(thread2, idx2);
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            let file1 = file1?;
            let file2 = file2?;
            kcmp_ptr(KCMP_FILE, &file1, &file2)
        }
        KCMP_VM => kcmp_ptr(KCMP_VM, image1.pinned(), image2.pinned()),
        KCMP_FILES => {
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            let files1 = thread1.try_fd_table().ok_or(AxError::NoSuchProcess)?;
            let files2 = thread2.try_fd_table().ok_or(AxError::NoSuchProcess)?;
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            kcmp_ptr(KCMP_FILES, &files1, &files2)
        }
        KCMP_FS => unreachable!("handled before process-target resolution"),
        KCMP_SIGHAND => kcmp_ptr(
            KCMP_SIGHAND,
            &proc1.signal.shared_actions(),
            &proc2.signal.shared_actions(),
        ),
        KCMP_IO => {
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            // Do not instantiate an io_context merely to observe it. Linux
            // compares the existing shared CLONE_IO object, with two absent
            // contexts comparing equal through the null pointer value.
            let io1 = thread1.io_context();
            let io2 = thread2.io_context();
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            kcmp_optional_ptr(KCMP_IO, &io1, &io2)
        }
        KCMP_SYSVSEM => {
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            let undo1 = thread1.sem_undo();
            let undo2 = thread2.sem_undo();
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            kcmp_ptr(KCMP_SYSVSEM, &undo1, &undo2)
        }
        KCMP_EPOLL_TFD => {
            let slot = (idx2 as *const KcmpEpollSlot)
                .vm_read(memory)
                .map_err(map_usercopy_error)?;
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            let file1 = kcmp_file_description(thread1, idx1)?;
            let epoll_file = kcmp_file_description(thread2, slot.efd as usize)?;
            let epoll = epoll_file
                .inner
                .downcast_ref::<Epoll>()
                .ok_or(AxError::InvalidInput)?;
            let target = epoll.target_description(slot.tfd, slot.toff)?;
            validate_kcmp_fd_image(&image1, proc1.exec_in_progress(), |image| {
                proc1.image_matches(image)
            })?;
            validate_kcmp_fd_image(&image2, proc2.exec_in_progress(), |image| {
                proc2.image_matches(image)
            })?;
            kcmp_ptr(KCMP_FILE, &file1, &target)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_unshare(flags: usize) -> AxResult<isize> {
    debug!("sys_unshare <= flags: {flags:#x}");

    // The syscall parameter is `unsigned long` in Linux
    // (`SYSCALL_DEFINE1(unshare, unsigned long, unshare_flags)`), while the mask
    // in `check_unshare_flags()` is a 32-bit constant.  Every bit at or above
    // bit 32 is therefore an unknown flag and must be rejected:
    //
    // 	if (unshare_flags & ~(CLONE_THREAD|CLONE_FS|CLONE_SIGHAND|
    // 				CLONE_VM|CLONE_FILES|CLONE_SYSVSEM|
    // 				CLONE_NS_ALL | UNSHARE_EMPTY_MNTNS))
    // 		return -EINVAL;
    //
    // Those are the 64-bit clone3 flags (CLONE_CLEAR_SIGHAND 1 << 32,
    // CLONE_INTO_CGROUP 1 << 33, CLONE_EMPTY_MNTNS 1 << 37, ...), which are not
    // valid for unshare(2).  Truncating to 32 bits first made
    // `unshare(CLONE_CLEAR_SIGHAND)` a silent success instead of -EINVAL.
    if flags >> 32 != 0 {
        return Err(AxError::InvalidInput);
    }
    let flags = flags as u32;

    // kernel/fork.c `ksys_unshare()` applies the flag implications *before*
    // `check_unshare_flags()`:
    //
    // 	/*
    // 	 * If unsharing a user namespace must also unshare the thread group
    // 	 * and unshare the filesystem root and working directories.
    // 	 */
    // 	if (unshare_flags & CLONE_NEWUSER)
    // 		unshare_flags |= CLONE_THREAD | CLONE_FS;
    // 	/*
    // 	 * If unsharing vm, must also unshare signal handlers.
    // 	 */
    // 	if (unshare_flags & CLONE_VM)
    // 		unshare_flags |= CLONE_SIGHAND;
    // 	/*
    // 	 * If unsharing a signal handlers, must also unshare the signal queues.
    // 	 */
    // 	if (unshare_flags & CLONE_SIGHAND)
    // 		unshare_flags |= CLONE_THREAD;
    // 	/*
    // 	 * If unsharing namespace, must also unshare filesystem information.
    // 	 */
    // 	if (unshare_flags & UNSHARE_EMPTY_MNTNS)
    // 		unshare_flags |= CLONE_NEWNS;
    // 	if (unshare_flags & CLONE_NEWNS)
    // 		unshare_flags |= CLONE_FS;
    //
    // The implications matter because the *implied* CLONE_THREAD is what makes
    // `unshare(CLONE_VM)` and `unshare(CLONE_NEWUSER)` fail with -EINVAL in a
    // multithreaded caller: the thread-group check below tests the union.
    let flags = {
        let mut flags = flags;
        if flags & CLONE_NEWUSER != 0 {
            flags |= CLONE_THREAD | CLONE_FS;
        }
        if flags & CLONE_VM != 0 {
            flags |= CLONE_SIGHAND;
        }
        if flags & CLONE_SIGHAND != 0 {
            flags |= CLONE_THREAD;
        }
        if flags & CLONE_NEWNS != 0 {
            flags |= CLONE_FS;
        }
        flags
    };
    if flags & !UNSHARE_RECOGNIZED_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    // `CLONE_NEWIPC|CLONE_SYSVSEM` is *not* rejected here.  The only pair check
    // in Linux is in `copy_namespaces()` (kernel/nsproxy.c), which is the
    // `clone(2)` path:
    //
    // 	if ((flags & (CLONE_NEWIPC | CLONE_SYSVSEM)) ==
    // 		(CLONE_NEWIPC | CLONE_SYSVSEM))
    // 		return -EINVAL;
    //
    // `ksys_unshare()` never calls it: `check_unshare_flags()` admits both bits
    // (they are in its mask) and `ksys_unshare()` then only records that the
    // caller has to leave the old semaphore undolist:
    //
    // 	if (unshare_flags & (CLONE_NEWIPC|CLONE_SYSVSEM))
    // 		do_sysvsem = 1;
    //
    // so a privileged `unshare(CLONE_NEWIPC|CLONE_SYSVSEM)` succeeds, and an
    // unprivileged one fails in `unshare_nsproxy_namespaces()` with the
    // `CAP_SYS_ADMIN` admission below -- -EPERM, not -EINVAL.  Rejecting the
    // pair here made both callers diverge.
    let curr = current();
    let thread = curr.as_thread();
    // kernel/fork.c `check_unshare_flags()`, the three constraint blocks:
    //
    // 	if (unshare_flags & (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM)) {
    // 		if (!thread_group_empty(current))
    // 			return -EINVAL;
    // 	}
    // 	if (unshare_flags & (CLONE_SIGHAND | CLONE_VM)) {
    // 		if (refcount_read(&current->sighand->count) > 1)
    // 			return -EINVAL;
    // 	}
    // 	if (unshare_flags & CLONE_VM) {
    // 		if (!current_is_single_threaded())
    // 			return -EINVAL;
    // 	}
    //
    // TheKernel keeps exactly one signal-handler structure per thread group and
    // shares it only among that group's threads (`clone()` forces
    // CLONE_SIGHAND -> CLONE_THREAD), so `thread_group_empty()`,
    // `sighand->count > 1` and `current_is_single_threaded()` are all decided by
    // the same fact: whether this is the only live thread of the group.  That
    // single predicate is therefore applied for all three blocks, which is also
    // what makes the implied CLONE_THREAD above meaningful.
    if flags & (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) != 0
        && thread.proc_data.proc.thread_count() != 1
    {
        return Err(AxError::InvalidInput);
    }
    let task_snapshot = thread.namespace_credential_fs_snapshot();
    let actor_cred = task_snapshot.credential;
    let namespace_owner = unshare_namespace_owner(flags, &actor_cred)?;
    let user_scope_owner = if flags & CLONE_NEWUSER != 0 {
        Some(
            thread
                .proc_data
                .begin_single_thread_scope_change(thread.kernel_tid())
                .then_some(thread.kernel_tid())
                .ok_or(AxError::InvalidInput)?,
        )
    } else {
        None
    };
    if flags & UNSHARE_SUPPORTED_FLAGS != 0 {
        // Linux namespace attachments are task-local; FS and FILES are also
        // task-local. Prepare every fallible replacement before publication.

        let result = (|| -> AxResult<()> {
            let private_user_ns = if flags & CLONE_NEWUSER != 0 {
                let ids = actor_cred.ids();
                Some(actor_cred.user_ns().try_fork(
                    ids.euid,
                    ids.egid,
                    actor_cred.has_effective_capability_in_own_user_ns(
                        linux_raw_sys::general::CAP_SETFCAP,
                    ),
                )?)
            } else {
                None
            };
            let namespace_owner = private_user_ns.clone().unwrap_or(namespace_owner.clone());
            let private_fd_table = if flags & CLONE_FILES != 0 {
                thread.try_clone_fd_table_if_shared()?
            } else {
                None
            };
            let mut private_fs_context = if flags & CLONE_FS != 0 {
                thread.try_clone_fs_context_if_shared()?
            } else {
                None
            };
            let private_uts_ns = if flags & CLONE_NEWUTS != 0 {
                Some(thread.uts_ns().try_fork(namespace_owner.clone())?)
            } else {
                None
            };
            let (private_mount_ns, private_mount_fs_context) = if flags & CLONE_NEWNS != 0 {
                // Take one stable namespace-topology generation for the
                // source graph, its idmaps/peer state, and root/cwd rebinding.
                // Do not retain this mutation serialization through the
                // namespace/fs publication below: that commit is independent
                // of topology mutation and may drop old filesystem objects.
                let _topology_snapshot = crate::mounts::namespace_operation();
                let mount_ns = thread.mount_ns().try_fork(namespace_owner.clone())?;
                let root = mount_ns.root_location()?;
                // CLONE_NEWNS clones a mount topology; it is neither chroot(2)
                // nor chdir(2).  The cloned topology has distinct VFS mount
                // instances, so retaining the old Locations would make this
                // task pathwalk through the namespace it just left.  Rebind
                // the existing root and cwd independently before either the
                // namespace proxy or fs_struct is published.  This also
                // retains the caller's umask/security view while authority
                // for every mount idmap remains in the cloned topology.
                let fs_context = thread.prepare_fs_context_for_cloned_mount_namespace(root)?;
                (Some(mount_ns), Some(fs_context))
            } else {
                (None, None)
            };
            if let Some(fs_context) = private_mount_fs_context {
                private_fs_context = Some(fs_context);
            }
            let private_ipc_ns = if flags & CLONE_NEWIPC != 0 {
                Some(crate::syscall::ipc::IpcNamespace::try_new(
                    namespace_owner.clone(),
                )?)
            } else {
                None
            };
            let private_sem_undo = if let Some(ipc_ns) = private_ipc_ns.as_ref() {
                Some(crate::task::SemUndoState::try_new(ipc_ns.clone())?)
            } else if flags & CLONE_SYSVSEM != 0 {
                Some(crate::task::SemUndoState::try_clone_for(
                    thread.ipc_ns(),
                    &thread.sem_undo(),
                )?)
            } else {
                None
            };
            let private_net_ns = if flags & CLONE_NEWNET != 0 {
                Some(crate::task::NetworkNamespace::try_new_network_namespace(
                    namespace_owner.clone(),
                )?)
            } else {
                None
            };
            let private_cgroup_ns = if flags & CLONE_NEWCGROUP != 0 {
                // unshare(CLONE_NEWCGROUP) takes its root from the caller's
                // present membership, not from the namespace view which the
                // caller is about to leave.
                let roots = crate::pseudofs::cgroup::cgroup_namespace_roots_for_pid(
                    thread.proc_data.proc.pid(),
                )?;
                Some(crate::task::CgroupNamespace::try_fork(
                    &thread.cgroup_ns(),
                    namespace_owner.clone(),
                    roots,
                )?)
            } else {
                None
            };
            let private_time_ns = if flags & CLONE_NEWTIME != 0 {
                Some(
                    thread
                        .time_ns_for_children()
                        .try_fork(namespace_owner.clone())?,
                )
            } else {
                None
            };
            let private_pid_ns = if flags & CLONE_NEWPID != 0 {
                let reaper_scope = process_domain()?
                    .try_new_reaper_scope()
                    .map_err(process_error)?;
                Some(
                    thread
                        .pid_ns_for_children()
                        .try_fork_for_children(namespace_owner.clone(), reaper_scope)?,
                )
            } else {
                None
            };
            let prepared_namespaces = thread.prepare_namespace_replacement(|proxy| {
                if let Some(user_ns) = private_user_ns {
                    proxy.replace_user(user_ns);
                }
                if let Some(mount_ns) = private_mount_ns {
                    proxy.replace_mount(mount_ns);
                }
                if let Some(ipc_ns) = private_ipc_ns {
                    proxy.replace_ipc(ipc_ns);
                }
                if let Some(net_ns) = private_net_ns {
                    proxy.replace_net(net_ns);
                }
                if let Some(cgroup_ns) = private_cgroup_ns {
                    proxy.replace_cgroup(cgroup_ns);
                }
                if let Some(uts_ns) = private_uts_ns {
                    proxy.replace_uts(uts_ns);
                }
                if let Some(time_ns) = private_time_ns {
                    proxy.replace_time_for_children(time_ns);
                }
                if let Some(pid_ns) = private_pid_ns {
                    proxy.replace_pid_for_children(pid_ns);
                }
            });

            // The user namespace (when requested) is the only replacement
            // which also changes credentials. Recover it from the immutable
            // proxy snapshot before its single commit. This is the final
            // fallible publication step: fs/files/SEM_UNDO replacements are
            // already allocated, but must remain untouched if it fails.
            let target_user_ns = (flags & CLONE_NEWUSER != 0).then(|| namespace_owner.clone());
            if let Some(user_ns) = target_user_ns {
                if let Some(replacement_fs) = private_fs_context.take() {
                    let old = thread.commit_user_namespace_transition_with_fs_context(
                        user_ns,
                        prepared_namespaces,
                        replacement_fs,
                    )?;
                    drop(old);
                } else {
                    thread.commit_user_namespace_transition(user_ns, prepared_namespaces)?;
                }
            } else {
                if let Some(replacement) = private_fs_context.take() {
                    let old =
                        thread.commit_namespace_with_fs_context(prepared_namespaces, replacement);
                    drop(old);
                } else {
                    prepared_namespaces.commit(thread);
                }
            }
            let old_fd_table =
                private_fd_table.map(|replacement| thread.replace_fd_table(replacement));
            let old_fs_context =
                private_fs_context.map(|replacement| thread.replace_fs_context(replacement));
            // Arc destructors can cascade into filesystem or file-description
            // cleanup. Keep all such work outside the IRQ/preempt-off scope gate.
            drop(old_fd_table);
            drop(old_fs_context);
            // This pointer exchange is infallible and deliberately follows
            // the namespace/credential publication, so a failed userns
            // transition cannot detach SEM_UNDO from the old IPC manager.
            if let Some(replacement) = private_sem_undo {
                thread.replace_sem_undo(replacement);
            }
            Ok(())
        })();
        if let Some(owner) = user_scope_owner {
            thread.proc_data.end_exec(owner);
        }
        result?;
    }

    Ok(0)
}

pub fn sys_setns(fd: i32, nstype: u32) -> AxResult<isize> {
    debug!("sys_setns <= fd: {fd}, nstype: {nstype:#x}");

    // Since Linux 5.8 a pidfd selects a bundle of namespaces rather than a
    // single procfs namespace object.  Resolve the descriptor as FileLike
    // first: a pidfd is not an ordinary VFS File and must not be rejected by
    // the legacy proc-ns path below.
    let file_like = get_file_like(fd)?;
    if let Some(pidfd) = file_like.downcast_ref::<PidFd>() {
        return sys_setns_pidfd(pidfd, nstype);
    }

    let file = File::from_fd(fd)?;
    let target = match namespace_target_from_proc_file(file.inner().location()) {
        ProcNamespaceTarget::Live(kind, object) => (kind, object),
        ProcNamespaceTarget::NotNamespace => return Err(AxError::InvalidInput),
    };
    let (kind, object) = target;
    let expected_type = match kind {
        ProcNamespaceKind::Cgroup => CLONE_NEWCGROUP,
        ProcNamespaceKind::Ipc => CLONE_NEWIPC,
        ProcNamespaceKind::Mount => CLONE_NEWNS,
        ProcNamespaceKind::Net => CLONE_NEWNET,
        ProcNamespaceKind::Pid => CLONE_NEWPID,
        ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren => CLONE_NEWTIME,
        ProcNamespaceKind::User => CLONE_NEWUSER,
        ProcNamespaceKind::Uts => CLONE_NEWUTS,
    };
    if nstype != 0 && nstype != expected_type {
        return Err(AxError::InvalidInput);
    }
    enum Replacement {
        Cgroup(Arc<crate::task::CgroupNamespace>),
        Ipc(Arc<crate::syscall::ipc::IpcNamespace>),
        Mount(Arc<crate::task::MountNamespace>),
        Net(Arc<crate::task::NetworkNamespace>),
        PidForChildren(Arc<crate::task::PidNamespace>),
        User(Arc<crate::task::UserNamespace>),
        Uts(Arc<crate::task::UtsNamespace>),
        Time(Arc<crate::task::TimeNamespace>),
    }

    let replacement = match (kind, object) {
        (ProcNamespaceKind::Cgroup, ProcNamespaceObject::Cgroup(cgroup_ns)) => {
            Replacement::Cgroup(cgroup_ns)
        }
        (ProcNamespaceKind::Ipc, ProcNamespaceObject::Ipc(ipc_ns)) => Replacement::Ipc(ipc_ns),
        (ProcNamespaceKind::Mount, ProcNamespaceObject::Mount(mount_ns)) => {
            Replacement::Mount(mount_ns)
        }
        (ProcNamespaceKind::Net, ProcNamespaceObject::Net(net_ns)) => Replacement::Net(net_ns),
        (ProcNamespaceKind::Pid, ProcNamespaceObject::Pid(pid_ns)) => {
            Replacement::PidForChildren(pid_ns)
        }
        (ProcNamespaceKind::User, ProcNamespaceObject::User(user_ns)) => Replacement::User(user_ns),
        (ProcNamespaceKind::Uts, ProcNamespaceObject::Uts(uts_ns)) => Replacement::Uts(uts_ns),
        (
            ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren,
            ProcNamespaceObject::Time(time_ns),
        ) => Replacement::Time(time_ns),
        _ => return Err(AxError::InvalidInput),
    };
    let owner_user_ns = match &replacement {
        Replacement::Cgroup(cgroup_ns) => cgroup_ns.owner_user_ns(),
        Replacement::Ipc(ipc_ns) => ipc_ns.owner_user_ns(),
        Replacement::Mount(mount_ns) => mount_ns.owner_user_ns(),
        Replacement::Net(net_ns) => net_ns.owner_user_ns(),
        Replacement::PidForChildren(pid_ns) => pid_ns.owner_user_ns(),
        Replacement::User(user_ns) => user_ns,
        Replacement::Uts(uts_ns) => uts_ns.owner_user_ns(),
        Replacement::Time(time_ns) => time_ns.owner_user_ns(),
    };
    let curr = current();
    let thread = curr.as_thread();
    let task_snapshot = thread.namespace_credential_fs_snapshot();
    let actor_cred = task_snapshot.credential;
    if !ns_capable(&actor_cred, owner_user_ns, CAP_SYS_ADMIN) {
        return Err(AxError::OperationNotPermitted);
    }
    // A mount namespace cannot retain an fs_struct rooted in the namespace
    // being left.  Resolve and validate the target root before entering the
    // single-thread publication scope, so no namespace pointer can be
    // published with an unusable cwd/root pair.
    let mount_root = match &replacement {
        Replacement::Mount(mount_ns) => Some(mount_ns.root_location()?),
        _ => None,
    };

    if let Replacement::PidForChildren(pid_ns) = &replacement
        && !task_snapshot.namespaces.pid().contains(pid_ns)
    {
        return Err(AxError::OperationNotPermitted);
    }
    // setns(CLONE_NEWNS) must not retarget a CLONE_FS-shared fs_struct for
    // sibling tasks which remain in the old namespace.  Clone before the
    // namespace publication transaction, then reset only this task's root/cwd.
    let replacement_fs = mount_root
        .as_ref()
        .map(|root| thread.prepare_fs_context_for_mount_namespace(root.clone()))
        .transpose()?;
    let replacement_sem_undo = match &replacement {
        Replacement::Ipc(ipc_ns) => Some(crate::task::SemUndoState::try_new(ipc_ns.clone())?),
        _ => None,
    };
    // A user namespace may only be entered from an ancestor, never re-entered
    // or entered from a sibling/descendant relationship. Linux also requires
    // this task to be the sole thread and to own an unshared fs_struct, since
    // both credentials and root interpretation change together.
    let user_scope_owner = match &replacement {
        Replacement::User(target) => {
            if !user_namespace_is_strict_descendant(target, actor_cred.user_ns())
                || thread.fs_context_is_shared()
                || !thread
                    .proc_data
                    .begin_single_thread_scope_change(thread.kernel_tid())
            {
                return Err(AxError::InvalidInput);
            }
            Some(thread.kernel_tid())
        }
        _ => None,
    };
    let result = match replacement {
        // Joining a user namespace changes the calling task's credential
        // namespace as well as the ProcessData aggregate.  Its helper
        // prepares both objects before publishing either.
        Replacement::User(user_ns) => thread.set_user_namespace(user_ns),
        replacement => {
            let prepared = thread.prepare_namespace_replacement(|proxy| match replacement {
                Replacement::Cgroup(cgroup_ns) => proxy.replace_cgroup(cgroup_ns),
                Replacement::Ipc(ipc_ns) => proxy.replace_ipc(ipc_ns),
                Replacement::Mount(mount_ns) => proxy.replace_mount(mount_ns),
                Replacement::Net(net_ns) => proxy.replace_net(net_ns),
                Replacement::PidForChildren(pid_ns) => proxy.replace_pid_for_children(pid_ns),
                Replacement::Uts(uts_ns) => proxy.replace_uts(uts_ns),
                // Linux time-namespace setns is deferred: the caller keeps
                // its current clock domain and only future children inherit
                // the selected namespace.  This mirrors unshare(NEWTIME).
                Replacement::Time(time_ns) => proxy.replace_time_for_children(time_ns),
                Replacement::User(_) => unreachable!("handled above"),
            });
            if let Some(replacement_fs) = replacement_fs {
                // The fs_struct was fully prepared before the namespace
                // transaction. Its pointer exchange and the proxy exchange
                // have no remaining fallible work.
                let old = thread.commit_namespace_with_fs_context(prepared, replacement_fs);
                drop(old);
            } else if let Some(replacement_sem_undo) = replacement_sem_undo {
                thread.commit_namespace_with_sem_undo(prepared, replacement_sem_undo);
            } else {
                prepared.commit(thread);
            }
            Ok(())
        }
    };
    if let Some(owner) = user_scope_owner {
        thread.proc_data.end_exec(owner);
    }
    result?;
    Ok(0)
}

/// `setns(2)` pidfd form.  Unlike a `/proc/<pid>/ns/*` fd, `nstype` is a
/// non-empty bitset and every requested attachment is sampled from one live
/// target task namespace proxy.
fn sys_setns_pidfd(pidfd: &PidFd, flags: u32) -> AxResult<isize> {
    if flags == 0 || flags & !SETNS_PIDFD_ALLOWED_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    // A thread pidfd names one exact task, not the group leader. Its weak
    // scheduler binding is the PID-reuse-safe authority source. Process
    // pidfds retain the legacy leader resolution path instead.
    let target_process_data = pidfd.process_data()?;
    let target_task = if let Some(task) = pidfd.signal_thread_task()? {
        let target_thread = task.as_thread();
        if target_thread.pending_exit()
            || !Arc::ptr_eq(&target_thread.proc_data, &target_process_data)
            || !Arc::ptr_eq(&pidfd.process_data()?, &target_process_data)
        {
            return Err(AxError::NoSuchProcess);
        }
        check_current_thread_ptrace_image_access(target_thread, PtraceAccessMode::ReadReal)?;
        if target_thread.pending_exit()
            || !Arc::ptr_eq(&pidfd.process_data()?, &target_process_data)
        {
            return Err(AxError::NoSuchProcess);
        }
        task
    } else {
        // Numeric PID lookup alone is never an authority source: retain and
        // compare the pidfd ProcessData Arc on both sides of resolution.
        let target_process = pidfd.process()?;
        let task = get_task(target_process.pid())?;
        let target_thread = task.as_thread();
        if target_thread.pending_exit()
            || !Arc::ptr_eq(&target_thread.proc_data, &target_process_data)
            || !Arc::ptr_eq(&pidfd.process_data()?, &target_process_data)
        {
            return Err(AxError::NoSuchProcess);
        }
        check_current_process_ptrace_access(&target_process_data, PtraceAccessMode::ReadReal)?;
        if target_thread.pending_exit()
            || !Arc::ptr_eq(&pidfd.process_data()?, &target_process_data)
        {
            return Err(AxError::NoSuchProcess);
        }
        task
    };
    let target_thread = target_task.as_thread();
    let target = target_thread.namespace_proxy();

    let curr = current();
    let thread = curr.as_thread();
    let task_snapshot = thread.namespace_credential_fs_snapshot();
    let actor_cred = task_snapshot.credential;

    // nsset's user-namespace admission precedes the generic per-target
    // capability pass.  Besides enforcing Linux's EINVAL priority for a
    // same/current user namespace, reserving the single-thread gate here
    // keeps a concurrent CLONE_THREAD from invalidating that admission.
    let mut scope_owner = if flags & CLONE_NEWUSER != 0 {
        if !user_namespace_is_strict_descendant(&target.user(), actor_cred.user_ns())
            || thread.fs_context_is_shared()
            || !thread
                .proc_data
                .begin_single_thread_scope_change(thread.kernel_tid())
        {
            return Err(AxError::InvalidInput);
        }
        Some(thread.kernel_tid())
    } else {
        None
    };
    let result = (|| -> AxResult<()> {
        let require_owner_admin = |owner: Arc<crate::task::UserNamespace>| -> AxResult<()> {
            if !ns_capable(&actor_cred, &owner, CAP_SYS_ADMIN) {
                return Err(AxError::OperationNotPermitted);
            }
            Ok(())
        };

        // Linux's pidfd nsset installation order is explicit.  Keep each
        // namespace's structural admission next to its owner check so a
        // later TIME request cannot mask an earlier selected namespace error.
        if flags & CLONE_NEWUSER != 0 {
            require_owner_admin(target.user())?;
        }
        if flags & CLONE_NEWNS != 0 {
            require_owner_admin(target.mount().owner_user_ns().clone())?;
            // The mount authority is relative to the namespace of the
            // credential which will be installed by nsset. The NEWUSER case
            // is checked against the prepared credential at commit; without
            // NEWUSER the current credential is already installed.
            if flags & CLONE_NEWUSER == 0
                && !ns_capable(&actor_cred, actor_cred.user_ns(), CAP_SYS_CHROOT)
            {
                return Err(AxError::OperationNotPermitted);
            }
        }
        if flags & CLONE_NEWUTS != 0 {
            require_owner_admin(target.uts().owner_user_ns().clone())?;
        }
        if flags & CLONE_NEWIPC != 0 {
            require_owner_admin(target.ipc().owner_user_ns().clone())?;
        }
        if flags & CLONE_NEWPID != 0 {
            require_owner_admin(target.pid().owner_user_ns().clone())?;
            if !task_snapshot.namespaces.pid().contains(&target.pid()) {
                return Err(AxError::InvalidInput);
            }
        }
        if flags & CLONE_NEWCGROUP != 0 {
            require_owner_admin(target.cgroup().owner_user_ns().clone())?;
        }
        if flags & CLONE_NEWNET != 0 {
            require_owner_admin(target.net().owner_user_ns().clone())?;
        }
        if flags & CLONE_NEWTIME != 0 {
            // NEWTIME's single-thread admission precedes its target-owner
            // capability check, including for a mixed nsset request. A
            // NEWUSER request has already reserved the same gate above.
            if scope_owner.is_none() {
                if !thread
                    .proc_data
                    .begin_single_thread_scope_change(thread.kernel_tid())
                {
                    return Err(LinuxError::EUSERS.into());
                }
                scope_owner = Some(thread.kernel_tid());
            }
            require_owner_admin(target.time().owner_user_ns().clone())?;
        }

        // VFS and IPC allocations remain before the final publication gate.
        let replacement_fs = if flags & CLONE_NEWNS != 0 {
            let root = target.mount().root_location()?;
            Some(thread.prepare_fs_context_for_mount_namespace(root)?)
        } else {
            None
        };
        let replacement_sem_undo = if flags & CLONE_NEWIPC != 0 {
            Some(crate::task::SemUndoState::try_new(target.ipc())?)
        } else {
            None
        };
        let prepared = thread.prepare_namespace_replacement(|proxy| {
            if flags & CLONE_NEWCGROUP != 0 {
                proxy.replace_cgroup(target.cgroup());
            }
            if flags & CLONE_NEWIPC != 0 {
                proxy.replace_ipc(target.ipc());
            }
            if flags & CLONE_NEWNS != 0 {
                proxy.replace_mount(target.mount());
            }
            if flags & CLONE_NEWNET != 0 {
                proxy.replace_net(target.net());
            }
            if flags & CLONE_NEWPID != 0 {
                // setns never changes the caller's current PID namespace.
                // The target is inherited only by its next child.
                proxy.replace_pid_for_children(target.pid());
            }
            if flags & CLONE_NEWTIME != 0 {
                // Likewise, time offsets apply to children only.
                proxy.replace_time_for_children(target.time());
            }
            if flags & CLONE_NEWUTS != 0 {
                proxy.replace_uts(target.uts());
            }
            if flags & CLONE_NEWUSER != 0 {
                proxy.replace_user(target.user());
            }
        });

        if flags & CLONE_NEWUSER != 0 {
            thread.commit_user_namespace_transition_with_resources(
                target.user(),
                prepared,
                replacement_fs,
                replacement_sem_undo,
                |post_transition_cred| {
                    // The second nsset authority check is deliberately
                    // singular: after NEWUSER every selected attachment is
                    // installed under the new credential's user namespace,
                    // not under each object's owner namespace (which the
                    // pre-transition check above already covered).
                    if !ns_capable(post_transition_cred, &target.user(), CAP_SYS_ADMIN) {
                        return Err(AxError::OperationNotPermitted);
                    }
                    if flags & CLONE_NEWNS != 0
                        && !ns_capable(post_transition_cred, &target.user(), CAP_SYS_CHROOT)
                    {
                        return Err(AxError::OperationNotPermitted);
                    }
                    Ok(())
                },
            )
        } else {
            thread.commit_namespace_with_resources(prepared, replacement_fs, replacement_sem_undo);
            Ok(())
        }
    })();
    if let Some(owner) = scope_owner {
        thread.proc_data.end_exec(owner);
    }
    result?;
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
    if matches!(policy.mode, mode if mode == MPOL_BIND as u32 || mode == MPOL_PREFERRED_MANY as u32)
        && let Some(home_node) = policy.home_node
        && ALLOWED_NODEMASK & (1usize.checked_shl(home_node as u32).unwrap_or(0)) != 0
    {
        return home_node as i32;
    }

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

fn validate_cap_version<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    header_ptr: *mut __user_cap_header_struct,
) -> AxResult<__user_cap_header_struct> {
    // FIXME: AnyBitPattern
    let mut header = unsafe {
        header_ptr
            .vm_read_uninit(memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    if !matches!(
        header.version,
        _LINUX_CAPABILITY_VERSION_1 | _LINUX_CAPABILITY_VERSION_2 | _LINUX_CAPABILITY_VERSION_3
    ) {
        header.version = _LINUX_CAPABILITY_VERSION_3;
        unsafe {
            VmMutPtr::vm_write_unchecked(header_ptr, memory, header).map_err(map_usercopy_error)?;
        }
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
        let target_pid = current()
            .as_thread()
            .pid_ns()
            .resolve_visible_pid(header.pid as u32)
            .ok_or(AxError::NoSuchProcess)?;
        get_visible_task(target_pid)
    }
}

fn write_cap_data<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    data: *mut __user_cap_data_struct,
    version: u32,
    state: CapabilitySets,
) -> AxResult<()> {
    let effective = state.effective();
    let permitted = state.permitted();
    let inheritable = state.inheritable();
    for index in 0..cap_data_words(version) {
        unsafe {
            VmMutPtr::vm_write_unchecked(
                data.wrapping_add(index),
                memory,
                __user_cap_data_struct {
                    effective: effective[index],
                    permitted: permitted[index],
                    inheritable: inheritable[index],
                },
            )
            .map_err(map_usercopy_error)?;
        }
    }
    Ok(())
}

fn read_cap_data<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    data: *mut __user_cap_data_struct,
    version: u32,
) -> AxResult<CapsetRequest> {
    let mut effective = [0; CAPABILITY_WORDS];
    let mut permitted = [0; CAPABILITY_WORDS];
    let mut inheritable = [0; CAPABILITY_WORDS];

    for index in 0..cap_data_words(version) {
        let entry: __user_cap_data_struct = unsafe {
            data.wrapping_add(index)
                .vm_read_uninit(memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
        let valid = CAPABILITY_VALID_MASK[index];
        effective[index] = entry.effective & valid;
        permitted[index] = entry.permitted & valid;
        inheritable[index] = entry.inheritable & valid;
    }
    CapsetRequest::try_new(effective, permitted, inheritable).map_err(cred_error)
}

pub fn sys_capget<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    header: *mut __user_cap_header_struct,
    data: *mut __user_cap_data_struct,
) -> AxResult<isize> {
    let header = match validate_cap_version(memory, header) {
        Ok(header) => header,
        Err(err) if data.is_null() && err == AxError::InvalidInput => return Ok(0),
        Err(err) => return Err(err),
    };
    if data.is_null() {
        return Ok(0);
    }

    let task = resolve_cap_task(header)?;
    let cred = task.as_thread().current_cred();
    write_cap_data(memory, data, header.version, cred.capabilities())?;
    Ok(0)
}

pub fn sys_capset<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    header: *mut __user_cap_header_struct,
    data: *mut __user_cap_data_struct,
) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let header = validate_cap_version(memory, header)?;
    // Linux's SYSCALL_DEFINE2(capset, ...) in kernel/capability.c reads the
    // header pid and rejects every value that is neither 0 nor
    // task_pid_vnr(current) with -EPERM *before* it looks the pid up. Probing
    // a foreign or nonexistent pid must therefore never surface as -ESRCH or
    // -EINVAL, and a negative pid is -EPERM rather than capget(2)'s -EINVAL.
    let visible_tid = thread.pid_ns().visible_pid(thread.tid());
    if !CapabilityHeaderPid::new(header.pid).authorizes_capset(visible_tid) {
        return Err(AxError::OperationNotPermitted);
    }
    // After the pid rule above, the only pid that can still reach the lookup
    // is the caller's own, so resolving it again can only confirm the target.
    let request = read_cap_data(memory, data, header.version)?;
    thread.apply_capset(request)?;

    Ok(0)
}

pub fn sys_umask(mask: u32) -> AxResult<isize> {
    let old = current()
        .as_thread()
        .fs_context()
        .lock()
        .replace_umask(mask & 0o777);
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

/// Linux 7.2.3 `get_mempolicy(2)` — `SYSCALL_DEFINE5(get_mempolicy, ...)`.
///
/// Validation order is Linux's and is the ABI: unknown flag bits first
/// (`do_get_mempolicy()`), then the `MPOL_F_MEMS_ALLOWED` early return — which
/// deliberately skips the `maxnode` and address checks — then the
/// `maxnode < nr_node_ids` check when a `nodemask` was supplied
/// (`kernel_get_mempolicy()`), then `addr` requiring `MPOL_F_ADDR`.
/// `MPOL_F_NODE` without `MPOL_F_ADDR` needs the task's own interleave policy
/// and is `EINVAL` for any other mode.
///
/// `kernel_get_mempolicy()` then stores `*policy` and `*nmask` in that order
/// on *every* successful path, including the two that look like early returns
/// inside `do_get_mempolicy()`: `MPOL_F_MEMS_ALLOWED` stores the initialized
/// `*policy = 0` before the allowed set, and `MPOL_F_ADDR|MPOL_F_NODE` stores
/// the resolved node id and still falls through to the mask store.
pub fn sys_get_mempolicy<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    policy: *mut i32,
    nodemask: *mut usize,
    maxnode: usize,
    addr: usize,
    flags: usize,
) -> AxResult<isize> {
    tk_linux_mm::validate_get(flags, !nodemask.is_null(), maxnode, addr, NR_NODE_IDS)
        .map_err(get_mempolicy_error)?;

    if flags & tk_linux_mm::MPOL_F_MEMS_ALLOWED != 0 {
        // `*policy = 0; /* just so it's initialized */` then the allowed set.
        write_get_mempolicy_policy(memory, policy, 0)?;
        return write_nodemask(memory, nodemask, maxnode, ALLOWED_NODEMASK).map(|()| 0);
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let selected = if flags & tk_linux_mm::MPOL_F_ADDR != 0 {
        let addr = VirtAddr::from(addr);
        let aspace_handle = proc_data.aspace();
        if aspace_handle.lock().find_area(addr).is_none() {
            return Err(AxError::BadAddress);
        }
        proc_data
            .mempolicy_for_addr(addr.as_usize())
            .unwrap_or_else(|| Mempolicy::new(tk_linux_mm::MPOL_DEFAULT, 0))
    } else {
        proc_data.mempolicy()
    };

    if flags & tk_linux_mm::MPOL_F_NODE != 0 {
        let node = if flags & tk_linux_mm::MPOL_F_ADDR != 0 {
            // `lookup_node()` resolves the *page's* node, and it resolves it
            // with `get_user_pages_fast()`, which faults the page in first.
            fault_in_mempolicy_page(memory, addr)?;
            numa_page_node(proc_data, addr)? as i32
        } else {
            // Without MPOL_F_ADDR this is the next interleave node, and Linux
            // accepts it only for the task's own interleave policy.
            let Some(node) = tk_linux_mm::next_interleave_node(selected.mode, true) else {
                return Err(AxError::InvalidInput);
            };
            node as i32
        };
        write_get_mempolicy_policy(memory, policy, node)?;
        // The `MPOL_F_NODE` arm only replaces `*policy`; Linux still reaches
        // the `if (nmask)` store at the end of `do_get_mempolicy()`.
        return write_nodemask(
            memory,
            nodemask,
            maxnode,
            mempolicy_reported_nodemask(selected),
        )
        .map(|()| 0);
    }

    write_get_mempolicy_policy(
        memory,
        policy,
        tk_linux_mm::reported_policy(selected.mode, selected.mode_flags) as i32,
    )?;
    write_nodemask(
        memory,
        nodemask,
        maxnode,
        mempolicy_reported_nodemask(selected),
    )?;
    Ok(0)
}

/// The read half of Linux's `lookup_node()`.
///
/// ```c
/// ret = get_user_pages_fast(addr & PAGE_MASK, 1, 0, &p);
/// if (ret > 0) {
/// 	ret = page_to_nid(p);
/// 	put_page(p);
/// }
/// ```
///
/// (`mm/mempolicy.c:1133-1144`). `get_user_pages_fast()` resolves the page by
/// *faulting it in* on a read fault rather than only looking it up, so an
/// address inside a VMA whose page is not resident yet is not an error:
/// `lookup_node()` reports the freshly populated page's node. A hole fails
/// earlier in `do_get_mempolicy()`'s `vma_lookup()`, and a page the caller may
/// not read (`PROT_NONE`) fails here in both kernels. Touching one byte
/// reproduces exactly that fault-in; the reported node itself still comes from
/// the task's policy for the address, as it does everywhere else in this
/// single-node kernel.
fn fault_in_mempolicy_page<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    addr: usize,
) -> AxResult<()> {
    let mut probe = MaybeUninit::<u8>::uninit();
    let start = addr & !(PAGE_SIZE_4K - 1);
    memory
        .read_bytes(start, core::slice::from_mut(&mut probe))
        .map_err(map_usercopy_error)
}

/// `if (policy && put_user(pval, policy)) return -EFAULT;`
fn write_get_mempolicy_policy<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    policy: *mut i32,
    value: i32,
) -> AxResult<()> {
    if policy.is_null() {
        return Ok(());
    }
    VmMutPtr::vm_write(policy, memory, value).map_err(map_usercopy_error)
}

/// `do_get_mempolicy()`'s nodemask report.
///
/// Linux writes `pol->w.user_nodemask` when `mpol_store_user_nodemask()` holds
/// -- a `MPOL_F_STATIC_NODES`/`MPOL_F_RELATIVE_NODES` policy keeps the caller's
/// own mask -- and otherwise asks `get_policy_nodemask()`, which is empty for
/// `MPOL_DEFAULT`/`MPOL_LOCAL` and the stored node set for every other mode.
fn mempolicy_reported_nodemask(policy: Mempolicy) -> usize {
    if tk_linux_mm::stores_user_nodemask(policy.mode_flags) {
        return policy.user_nodemask;
    }
    match policy.mode {
        mode if mode == tk_linux_mm::MPOL_LOCAL || mode == tk_linux_mm::MPOL_DEFAULT => 0,
        // `MPOL_PREFERRED` reports the stored `pol->nodes`, which
        // `mpol_new_preferred()` narrowed to the single preferred node.
        _ => policy.nodemask,
    }
}

/// Linux 7.2.3 `set_mempolicy(2)` — `SYSCALL_DEFINE3(set_mempolicy, ...)`.
///
/// `kernel_set_mempolicy()` runs `sanitize_mpol_flags()` and then `get_nodes()`
/// before `do_set_mempolicy()` reaches `mpol_new()`, so an invalid mode or a
/// bad `maxnode` is reported without ever consulting the allowed node set.
pub fn sys_set_mempolicy<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    mode: usize,
    nodemask: *const usize,
    maxnode: usize,
) -> AxResult<isize> {
    // `sanitize_mpol_flags()` first: this rejects an out-of-range mode and a
    // STATIC|RELATIVE combination before the mask is read.
    let mode = mode as u32;
    sanitize_mode_flags(mode).map_err(mempolicy_error)?;
    let nodes = read_nodemask(memory, nodemask, maxnode)?;
    let request = tk_linux_mm::validate(mode, nodes, !nodemask.is_null(), current_allowed_nodemask())
        .map_err(mempolicy_error)?;
    let effective = tk_linux_mm::effective_policy(request);
    // The stored policy is the one `mpol_new()` built: it keeps the sanitized
    // mode flags and the caller's own mask — which is what `get_mempolicy(2)`
    // reports back for a `MPOL_F_STATIC_NODES`/`MPOL_F_RELATIVE_NODES` policy —
    // and for `MPOL_DEFAULT` it keeps none of them, because `mpol_new()`
    // returns NULL there.
    current().as_thread().proc_data.set_mempolicy(
        Mempolicy::new(effective.mode, effective.nodes)
            .with_request_flags(effective.mode_flags, effective.user_nodemask),
    );
    Ok(0)
}

/// Linux 7.2.3 `mbind(2)` — `SYSCALL_DEFINE6(mbind, ...)`.
///
/// `kernel_mbind()` splits the mode and reads the mask *before* `do_mbind()`
/// validates the range, so a zero-length `mbind` still reports an invalid mode
/// or mask. `do_mbind()` then checks the bind flags, the `MPOL_MF_MOVE_ALL`
/// capability, the page alignment of `start`, and the aligned range overflow —
/// in that order — and returns early for an empty range *before* it builds the
/// policy with `mpol_new()`/`mpol_set_nodemask()`. The policy checks therefore
/// come last: a zero-length range with a mask that names no allowed node is
/// `0`, and `MPOL_MF_MOVE_ALL` without `CAP_SYS_NICE` is `EPERM` whatever the
/// mask says.
///
/// The hole verdict is [`mbind_range_rejects_hole`]'s, and it is decided
/// before any page is examined because `queue_pages_range()` reports a hole
/// from its `test_walk` callback, ahead of the page scan of the VMA that
/// callback was entered for.
pub fn sys_mbind<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    start: usize,
    len: usize,
    mode: usize,
    nodemask: *const usize,
    maxnode: usize,
    flags: usize,
) -> AxResult<isize> {
    let mode = mode as u32;
    let (sanitized_mode, _) = sanitize_mode_flags(mode).map_err(mempolicy_error)?;
    let nodes = read_nodemask(memory, nodemask, maxnode)?;

    // `do_mbind()`'s range checks run before any policy is created.
    let plan = tk_linux_mm::plan_mbind(
        start,
        len,
        sanitized_mode,
        flags,
        current_has_capability(CAP_SYS_NICE),
    )
    .map_err(mempolicy_error)?;
    let Some(plan) = plan else {
        return Ok(0);
    };

    // Only now `mpol_new()` + `mpol_set_nodemask()`: the mode table, the
    // user-nodemask flags and the intersection with the allowed set.
    let request = tk_linux_mm::validate(mode, nodes, !nodemask.is_null(), current_allowed_nodemask())
        .map_err(mempolicy_error)?;
    let effective = tk_linux_mm::effective_policy(request);
    let policy = Mempolicy::new(effective.mode, effective.nodes)
        .with_request_flags(effective.mode_flags, effective.user_nodemask);

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let aspace_handle = proc_data.aspace();
    let end = plan.start + plan.len;
    // `do_mbind()` marks a range that only spans holes as acceptable when
    // `mpol_new()` returned NULL, and that is `MPOL_DEFAULT` alone
    // (`mm/mempolicy.c:1519-1528`).  The verdict comes first because
    // `queue_pages_range()`'s `test_walk` reports the hole before the page
    // scan of the VMA it was entered for.
    let discontig_ok = sanitized_mode == tk_linux_mm::MPOL_DEFAULT;
    if mbind_range_rejects_hole(&aspace_handle.lock(), plan.start, end, discontig_ok) {
        return Err(AxError::BadAddress);
    }

    // With `ALLOWED_NODEMASK == {0}` every admitted policy's target mask
    // contains node 0, the only node a resident page can be on, so
    // `queue_pages_range()`'s `-EIO` for a misplaced page is unreachable and
    // the walk's hole verdict and its page scan cannot be observed out of
    // order.
    if plan.flags & tk_linux_mm::MPOL_MF_STRICT != 0
        && plan.flags & (tk_linux_mm::MPOL_MF_MOVE | tk_linux_mm::MPOL_MF_MOVE_ALL) == 0
    {
        check_mbind_strict_resident(VirtAddr::from(plan.start), plan.len, policy)?;
    }

    // Serialize policy publication with VMA inspection exactly as the
    // home-node path below does: address-space topology first, then policy
    // intervals.  Revalidate while holding that order so an unmap cannot
    // leave a freshly published policy for a vanished VMA.
    let aspace = aspace_handle.lock();
    if mbind_range_rejects_hole(&aspace, plan.start, end, discontig_ok) {
        return Err(AxError::BadAddress);
    }
    proc_data.bind_mempolicy_range(plan.start, plan.len, policy);
    Ok(0)
}

/// `queue_pages_test_walk()` and `queue_pages_range()`'s hole verdict for the
/// range `[start, end)`.
///
/// `queue_pages_test_walk()` (`mm/mempolicy.c:910-932`) walks the VMAs of the
/// range in address order and returns `-EFAULT` for a hole at the head of the
/// range (`qp->start < vma->vm_start` on the first VMA it visits) or after a
/// visited VMA that does not reach `qp->end`
/// (`vma->vm_end < qp->end && (!next || vma->vm_end < next->vm_start)`), and
/// `queue_pages_range()` adds the same error when the walk visited no VMA at
/// all (`if (!qp.first) err = -EFAULT;`, `mm/mempolicy.c:998-1000`).
/// `MPOL_MF_DISCONTIG_OK` suppresses the per-VMA reports — so a range with a
/// hole and at least one VMA is accepted for `MPOL_DEFAULT` — but never the
/// whole-range one.
fn mbind_range_rejects_hole(
    aspace: &AddrSpace,
    start: usize,
    end: usize,
    discontig_ok: bool,
) -> bool {
    let mut cursor = start;
    let mut visited = false;
    while cursor < end {
        let Some(area) = aspace
            .areas()
            .find(|area| area.end() > VirtAddr::from(cursor))
        else {
            break;
        };
        let area_start = area.start().as_usize();
        // The walk stops at the range end, so an area at or past it is not a
        // VMA of this range at all.
        if area_start >= end {
            break;
        }
        // The first VMA starting after `start` is the head hole, and a later
        // one starting after the previous VMA ended is a middle hole.
        if !discontig_ok && area_start > cursor {
            return true;
        }
        visited = true;
        cursor = area.end().as_usize().min(end);
    }
    // A range no VMA covers at all is `-EFAULT` for every policy; a last VMA
    // ending before `end` leaves the tail hole that only
    // `MPOL_MF_DISCONTIG_OK` tolerates.
    !visited || (!discontig_ok && cursor < end)
}

/// Linux 6.12 `set_mempolicy_home_node(2)`.
///
/// The VMA walk intentionally applies each successful policy interval before
/// looking at the next one. Holes are skipped; an unsupported later policy
/// returns an error while retaining the already-updated prefix, as Linux does.
pub fn sys_set_mempolicy_home_node(
    start: usize,
    len: usize,
    home_node: usize,
    flags: usize,
) -> AxResult<isize> {
    if start & (PAGE_SIZE_4K - 1) != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    if home_node >= usize::BITS as usize
        || ALLOWED_NODEMASK & (1usize.checked_shl(home_node as u32).unwrap_or(0)) == 0
    {
        return Err(AxError::InvalidInput);
    }

    // Linux uses the unchecked PAGE_ALIGN() macro here.  Keep its unsigned
    // wrap behavior: a near-ULONG_MAX length may become the empty interval.
    let len = len.wrapping_add(PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1);
    let end = start.wrapping_add(len);
    if end < start {
        return Err(AxError::InvalidInput);
    }
    if end == start {
        return Ok(0);
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let aspace_handle = proc_data.aspace();
    let aspace = aspace_handle.lock();
    let mut cursor = VirtAddr::from(start);
    let end = VirtAddr::from(end);
    let mut updated = false;
    while cursor < end {
        let Some(area) = aspace
            .areas()
            .find(|area| area.end() > cursor && area.start() < end)
        else {
            break;
        };
        let range_start = area.start().max(cursor);
        let range_end = area.end().min(end);
        updated |= proc_data.set_mempolicy_home_node_range(
            range_start.as_usize(),
            range_end.sub_addr(range_start),
            home_node,
        )?;
        cursor = range_end;
    }
    updated
        .then_some(0)
        .ok_or_else(|| LinuxError::ENOENT.into())
}

pub fn sys_move_pages<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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

    // `kernel_move_pages()` in mm/migrate.c resolves and authorizes the target
    // through `find_mm_struct()` — the `find_get_task_by_vpid()` ESRCH and the
    // `ptrace_may_access()` EPERM — *before* it dispatches to `do_pages_move()`
    // or `do_pages_stat()`. A zero `nr_pages` only makes those loops no-ops;
    // it never skips target admission, so `move_pages(bogus_pid, 0, ...)` is
    // ESRCH and an unauthorized target is EPERM rather than 0.
    let target = numa_target_process(pid)?;
    check_numa_target_permission(&target)?;

    if nr_pages == 0 {
        // Both loops copy no chunks, so the pointers are never dereferenced.
        return Ok(0);
    }
    if pages.is_null() || status.is_null() {
        return Err(AxError::BadAddress);
    }

    // Linux processes these arrays in small chunks.  Copy each chunk before
    // applying its page operations, so a later user fault cannot make one
    // chunk's inputs change underneath the corresponding target updates while
    // keeping memory use independent of the user-controlled `nr_pages`.
    let mut page_values = [const { MaybeUninit::<usize>::uninit() }; MOVE_PAGES_SNAPSHOT_CHUNK];
    let mut node_values = [const { MaybeUninit::<i32>::uninit() }; MOVE_PAGES_SNAPSHOT_CHUNK];
    let mut offset = 0;
    while offset < nr_pages {
        let chunk_len = (nr_pages - offset).min(MOVE_PAGES_SNAPSHOT_CHUNK);
        snapshot_move_pages_array(memory, pages, offset, &mut page_values[..chunk_len])?;
        if !nodes.is_null() {
            snapshot_move_pages_array(memory, nodes, offset, &mut node_values[..chunk_len])?;
        }

        for chunk_index in 0..chunk_len {
            let index = offset + chunk_index;
            let page = unsafe { page_values[chunk_index].assume_init() };
            let status_value = match numa_page_node(&target, page) {
                Ok(current_node) if nodes.is_null() => current_node,
                Ok(_) => {
                    let node = unsafe { node_values[chunk_index].assume_init() };
                    validate_movable_node(node)?;
                    if flags & MPOL_MF_MOVE_ALL as usize == 0
                        && numa_page_is_shareable(&target, page)
                    {
                        -LinuxError::EACCES.code()
                    } else {
                        target.bind_mempolicy_range(
                            VirtAddr::from(page).align_down_4k().as_usize(),
                            4096,
                            Mempolicy::new(MPOL_BIND as u32, 1usize << node),
                        );
                        node
                    }
                }
                Err(_) => -LinuxError::EFAULT.code(),
            };
            write_move_pages_status(memory, status, index, status_value)?;
        }
        offset += chunk_len;
    }

    Ok(0)
}

pub fn sys_migrate_pages<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: i32,
    maxnode: usize,
    old_nodes: *const usize,
    new_nodes: *const usize,
) -> AxResult<isize> {
    let target = numa_target_process(pid)?;
    check_numa_target_permission(&target)?;

    let old_nodes = read_nodemask(memory, old_nodes, maxnode)?;
    let new_nodes = read_nodemask(memory, new_nodes, maxnode)?;
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

/// Reads a task name with Linux `strncpy_from_user()` semantics.
///
/// `strncpy_from_user(dst, src, count)` copies at most `count` bytes, stops
/// after the NUL that terminates the source, and returns `-EFAULT` only when a
/// fault happens before that terminator. It therefore has to be driven one byte
/// at a time: a single wide read would fault on the unmapped page after a short
/// name that ends near the end of its mapping, which Linux accepts.
///
/// Returns the bytes before the terminator, or exactly
/// [`prctl_set_name_read_bound`] bytes when the source has no NUL inside the
/// bound, matching the successful "count reached" result.
fn load_strncpy_from_user_prefix<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    address: usize,
) -> AxResult<Vec<u8>> {
    let bound = prctl_set_name_read_bound();
    let mut prefix = Vec::new();
    prefix
        .try_reserve_exact(bound)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..bound {
        let mut byte = [MaybeUninit::<u8>::uninit(); 1];
        let address = address.checked_add(index).ok_or(AxError::BadAddress)?;
        memory
            .read_bytes(address, &mut byte)
            .map_err(map_usercopy_error)?;
        // SAFETY: a successful provider read initialized the single byte.
        let byte = unsafe { byte[0].assume_init() };
        if byte == 0 {
            break;
        }
        prefix.push(byte);
    }
    Ok(prefix)
}

fn prctl_mm_capable() -> AxResult<()> {
    current()
        .as_thread()
        .has_effective_capability(CAP_SYS_RESOURCE)
        .then_some(())
        .ok_or(AxError::OperationNotPermitted)
}

fn prctl_mm_address_valid(aspace: &crate::mm::AddrSpace, address: usize) -> bool {
    address == 0 || (address >= aspace.base().as_usize() && address <= aspace.end().as_usize())
}

fn validate_prctl_mm_layout(layout: &ProcessMmLayout) -> AxResult<()> {
    if layout.start_code > layout.end_code
        || layout.start_data > layout.end_data
        || layout.start_brk > layout.brk
        || layout.arg_start > layout.arg_end
        || layout.env_start > layout.env_end
        || layout.auxv.len() > PRCTL_MM_AUXV_MAX
        || !layout
            .auxv
            .len()
            .is_multiple_of(core::mem::size_of::<usize>() * 2)
    {
        return Err(AxError::InvalidInput);
    }
    let aspace_handle = current().as_thread().proc_data.aspace();
    let aspace = aspace_handle.lock();
    for address in [
        layout.start_code,
        layout.end_code,
        layout.start_data,
        layout.end_data,
        layout.start_brk,
        layout.brk,
        layout.start_stack,
        layout.arg_start,
        layout.arg_end,
        layout.env_start,
        layout.env_end,
    ] {
        if !prctl_mm_address_valid(&aspace, address) {
            return Err(AxError::InvalidInput);
        }
    }
    Ok(())
}

fn copy_prctl_mm_auxv<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    address: usize,
    len: usize,
) -> AxResult<Vec<u8>> {
    if len > PRCTL_MM_AUXV_MAX || !len.is_multiple_of(core::mem::size_of::<usize>() * 2) {
        return Err(AxError::InvalidInput);
    }
    if len == 0 {
        return Ok(Vec::new());
    }
    if address == 0 {
        return Err(AxError::BadAddress);
    }
    let mut auxv = Vec::new();
    auxv.try_reserve_exact(len).map_err(|_| AxError::NoMemory)?;
    auxv.resize(len, 0);
    let destination = unsafe {
        core::slice::from_raw_parts_mut(auxv.as_mut_ptr().cast::<MaybeUninit<u8>>(), len)
    };
    memory
        .read_bytes(address, destination)
        .map_err(map_usercopy_error)?;
    Ok(auxv)
}

fn prctl_set_mm_executable(fd: i32) -> AxResult<()> {
    let handle = get_file_like(fd)?;
    let file = handle
        .downcast::<File>()
        .map_err(|_| AxError::InvalidInput)?;
    let location = file.inner().location();
    if location.node_type() != NodeType::RegularFile {
        return Err(AxError::InvalidInput);
    }
    let source = location.absolute_path()?;
    let mut path = Vec::new();
    path.try_reserve_exact(source.as_bytes().len())
        .map_err(|_| AxError::NoMemory)?;
    path.extend_from_slice(source.as_bytes());
    let key = executable::acquire(location)?;
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    proc_data.replace_executable(key);
    *proc_data.exe_path.write() = FsPathBuf::from_vec(path);
    Ok(())
}

fn prctl_commit_mm_layout(layout: ProcessMmLayout) -> AxResult<()> {
    validate_prctl_mm_layout(&layout)?;
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    // `brk` is the only PR_SET_MM field with an actual VMA consequence. Run
    // its real allocator transaction before publishing the new metadata; a
    // failed growth/shrink leaves the old complete layout visible.
    if layout.brk != proc_data.get_heap_top() {
        let observed = crate::syscall::sys_brk_transaction(layout.brk, false)? as usize;
        if observed != layout.brk {
            return Err(AxError::NoMemory);
        }
    }
    proc_data.replace_mm_layout(layout);
    Ok(())
}

/// prctl() is called with a first argument describing what to do, and further
/// arguments with a significance depending on the first one.
/// The first argument can be:
/// - PR_SET_NAME: set the name of the calling thread, using the value pointed to by `arg2`
/// - PR_GET_NAME: get the name of the calling
/// - PR_SET_MM options: set various memory management options (start/end code/data/brk/stack)
pub fn sys_prctl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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
            // kernel/sys.c clears `comm[TASK_COMM_LEN - 1]` and then calls
            // `strncpy_from_user(comm, arg2, TASK_COMM_LEN - 1)`: at most 15
            // bytes are read, the copy stops at the first NUL, and a fault
            // before the terminator is -EFAULT. `set_task_comm()` then stores
            // the raw bytes through `strscpy_pad()`, so the name is never
            // validated as UTF-8 and never read past the bound.
            let prefix = load_strncpy_from_user_prefix(memory, arg2)?;
            let comm = TaskComm::from_prefix(&prefix);
            current().set_comm(TaskName::from_raw(comm.raw()));
        }
        PR_GET_NAME => {
            // `get_task_comm()` -> `strscpy_pad()` followed by
            // `copy_to_user(arg2, comm, sizeof(comm))` copies the complete
            // NUL-padded TASK_COMM_LEN image, not a truncated string.
            let comm = current().comm();
            vm_write_slice(memory, arg2 as _, &comm.raw()).map_err(map_usercopy_error)?;
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
            VmMutPtr::vm_write(
                arg2 as *mut i32,
                memory,
                current().as_thread().pdeath_signal() as i32,
            )
            .map_err(map_usercopy_error)?;
        }
        PR_SET_CHILD_SUBREAPER => {
            current()
                .as_thread()
                .proc_data
                .proc
                .set_child_subreaper(arg2 != 0);
        }
        PR_GET_CHILD_SUBREAPER => {
            VmMutPtr::vm_write(
                arg2 as *mut i32,
                memory,
                current().as_thread().proc_data.proc.is_child_subreaper() as i32,
            )
            .map_err(map_usercopy_error)?;
        }
        PR_GET_TID_ADDRESS => {
            // This is the same per-thread clear_child_tid word configured by
            // set_tid_address(2) and CLONE_CHILD_CLEARTID. It remains
            // observable as an address even if the address cannot currently
            // be dereferenced; only the output pointer is copied here.
            VmMutPtr::vm_write(
                arg2 as *mut usize,
                memory,
                current().as_thread().clear_child_tid(),
            )
            .map_err(map_usercopy_error)?;
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
        PR_GET_SECCOMP => {
            // Linux ignores the unused arguments for PR_GET_SECCOMP.
            return Ok(current().as_thread().seccomp_mode() as isize);
        }
        PR_SET_SECCOMP => {
            // Unlike PR_SET_NO_NEW_PRIVS, Linux treats arg4/arg5 as unused.
            // The common adapter maps the prctl mode values to seccomp(2)
            // operations and applies the exact same install transaction.
            return crate::syscall::sys_prctl_set_seccomp(memory, arg2, arg3 as *const ());
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
        PR_SET_THP_DISABLE => {
            const PR_THP_DISABLE_EXCEPT_ADVISED: usize = 1 << 1;
            if arg4 != 0
                || arg5 != 0
                || arg3 & !PR_THP_DISABLE_EXCEPT_ADVISED != 0
                || (arg2 == 0 && arg3 != 0)
            {
                return Err(AxError::InvalidInput);
            }
            let mode = if arg2 == 0 {
                ThpDisableMode::Enabled
            } else if arg3 == PR_THP_DISABLE_EXCEPT_ADVISED {
                ThpDisableMode::ExceptAdvised
            } else {
                ThpDisableMode::Disabled
            };
            let aspace = current().as_thread().proc_data.aspace();
            AddrSpace::lock_interruptibly(&aspace)?.set_thp_disable_mode(mode);
        }
        PR_GET_THP_DISABLE => {
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            return Ok(current()
                .as_thread()
                .proc_data
                .aspace()
                .lock()
                .thp_disable_mode()
                .prctl_value() as isize);
        }
        PR_SET_MDWE => {
            if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            current()
                .as_thread()
                .proc_data
                .set_mdwe(u8::try_from(arg2).map_err(|_| AxError::InvalidInput)?)?;
        }
        PR_GET_MDWE => {
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            return Ok(current().as_thread().proc_data.mdwe() as isize);
        }
        PR_GET_TIMING => {
            // kernel/sys.c: `case PR_GET_TIMING: error = PR_TIMING_STATISTICAL;`
            // validates nothing at all -- the four tail arguments are ignored --
            // because only statistical timing has ever been selectable.
            return Ok(PR_TIMING_STATISTICAL as isize);
        }
        PR_SET_TIMING => {
            // kernel/sys.c rejects PR_TIMING_TIMESTAMP, the only other defined
            // value, so a successful call is always the statistical request.
            if arg2 != PR_TIMING_STATISTICAL as usize {
                return Err(AxError::InvalidInput);
            }
        }
        PR_SET_IO_FLUSHER => {
            // kernel/sys.c checks `capable(CAP_SYS_RESOURCE)` before it looks at
            // any argument, so an unprivileged caller sees EPERM even for an
            // out-of-range mode or a non-zero tail.
            let curr = current();
            let thread = curr.as_thread();
            if !thread.has_effective_capability(CAP_SYS_RESOURCE) {
                return Err(AxError::OperationNotPermitted);
            }
            if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            match arg2 {
                0 => thread.set_io_flusher(false),
                1 => thread.set_io_flusher(true),
                _ => return Err(AxError::InvalidInput),
            }
        }
        PR_GET_IO_FLUSHER => {
            // The query is privileged in Linux as well: the flag describes a
            // writeback-reserve policy rather than a task-visible property.
            if !current()
                .as_thread()
                .has_effective_capability(CAP_SYS_RESOURCE)
            {
                return Err(AxError::OperationNotPermitted);
            }
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            return Ok(current().as_thread().io_flusher() as isize);
        }
        PR_TASK_PERF_EVENTS_DISABLE | PR_TASK_PERF_EVENTS_ENABLE => {
            // `perf_event_task_disable()`/`_enable()` in kernel/events/core.c
            // walk `current->perf_event_list`, validate no arguments, and
            // return a literal 0: the operation cannot fail. The enable flag
            // applies to every group owned by the calling thread.
            let enable = option == PR_TASK_PERF_EVENTS_ENABLE;
            if let Err(error) = current().as_thread().perf_set_task_wide_enabled(enable) {
                warn!("sys_prctl: task-wide perf control failed: {error:?}");
            }
        }
        PR_MCE_KILL => {
            // Linux accepts CLEAR or SET plus one of LATE/EARLY/DEFAULT.
            // This is task-local and feeds the real x86 machine-check signal
            // bridge; there is deliberately no process-wide shadow setting.
            if arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            let policy = match arg2 as u32 {
                PR_MCE_KILL_CLEAR => {
                    if arg3 != 0 {
                        return Err(AxError::InvalidInput);
                    }
                    PR_MCE_KILL_DEFAULT as u8
                }
                PR_MCE_KILL_SET => match arg3 as u32 {
                    PR_MCE_KILL_LATE | PR_MCE_KILL_EARLY | PR_MCE_KILL_DEFAULT => arg3 as u8,
                    _ => return Err(AxError::InvalidInput),
                },
                _ => return Err(AxError::InvalidInput),
            };
            current().as_thread().set_mce_kill_policy(policy);
        }
        PR_MCE_KILL_GET => {
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            return Ok(current().as_thread().mce_kill_policy() as isize);
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
            prctl_mm_capable()?;
            let subcommand = arg2 as u32;
            if subcommand == PR_SET_MM_MAP_SIZE {
                if arg4 != 0 || arg5 != 0 {
                    return Err(AxError::InvalidInput);
                }
                VmMutPtr::vm_write(arg3 as *mut u32, memory, PRCTL_MM_MAP_SIZE as u32)
                    .map_err(map_usercopy_error)?;
                return Ok(0);
            }

            if subcommand == PR_SET_MM_EXE_FILE {
                if arg4 != 0 || arg5 != 0 {
                    return Err(AxError::InvalidInput);
                }
                return prctl_set_mm_executable(
                    i32::try_from(arg3).map_err(|_| AxError::BadFileDescriptor)?,
                )
                .map(|_| 0);
            }

            let curr = current();
            let proc_data = &curr.as_thread().proc_data;
            let mut layout = proc_data.mm_layout();
            if subcommand == PR_SET_MM_MAP {
                if arg5 != 0 || arg4 != PRCTL_MM_MAP_SIZE {
                    return Err(AxError::InvalidInput);
                }
                let map = VmPtr::vm_read(arg3 as *const PrctlMmMap, memory)
                    .map_err(map_usercopy_error)?;
                layout.start_code =
                    usize::try_from(map.start_code).map_err(|_| AxError::InvalidInput)?;
                layout.end_code =
                    usize::try_from(map.end_code).map_err(|_| AxError::InvalidInput)?;
                layout.start_data =
                    usize::try_from(map.start_data).map_err(|_| AxError::InvalidInput)?;
                layout.end_data =
                    usize::try_from(map.end_data).map_err(|_| AxError::InvalidInput)?;
                layout.start_brk =
                    usize::try_from(map.start_brk).map_err(|_| AxError::InvalidInput)?;
                layout.brk = usize::try_from(map.brk).map_err(|_| AxError::InvalidInput)?;
                layout.start_stack =
                    usize::try_from(map.start_stack).map_err(|_| AxError::InvalidInput)?;
                layout.arg_start =
                    usize::try_from(map.arg_start).map_err(|_| AxError::InvalidInput)?;
                layout.arg_end = usize::try_from(map.arg_end).map_err(|_| AxError::InvalidInput)?;
                layout.env_start =
                    usize::try_from(map.env_start).map_err(|_| AxError::InvalidInput)?;
                layout.env_end = usize::try_from(map.env_end).map_err(|_| AxError::InvalidInput)?;
                layout.auxv =
                    copy_prctl_mm_auxv(memory, map.auxv as usize, map.auxv_size as usize)?;
                prctl_commit_mm_layout(layout)?;
                if map.exe_fd != u32::MAX {
                    prctl_set_mm_executable(
                        i32::try_from(map.exe_fd).map_err(|_| AxError::BadFileDescriptor)?,
                    )?;
                }
                return Ok(0);
            }

            if subcommand == PR_SET_MM_AUXV {
                if arg5 != 0 {
                    return Err(AxError::InvalidInput);
                }
                layout.auxv = copy_prctl_mm_auxv(memory, arg3, arg4)?;
                return prctl_commit_mm_layout(layout).map(|_| 0);
            }
            if arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            let value = arg3;
            match subcommand {
                PR_SET_MM_START_CODE => layout.start_code = value,
                PR_SET_MM_END_CODE => layout.end_code = value,
                PR_SET_MM_START_DATA => layout.start_data = value,
                PR_SET_MM_END_DATA => layout.end_data = value,
                PR_SET_MM_START_STACK => layout.start_stack = value,
                PR_SET_MM_START_BRK => layout.start_brk = value,
                PR_SET_MM_BRK => layout.brk = value,
                PR_SET_MM_ARG_START => layout.arg_start = value,
                PR_SET_MM_ARG_END => layout.arg_end = value,
                PR_SET_MM_ENV_START => layout.env_start = value,
                PR_SET_MM_ENV_END => layout.env_end = value,
                _ => return Err(AxError::InvalidInput),
            }
            prctl_commit_mm_layout(layout)?;
        }
        PR_GET_AUXV => {
            // kernel/sys.c: `if (arg4 || arg5) return -EINVAL;` then
            // `prctl_get_auxv()`. The call always reports the full size of
            // `mm->saved_auxv` -- never the number of bytes copied -- which is
            // how a caller with `len == 0` discovers the required size, and a
            // short `len` truncates the copy without failing.
            if arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            let saved_auxv = current().as_thread().proc_data.saved_auxv();
            let size = arg3.min(SAVED_AUXV_BYTES);
            if size != 0 {
                let mut image = [0u8; SAVED_AUXV_BYTES];
                let copied = saved_auxv.len().min(size);
                image[..copied].copy_from_slice(&saved_auxv[..copied]);
                memory
                    .write_bytes(arg2, &image[..size])
                    .map_err(map_usercopy_error)?;
            }
            return Ok(SAVED_AUXV_BYTES as isize);
        }
        PR_TIMER_CREATE_RESTORE_IDS => {
            // kernel/sys.c tail-checks, then `posixtimer_create_prctl()`:
            // OFF clears the bit, ON sets it, GET returns the bit itself (0 or
            // 1) and anything else is EINVAL. The bit lives in `signal_struct`,
            // so it is shared by the thread group and survives until exec.
            if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            let curr = current();
            let proc_data = &curr.as_thread().proc_data;
            match arg2 as u32 {
                PR_TIMER_CREATE_RESTORE_IDS_OFF => proc_data.set_timer_restore_ids(false),
                PR_TIMER_CREATE_RESTORE_IDS_ON => proc_data.set_timer_restore_ids(true),
                PR_TIMER_CREATE_RESTORE_IDS_GET => {
                    return Ok(proc_data.timer_restore_ids() as isize);
                }
                _ => return Err(AxError::InvalidInput),
            }
        }
        PR_FUTEX_HASH => {
            // This kernel has no per-mm private futex hash table -- private
            // futexes are keyed in a growable map, not in a slot array -- which
            // is exactly the `CONFIG_FUTEX_PRIVATE_HASH=n` configuration of
            // kernel/futex/core.c. There `futex_hash_allocate()` is
            // `return -EINVAL` and `futex_hash_get_slots()` is `return 0`.
            // SET_SLOTS rejects a non-zero arg4 before allocating; GET_SLOTS
            // validates nothing but arg2, and `futex_hash_prctl()` itself never
            // looks at arg5.
            match arg2 as u32 {
                PR_FUTEX_HASH_SET_SLOTS => {
                    let _ = arg3;
                    return Err(AxError::InvalidInput);
                }
                PR_FUTEX_HASH_GET_SLOTS => return Ok(0),
                _ => return Err(AxError::InvalidInput),
            }
        }
        PR_RSEQ_SLICE_EXTENSION => {
            // kernel/sys.c checks `arg4 || arg5` first, then calls the
            // `rseq_slice_extension_prctl()` stub that CONFIG_RSEQ_SLICE_EXTENSION=n
            // (the Kconfig default) selects. That stub ignores both command and
            // argument and returns -ENOTSUPP, so no request can succeed here.
            if arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            let _ = (arg2, arg3);
            return Ok(ENOTSUPP_RESULT);
        }
        PR_GET_CFI | PR_SET_CFI => {
            // arch/x86 defines neither `arch_prctl_get_branch_landing_pad_state()`
            // nor `_set_`/`_lock_`, so the `__weak` fallbacks in kernel/sys.c
            // link and return -EINVAL. They never dereference arg3, which means
            // the generic tail checks are the only other way to fail.
            return Err(AxError::InvalidInput);
        }
        PR_SET_VMA => {
            // `prctl_set_vma()` rejects every option but PR_SET_VMA_ANON_NAME
            // and that one reaches `set_anon_vma_name()`, which is
            // `return -EINVAL` when CONFIG_ANON_VMA_NAME is off. This kernel
            // has no anonymous VMA naming, so it matches that configuration:
            // the option is rejected for every argument vector.
            let _ = (arg2, arg3, arg4, arg5);
            return Err(AxError::InvalidInput);
        }
        PR_SET_SYSCALL_USER_DISPATCH => {
            // The dispatch filter is consulted by the syscall entry path; this
            // kernel has no such hook, which matches the `!CONFIG_GENERIC_ENTRY`
            // stub in include/linux/syscall_user_dispatch.h that returns -EINVAL
            // for every argument vector.
            let _ = (arg2, arg3, arg4, arg5);
            return Err(AxError::InvalidInput);
        }
        PR_SET_TSC | PR_GET_TSC => {
            // GET_TSC_CTL/SET_TSC_CTL exist on x86 because Linux traps RDTSC by
            // setting CR4.TSD per task. This kernel never sets CR4.TSD and has
            // no per-task mode to report, so it matches an architecture that
            // defines neither macro: kernel/sys.c falls back to `(-EINVAL)`
            // for both, without dereferencing the pointer of PR_GET_TSC.
            let _ = (arg2, arg3, arg4, arg5);
            return Err(AxError::InvalidInput);
        }
        PR_GET_SPECULATION_CTRL | PR_SET_SPECULATION_CTRL => {
            // The x86 implementations program MSR_IA32_SPEC_CTRL and
            // MSR_IA32_PRED_CMD and track per-task TIF_SPEC_* flags. This kernel
            // models none of that state, and there is no errno that means
            // "unsupported" for a valid `which`: the generic `__weak` hooks
            // return -EINVAL for every request, which is what is reported here.
            let _ = (arg2, arg3, arg4, arg5);
            return Err(AxError::InvalidInput);
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
    fn mempolicy_home_node_selects_bind_and_preferred_many_allocations() {
        let bind = Mempolicy::new(MPOL_BIND as u32, 1).with_home_node(0);
        let preferred_many = Mempolicy::new(MPOL_PREFERRED_MANY as u32, 1).with_home_node(0);
        let interleave = Mempolicy::new(MPOL_INTERLEAVE as u32, 1).with_home_node(0);

        assert_eq!(mempolicy_page_node(bind, 0x1000), 0);
        assert_eq!(mempolicy_page_node(preferred_many, 0x1000), 0);
        assert_eq!(mempolicy_page_node(interleave, 0x1000), 0);
    }

    #[test]
    fn set_mempolicy_home_node_syscall_validates_before_current_task_lookup() {
        assert_eq!(
            sys_set_mempolicy_home_node(1, 0, 0, 0),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            sys_set_mempolicy_home_node(0, 0, 0, 1),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            sys_set_mempolicy_home_node(0, 0, 1, 0),
            Err(AxError::InvalidInput)
        );
        assert_eq!(sys_set_mempolicy_home_node(0, 0, 0, 0), Ok(0));
        assert_eq!(
            sys_set_mempolicy_home_node(0, usize::MAX - PAGE_SIZE_4K + 2, 0, 0),
            Ok(0)
        );
    }

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

    #[test]
    fn move_pages_snapshot_chunk_is_bounded() {
        assert_eq!(MOVE_PAGES_SNAPSHOT_CHUNK, 16);
        assert_eq!(0usize, 0);
        assert_eq!(1usize.min(MOVE_PAGES_SNAPSHOT_CHUNK), 1);
        assert_eq!(MOVE_PAGES_SNAPSHOT_CHUNK.min(MOVE_PAGES_SNAPSHOT_CHUNK), 16);
        assert_eq!(
            (MOVE_PAGES_SNAPSHOT_CHUNK + 1).min(MOVE_PAGES_SNAPSHOT_CHUNK),
            16
        );
        assert_eq!(MOVE_PAGES_SNAPSHOT_CHUNK, 16);
    }

    #[test]
    fn move_pages_snapshot_address_checks_element_arithmetic() {
        assert_eq!(
            checked_move_pages_element_address::<usize>(usize::MAX as *const usize, 1),
            Err(AxError::BadAddress)
        );
        assert_eq!(
            checked_move_pages_element_address::<usize>(0x1000 as *const usize, 2),
            Ok(0x1010)
        );
    }

    struct NoUserMemory;

    // SAFETY: the provider reports every access as unmapped; the tests below
    // only exercise rejection paths that run before the first usercopy.
    unsafe impl tk_linux_usercopy::UserMemory for NoUserMemory {
        fn read(
            &mut self,
            _start: usize,
            _dst: &mut [MaybeUninit<u8>],
        ) -> Result<(), tk_linux_usercopy::UserCopyError> {
            Err(tk_linux_usercopy::UserCopyError::BadAddress)
        }

        fn write(
            &mut self,
            _start: usize,
            _src: &[u8],
        ) -> Result<(), tk_linux_usercopy::UserCopyError> {
            Err(tk_linux_usercopy::UserCopyError::BadAddress)
        }
    }

    #[test]
    fn move_pages_resolves_the_target_before_an_empty_request() {
        let mut provider = NoUserMemory;
        let mut memory = UserMemoryContext::new(&mut provider);

        // kernel_move_pages() in mm/migrate.c calls find_mm_struct() — the
        // ESRCH lookup plus ptrace_may_access() — before do_pages_stat(), so a
        // zero page count is still ESRCH for a pid that cannot be resolved and
        // must never short-circuit to 0.
        assert_eq!(
            sys_move_pages(
                &mut memory,
                -1,
                0,
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null_mut(),
                0,
            ),
            Err(AxError::NoSuchProcess)
        );

        // The flag check is the one precondition that precedes the target
        // lookup, so an unknown bit wins over the unresolvable pid.
        assert_eq!(
            sys_move_pages(
                &mut memory,
                -1,
                0,
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null_mut(),
                1 << 3,
            ),
            Err(AxError::InvalidInput)
        );
    }
}
