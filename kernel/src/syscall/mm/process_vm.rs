use alloc::{sync::Arc, vec::Vec};
use core::mem::MaybeUninit;

use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::MappingFlags;
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{CAP_SYS_NICE, MADV_COLD, MADV_COLLAPSE, MADV_PAGEOUT, MADV_WILLNEED};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};

use crate::{
    file::{FileLike, PidFd},
    mm::{AddrSpace, IoVec, UserMemoryCapability, checked_align_up_4k, map_usercopy_error},
    task::{
        AsThread, PtraceAccessMode, check_current_ptrace_image_snapshot,
        check_current_thread_ptrace_image_access, get_process_data, get_visible_task,
        process_domain, process_error,
    },
};

const PROCESS_VM_MAX_IOV: usize = 1024;
const PROCESS_VM_COPY_CHUNK: usize = 16 * 1024;
// Linux bounds vector I/O by MAX_RW_COUNT even on a 64-bit task.  Keeping the
// cap below isize::MAX also makes the syscall's positive byte result lossless.
const MAX_RW_COUNT: usize = 0x7fff_f000;

#[derive(Clone, Copy)]
struct UserIoVec {
    base: usize,
    len: usize,
}

#[derive(Clone, Copy)]
enum ProcessVmOp {
    ReadRemote,
    WriteRemote,
}

fn read_iovecs(
    caller: &UserMemoryCapability,
    iovs: *const IoVec,
    iovcnt: usize,
) -> AxResult<(Vec<UserIoVec>, usize)> {
    if iovcnt > PROCESS_VM_MAX_IOV {
        return Err(AxError::InvalidInput);
    }
    if iovcnt == 0 {
        return Ok((Vec::new(), 0));
    }
    if iovs.is_null() {
        return Err(AxError::BadAddress);
    }

    let mut result = Vec::new();
    result
        .try_reserve_exact(iovcnt)
        .map_err(|_| AxError::NoMemory)?;
    let mut total = 0usize;
    for index in 0..iovcnt {
        let offset = index
            .checked_mul(core::mem::size_of::<IoVec>())
            .ok_or(AxError::BadAddress)?;
        let address = (iovs as usize)
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        let iov = caller
            .read_value(address as *const IoVec)
            .map_err(map_usercopy_error)?;
        if iov.iov_len < 0 {
            return Err(AxError::InvalidInput);
        }
        let len = iov.iov_len as usize;
        total = total.checked_add(len).ok_or(AxError::InvalidInput)?;
        result.push(UserIoVec {
            base: iov.iov_base as usize,
            len,
        });
    }
    Ok((result, total))
}

/// Imports a remote iovec array under process_madvise's aggregate byte cap.
///
/// Do the arithmetic checks while copyin is still the only operation: a bad
/// user descriptor must win over pidfd lookup, ptrace, or advice validation.
/// Linux truncates the request at MAX_RW_COUNT rather than allowing the
/// eventual signed return value to overflow.
fn read_process_madvise_iovecs(
    caller: &UserMemoryCapability,
    iovs: *const IoVec,
    iovcnt: usize,
) -> AxResult<(Vec<UserIoVec>, usize)> {
    let (mut iovecs, _) = read_iovecs(caller, iovs, iovcnt)?;
    let mut remaining = MAX_RW_COUNT;
    let mut total = 0usize;
    for iov in &mut iovecs {
        if iov.len != 0 {
            iov.base.checked_add(iov.len).ok_or(AxError::BadAddress)?;
        }
        let accepted = iov.len.min(remaining);
        iov.len = accepted;
        total += accepted;
        remaining -= accepted;
    }
    Ok((iovecs, total))
}

fn check_process_madvise_capability() -> AxResult<()> {
    let curr = current();
    if curr.as_thread().has_effective_capability(CAP_SYS_NICE) {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn check_process_madvise_capability_if_remote(
    caller_aspace: &Arc<Mutex<AddrSpace>>,
    target_aspace: &Arc<Mutex<AddrSpace>>,
) -> AxResult<()> {
    if !process_madvise_is_remote(caller_aspace, target_aspace) {
        Ok(())
    } else {
        check_process_madvise_capability()
    }
}

#[inline]
fn process_madvise_is_remote(
    caller_aspace: &Arc<Mutex<AddrSpace>>,
    target_aspace: &Arc<Mutex<AddrSpace>>,
) -> bool {
    !Arc::ptr_eq(caller_aspace, target_aspace)
}

fn validate_process_madvise_behavior(behavior: u32) -> AxResult<()> {
    match behavior {
        MADV_WILLNEED | MADV_COLD | MADV_PAGEOUT | MADV_COLLAPSE => Ok(()),
        _ => Err(AxError::InvalidInput),
    }
}

/// `find_lock_task_mm()` does not treat a retained zombie ProcessData as an
/// mm owner. Resolve one still-published thread in the exact process group;
/// final exit removes every such thread before the zombie payload survives.
fn process_mrelease_has_live_mm_thread(target: &crate::task::ProcessData) -> bool {
    target.proc.thread_ids().any(|tid| {
        get_visible_task(tid)
            .ok()
            .is_some_and(|task| core::ptr::eq(&*task.as_thread().proc_data, target))
    })
}

fn validate_address_range(
    aspace: &mut AddrSpace,
    base: usize,
    len: usize,
    access_flags: MappingFlags,
) -> AxResult<()> {
    if len == 0 {
        return Ok(());
    }
    let start = VirtAddr::from(base);
    let end = start.checked_add(len).ok_or(AxError::BadAddress)?;
    if !aspace.contains_range(start, len) {
        return Err(AxError::BadAddress);
    }
    // process_vm is implemented with AddrSpace::{read,write}; those use the
    // direct map and must neither populate nor alias secret frames.
    if aspace.has_secret_mapping(start, len) {
        return Err(AxError::BadAddress);
    }
    if !aspace.can_access_range(start, len, access_flags) {
        return Err(AxError::BadAddress);
    }
    let page_start = start.align_down_4k();
    let page_end = VirtAddr::from(checked_align_up_4k(end.as_usize()).ok_or(AxError::BadAddress)?);
    match aspace.populate_area(page_start, page_end.sub_addr(page_start), access_flags) {
        Ok(()) => {}
        // The VMA and permissions were checked above. Preserve an allocation
        // failure for a valid range instead of collapsing it into EFAULT.
        Err(AxError::NoMemory) => return Err(AxError::NoMemory),
        // Address/permission failures are user-range faults; only an
        // allocation failure for an otherwise valid VMA remains ENOMEM.
        Err(AxError::BadAddress | AxError::InvalidInput | AxError::PermissionDenied) => {
            return Err(AxError::BadAddress);
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[inline]
fn page_copy_len(address: usize, len: usize) -> usize {
    let page_offset = address & (PAGE_SIZE_4K - 1);
    len.min(PAGE_SIZE_4K - page_offset)
}

fn validate_remote_iovecs(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    remote: &[UserIoVec],
) -> AxResult<()> {
    let mut aspace = aspace_handle.lock();
    for iov in remote {
        validate_address_range(&mut aspace, iov.base, iov.len, MappingFlags::READ)?;
    }
    Ok(())
}

fn copy_from_remote(
    caller: &UserMemoryCapability,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    remote: usize,
    local: usize,
    len: usize,
    scratch: &mut [u8],
) -> AxResult<()> {
    let mut copied = 0usize;
    while copied < len {
        let remote_addr = remote.checked_add(copied).ok_or(AxError::BadAddress)?;
        let local_addr = local.checked_add(copied).ok_or(AxError::BadAddress)?;
        let chunk = (len - copied)
            .min(scratch.len())
            .min(page_copy_len(remote_addr, len - copied))
            .min(page_copy_len(local_addr, len - copied));
        debug_assert!(chunk != 0);
        {
            let mut aspace = aspace_handle.lock();
            validate_address_range(&mut aspace, remote_addr, chunk, MappingFlags::READ)?;
            aspace.read(VirtAddr::from(remote_addr), &mut scratch[..chunk])?;
        }
        caller
            .write_bytes(local_addr, &scratch[..chunk])
            .map_err(map_usercopy_error)?;
        copied += chunk;
    }
    Ok(())
}

fn copy_to_remote(
    caller: &UserMemoryCapability,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    local: usize,
    remote: usize,
    len: usize,
    scratch: &mut [u8],
) -> AxResult<()> {
    let mut copied = 0usize;
    while copied < len {
        let local_addr = local.checked_add(copied).ok_or(AxError::BadAddress)?;
        let remote_addr = remote.checked_add(copied).ok_or(AxError::BadAddress)?;
        let chunk = (len - copied)
            .min(scratch.len())
            .min(page_copy_len(local_addr, len - copied))
            .min(page_copy_len(remote_addr, len - copied));
        debug_assert!(chunk != 0);
        let buf = unsafe {
            core::slice::from_raw_parts_mut(scratch.as_mut_ptr().cast::<MaybeUninit<u8>>(), chunk)
        };
        caller
            .read_bytes(local_addr, buf)
            .map_err(map_usercopy_error)?;
        {
            let mut aspace = aspace_handle.lock();
            validate_address_range(&mut aspace, remote_addr, chunk, MappingFlags::WRITE)?;
            aspace.write(VirtAddr::from(remote_addr), &scratch[..chunk])?;
        }
        copied += chunk;
    }
    Ok(())
}

fn process_vm_copy(
    caller: &UserMemoryCapability,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    local: &[UserIoVec],
    remote: &[UserIoVec],
    max_len: usize,
    op: ProcessVmOp,
) -> AxResult<isize> {
    if max_len == 0 {
        return Ok(0);
    }

    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(PROCESS_VM_COPY_CHUNK.min(max_len))
        .map_err(|_| AxError::NoMemory)?;
    scratch.resize(PROCESS_VM_COPY_CHUNK.min(max_len), 0);
    let mut local_index = 0usize;
    let mut remote_index = 0usize;
    let mut local_offset = 0usize;
    let mut remote_offset = 0usize;
    let mut copied_total = 0usize;

    while copied_total < max_len && local_index < local.len() && remote_index < remote.len() {
        while local_index < local.len() && local[local_index].len == local_offset {
            local_index += 1;
            local_offset = 0;
        }
        while remote_index < remote.len() && remote[remote_index].len == remote_offset {
            remote_index += 1;
            remote_offset = 0;
        }
        if local_index >= local.len() || remote_index >= remote.len() {
            break;
        }

        let copy_len = (max_len - copied_total)
            .min(local[local_index].len - local_offset)
            .min(remote[remote_index].len - remote_offset)
            .min(scratch.len());
        let Some(local_addr) = local[local_index].base.checked_add(local_offset) else {
            return if copied_total == 0 {
                Err(AxError::BadAddress)
            } else {
                Ok(copied_total as isize)
            };
        };
        let Some(remote_addr) = remote[remote_index].base.checked_add(remote_offset) else {
            return if copied_total == 0 {
                Err(AxError::BadAddress)
            } else {
                Ok(copied_total as isize)
            };
        };
        let copy_len = copy_len
            .min(page_copy_len(local_addr, copy_len))
            .min(page_copy_len(remote_addr, copy_len));
        debug_assert!(copy_len != 0);

        // Validate the caller-owned destination/source before touching the
        // target image. This keeps a local fault from causing a remote access
        // and preserves Linux's positive-prefix result after prior chunks.
        let local_result = {
            let mut caller_aspace = caller.address_space().lock();
            let access_flags = match op {
                ProcessVmOp::ReadRemote => MappingFlags::WRITE,
                ProcessVmOp::WriteRemote => MappingFlags::READ,
            };
            validate_address_range(&mut caller_aspace, local_addr, copy_len, access_flags)
        };
        if let Err(err) = local_result {
            return if copied_total == 0 {
                Err(err)
            } else {
                Ok(copied_total as isize)
            };
        }

        let copy_result = match op {
            ProcessVmOp::ReadRemote => copy_from_remote(
                caller,
                aspace_handle,
                remote_addr,
                local_addr,
                copy_len,
                &mut scratch,
            ),
            ProcessVmOp::WriteRemote => copy_to_remote(
                caller,
                aspace_handle,
                local_addr,
                remote_addr,
                copy_len,
                &mut scratch,
            ),
        };
        if let Err(err) = copy_result {
            return if copied_total == 0 {
                Err(err)
            } else {
                Ok(copied_total as isize)
            };
        }

        copied_total += copy_len;
        local_offset += copy_len;
        remote_offset += copy_len;
    }

    Ok(copied_total as isize)
}

fn sys_process_vm_rw(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    pid: i32,
    local_iov: *const IoVec,
    local_iovcnt: usize,
    remote_iov: *const IoVec,
    remote_iovcnt: usize,
    flags: usize,
    op: ProcessVmOp,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let caller = UserMemoryCapability::new(caller_aspace);
    let (local, local_len) = read_iovecs(&caller, local_iov, local_iovcnt)?;
    if local_len == 0 {
        return Ok(0);
    }
    let (remote, remote_len) = read_iovecs(&caller, remote_iov, remote_iovcnt)?;
    let copy_len = local_len.min(remote_len);
    if copy_len == 0 {
        return Ok(0);
    }
    if pid < 0 {
        return Err(AxError::NoSuchProcess);
    }

    let target_task = get_visible_task(pid as u32)?;
    let target_thread = target_task.as_thread();
    let target_image =
        check_current_thread_ptrace_image_access(target_thread, PtraceAccessMode::AttachReal)?;
    let target_aspace = target_image.into_aspace();
    process_vm_copy(
        &caller,
        &target_aspace,
        &local,
        &remote,
        copy_len,
        op,
    )
}

pub fn sys_process_vm_readv(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    pid: i32,
    local_iov: *const IoVec,
    local_iovcnt: usize,
    remote_iov: *const IoVec,
    remote_iovcnt: usize,
    flags: usize,
) -> AxResult<isize> {
    sys_process_vm_rw(
        caller_aspace,
        pid,
        local_iov,
        local_iovcnt,
        remote_iov,
        remote_iovcnt,
        flags,
        ProcessVmOp::ReadRemote,
    )
}

pub fn sys_process_vm_writev(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    pid: i32,
    local_iov: *const IoVec,
    local_iovcnt: usize,
    remote_iov: *const IoVec,
    remote_iovcnt: usize,
    flags: usize,
) -> AxResult<isize> {
    sys_process_vm_rw(
        caller_aspace,
        pid,
        local_iov,
        local_iovcnt,
        remote_iov,
        remote_iovcnt,
        flags,
        ProcessVmOp::WriteRemote,
    )
}

pub fn sys_process_madvise(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    pidfd: i32,
    iovs: *const IoVec,
    iovcnt: usize,
    behavior: u32,
    flags: u32,
) -> AxResult<isize> {
    debug!(
        "sys_process_madvise <= pidfd: {pidfd}, iovs: {iovs:?}, iovcnt: {iovcnt}, behavior: \
         {behavior:#x}, flags: {flags:#x}"
    );

    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let caller = UserMemoryCapability::new(caller_aspace);
    let (remote, total_len) = read_process_madvise_iovecs(&caller, iovs, iovcnt)?;

    let pidfd = PidFd::from_fd(pidfd)?;
    let target = pidfd.process_data()?;
    let target_image = pidfd.image_access_snapshot()?;
    validate_process_madvise_behavior(behavior)?;
    check_current_ptrace_image_snapshot(&target, &target_image, PtraceAccessMode::ReadFs)?;
    check_process_madvise_capability_if_remote(caller.address_space(), target_image.aspace())?;
    let target_aspace = target_image.into_aspace();
    let mut pageout_work = Vec::new();
    let mut completed = 0usize;
    let mut terminal_error = None;
    for iov in remote.into_iter().filter(|iov| iov.len != 0) {
        let result = match behavior {
            // COLLAPSE may transact every shared alias.  It owns its lock
            // acquisition so it can order every participating mm by ID.
            MADV_COLLAPSE => crate::syscall::mm::mmap::process_madvise_collapse(
                &target_aspace,
                iov.base,
                iov.len,
            ),
            MADV_COLD | MADV_PAGEOUT => {
                crate::syscall::mm::mmap::ensure_4k_granularity_across_aliases(
                    &target_aspace,
                    VirtAddr::from(iov.base),
                    iov.len,
                )
                .and_then(|_| {
                    let mut aspace = target_aspace.lock();
                    match behavior {
                        MADV_WILLNEED => crate::syscall::mm::mmap::process_madvise_willneed(
                            &mut aspace, iov.base, iov.len,
                        ),
                        MADV_COLD => crate::syscall::mm::mmap::process_madvise_cold(
                            &mut aspace, iov.base, iov.len,
                        ),
                        MADV_PAGEOUT => crate::syscall::mm::mmap::process_madvise_collect_pageout(
                            &mut aspace,
                            iov.base,
                            iov.len,
                            &mut pageout_work,
                        ),
                        _ => unreachable!("behavior was validated before ptrace access"),
                    }
                })
            }
            MADV_WILLNEED => {
                let mut aspace = target_aspace.lock();
                crate::syscall::mm::mmap::process_madvise_willneed(&mut aspace, iov.base, iov.len)
            }
            _ => unreachable!("behavior was validated before ptrace access"),
        };
        match result {
            Ok(()) => {
                completed = completed
                    .checked_add(iov.len)
                    .ok_or(AxError::InvalidInput)?
            }
            Err(error) => {
                terminal_error = Some(error);
                break;
            }
        }
    }
    for (backend, range) in pageout_work {
        // PAGEOUT is advisory: per-page cache writeback/eviction failure
        // preserves that page and does not retract already planned effects.
        let _ = backend.pageout_file_pages(range);
    }
    if let Some(error) = terminal_error {
        return if completed == 0 {
            Err(error)
        } else {
            Ok(completed as isize)
        };
    }
    debug_assert_eq!(completed, total_len);
    Ok(completed as isize)
}

/// Linux 6.12 `process_mrelease(2)`: synchronously run the OOM-reaper portion
/// for an exiting pidfd target.  This never tears down VMAs themselves; it
/// drains only reclaimable private COW PTEs/backing and preserves the mm for
/// the normal exit/reap lifecycle.
pub fn sys_process_mrelease(pidfd: i32, flags: u32) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    // Preserve pidfd_get_task-style descriptor/type/liveness errors exactly.
    let pidfd = PidFd::from_fd(pidfd)?;
    let target = pidfd.process_data()?;
    if !target.oom_reap_eligible() {
        return Err(AxError::InvalidInput);
    }
    if !process_mrelease_has_live_mm_thread(&target) {
        return Err(AxError::NoSuchProcess);
    }

    let aspace = target.aspace();
    // `mm_users` in Linux includes other CLONE_VM process groups.  Snapshot
    // the process domain before claiming teardown and refuse a live sharer;
    // every remaining sharer has already crossed its permanent exit gate, so
    // none can publish a new CLONE_VM child after this point.
    for process in process_domain()?
        .registry()
        .try_processes()
        .map_err(process_error)?
    {
        if core::ptr::eq(&*process, &*target.proc) {
            continue;
        }
        let Ok(sharer) = get_process_data(process.pid()) else {
            continue;
        };
        if Arc::ptr_eq(&aspace, &sharer.aspace()) && !sharer.oom_reap_eligible() {
            return Err(AxError::InvalidInput);
        }
    }

    // Match mmap_read_lock_killable(): contention waits, while a deliverable
    // signal during that wait yields EINTR rather than a spurious EAGAIN.
    let mut aspace = AddrSpace::lock_interruptibly(&aspace)?;
    let claimed = aspace.begin_oom_reap().map_err(|error| match error {
        AxError::ResourceBusy => LinuxError::EAGAIN.into(),
        _ => error,
    })?;
    debug_assert!(claimed, "the mmap lock serializes OOM reaper ownership");
    let completed = aspace.oom_reap_private_pages();
    aspace.finish_oom_reap();
    drop(aspace);
    if completed {
        Ok(0)
    } else {
        Err(LinuxError::EAGAIN.into())
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axerrno::AxError;
    use axhal::paging::{MappingFlags, PageSize};
    use axsync::Mutex;
    use linux_raw_sys::general::{MADV_COLD, MADV_COLLAPSE, MADV_PAGEOUT, MADV_WILLNEED};
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::{
        IoVec, ProcessVmOp, UserIoVec, UserMemoryCapability, page_copy_len,
        process_madvise_is_remote, process_vm_copy, read_iovecs, validate_process_madvise_behavior,
    };
    use crate::mm::{AddrSpace, Backend};

    fn mapped_aspace(base: usize, mapped_pages: usize) -> Arc<Mutex<AddrSpace>> {
        let mut aspace = AddrSpace::new_empty(VirtAddr::from(base), PAGE_SIZE_4K * 2).unwrap();
        for page in 0..mapped_pages {
            let address = base + page * PAGE_SIZE_4K;
            aspace
                .map(
                    VirtAddr::from(address),
                    PAGE_SIZE_4K,
                    MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                    false,
                    Backend::new_alloc(VirtAddr::from(address), PageSize::Size4K),
                )
                .unwrap();
        }
        Arc::new(Mutex::new(aspace))
    }

    fn mapped_capability() -> UserMemoryCapability {
        UserMemoryCapability::new(mapped_aspace(0x1000, 1))
    }

    #[test]
    fn process_vm_iov_descriptors_are_imported_from_explicit_caller_space() {
        let capability = mapped_capability();
        let descriptor = IoVec {
            iov_base: 0x1000 as *mut u8,
            iov_len: 37,
        };
        // SAFETY: IoVec is a complete initialized descriptor and the selected
        // capability owns the mapped destination range.
        unsafe {
            capability
                .write_value_unchecked(0x1000 as *mut IoVec, descriptor)
                .unwrap();
        }

        let (iovecs, total) = read_iovecs(&capability, 0x1000 as *const IoVec, 1).unwrap();
        assert_eq!(total, 37);
        assert_eq!(iovecs[0].base, 0x1000);
        assert_eq!(iovecs[0].len, 37);
    }

    #[test]
    fn process_vm_descriptor_range_is_faulted_before_element_copyin() {
        let capability = mapped_capability();
        assert!(matches!(
            read_iovecs(&capability, 0x1ff8 as *const IoVec, 1),
            Err(AxError::BadAddress)
        ));
    }

    #[test]
    fn process_vm_copy_batches_stop_at_page_boundaries() {
        assert_eq!(page_copy_len(0x1000, PAGE_SIZE_4K * 4), PAGE_SIZE_4K);
        assert_eq!(page_copy_len(0x1fff, PAGE_SIZE_4K), 1);
        assert_eq!(page_copy_len(0x2000, 37), 37);
    }

    #[test]
    fn process_vm_copy_returns_prefix_when_remote_second_page_faults() {
        let caller_aspace = mapped_aspace(0x1000, 2);
        let target_aspace = mapped_aspace(0x4000, 1);
        let caller = UserMemoryCapability::new(caller_aspace);
        let local = [UserIoVec {
            base: 0x1000,
            len: PAGE_SIZE_4K * 2,
        }];
        let remote = [UserIoVec {
            base: 0x4000,
            len: PAGE_SIZE_4K * 2,
        }];

        assert_eq!(
            process_vm_copy(
                &caller,
                &target_aspace,
                &local,
                &remote,
                PAGE_SIZE_4K * 2,
                ProcessVmOp::ReadRemote,
            )
            .unwrap(),
            PAGE_SIZE_4K as isize
        );

        assert_eq!(
            process_vm_copy(
                &caller,
                &target_aspace,
                &local,
                &remote,
                PAGE_SIZE_4K * 2,
                ProcessVmOp::WriteRemote,
            )
            .unwrap(),
            PAGE_SIZE_4K as isize
        );
    }

    #[test]
    fn process_vm_copy_reports_efault_before_the_first_byte() {
        let caller = UserMemoryCapability::new(mapped_aspace(0x1000, 0));
        let target = mapped_aspace(0x4000, 1);
        let local = [UserIoVec {
            base: 0x1000,
            len: PAGE_SIZE_4K,
        }];
        let remote = [UserIoVec {
            base: 0x4000,
            len: PAGE_SIZE_4K,
        }];

        assert_eq!(
            process_vm_copy(
                &caller,
                &target,
                &local,
                &remote,
                PAGE_SIZE_4K,
                ProcessVmOp::ReadRemote,
            ),
            Err(AxError::BadAddress)
        );
    }

    #[test]
    fn process_madvise_capability_gate_distinguishes_same_mm() {
        let caller = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K).unwrap(),
        ));
        let remote = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x4000), PAGE_SIZE_4K).unwrap(),
        ));

        assert!(!process_madvise_is_remote(&caller, &caller));
        assert!(process_madvise_is_remote(&caller, &remote));
    }

    #[test]
    fn process_madvise_accepts_implemented_behavior() {
        for behavior in [MADV_WILLNEED, MADV_COLD, MADV_PAGEOUT, MADV_COLLAPSE] {
            assert_eq!(validate_process_madvise_behavior(behavior), Ok(()));
        }
        assert_eq!(
            validate_process_madvise_behavior(u32::MAX),
            Err(AxError::InvalidInput)
        );
    }
}
