use alloc::{sync::Arc, vec, vec::Vec};
use core::mem::MaybeUninit;

use axerrno::{AxError, AxResult};
use axhal::paging::MappingFlags;
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{
    CAP_SYS_NICE, CAP_SYS_PTRACE, MADV_COLD, MADV_COLLAPSE, MADV_PAGEOUT, MADV_WILLNEED,
};
use memory_addr::{MemoryAddr, VirtAddr};
use starry_vm::{VmPtr, vm_read_slice, vm_write_slice};

use crate::{
    file::{FileLike, PidFd},
    mm::{AddrSpace, IoVec},
    task::{AsThread, ProcessData, get_process_data},
};

const PROCESS_VM_MAX_IOV: usize = 1024;
const PROCESS_VM_COPY_CHUNK: usize = 16 * 1024;

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

fn read_iovecs(iovs: *const IoVec, iovcnt: usize) -> AxResult<(Vec<UserIoVec>, usize)> {
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
    let mut total = 0usize;
    for index in 0..iovcnt {
        let iov = iovs.wrapping_add(index).vm_read()?;
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

fn check_process_vm_permission(target: &ProcessData) -> AxResult<()> {
    let curr = current();
    let actor = &curr.as_thread().proc_data;
    if actor.proc.pid() == target.proc.pid()
        || actor.euid() == 0
        || actor.has_effective_capability(CAP_SYS_PTRACE)
        || [actor.uid(), actor.euid()]
            .into_iter()
            .any(|id| id == target.uid() || id == target.euid() || id == target.suid())
    {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn check_process_madvise_permission(target: &ProcessData) -> AxResult<()> {
    check_process_vm_permission(target)?;
    let curr = current();
    if curr
        .as_thread()
        .proc_data
        .has_effective_capability(CAP_SYS_NICE)
    {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn validate_process_madvise_behavior(behavior: u32) -> AxResult<()> {
    match behavior {
        MADV_COLD | MADV_PAGEOUT | MADV_WILLNEED | MADV_COLLAPSE => Ok(()),
        _ => Err(AxError::InvalidInput),
    }
}

fn validate_remote_range(
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
    if !aspace.can_access_range(start, len, access_flags) {
        return Err(AxError::BadAddress);
    }
    let page_start = start.align_down_4k();
    let page_end = end.align_up_4k();
    aspace.populate_area(page_start, page_end.sub_addr(page_start), access_flags)?;
    Ok(())
}

fn validate_remote_iovecs(target: &ProcessData, remote: &[UserIoVec]) -> AxResult<()> {
    let aspace_handle = target.aspace();
    let mut aspace = aspace_handle.lock();
    for iov in remote {
        validate_remote_range(&mut aspace, iov.base, iov.len, MappingFlags::READ)?;
    }
    Ok(())
}

fn copy_from_remote(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    remote: usize,
    local: usize,
    len: usize,
    scratch: &mut [u8],
) -> AxResult<()> {
    let mut copied = 0usize;
    while copied < len {
        let chunk = (len - copied).min(scratch.len());
        {
            let mut aspace = aspace_handle.lock();
            validate_remote_range(&mut aspace, remote + copied, chunk, MappingFlags::READ)?;
            aspace.read(VirtAddr::from(remote + copied), &mut scratch[..chunk])?;
        }
        vm_write_slice((local + copied) as *mut u8, &scratch[..chunk])?;
        copied += chunk;
    }
    Ok(())
}

fn copy_to_remote(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    local: usize,
    remote: usize,
    len: usize,
    scratch: &mut [u8],
) -> AxResult<()> {
    let mut copied = 0usize;
    while copied < len {
        let chunk = (len - copied).min(scratch.len());
        let buf = unsafe {
            core::slice::from_raw_parts_mut(scratch.as_mut_ptr().cast::<MaybeUninit<u8>>(), chunk)
        };
        vm_read_slice((local + copied) as *const u8, buf)?;
        {
            let mut aspace = aspace_handle.lock();
            validate_remote_range(&mut aspace, remote + copied, chunk, MappingFlags::WRITE)?;
            aspace.write(VirtAddr::from(remote + copied), &scratch[..chunk])?;
        }
        copied += chunk;
    }
    Ok(())
}

fn process_vm_copy(
    target: &ProcessData,
    local: &[UserIoVec],
    remote: &[UserIoVec],
    max_len: usize,
    op: ProcessVmOp,
) -> AxResult<isize> {
    if max_len == 0 {
        return Ok(0);
    }

    let aspace_handle = target.aspace();
    let mut scratch = vec![0u8; PROCESS_VM_COPY_CHUNK.min(max_len)];
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
        let local_addr = local[local_index]
            .base
            .checked_add(local_offset)
            .ok_or(AxError::BadAddress)?;
        let remote_addr = remote[remote_index]
            .base
            .checked_add(remote_offset)
            .ok_or(AxError::BadAddress)?;

        let copy_result = match op {
            ProcessVmOp::ReadRemote => copy_from_remote(
                &aspace_handle,
                remote_addr,
                local_addr,
                copy_len,
                &mut scratch,
            ),
            ProcessVmOp::WriteRemote => copy_to_remote(
                &aspace_handle,
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

    let (local, local_len) = read_iovecs(local_iov, local_iovcnt)?;
    if local_len == 0 {
        return Ok(0);
    }
    let (remote, remote_len) = read_iovecs(remote_iov, remote_iovcnt)?;
    let copy_len = local_len.min(remote_len);
    if copy_len == 0 {
        return Ok(0);
    }
    if pid < 0 {
        return Err(AxError::NoSuchProcess);
    }

    let target = get_process_data(pid as u32)?;
    check_process_vm_permission(&target)?;
    process_vm_copy(&target, &local, &remote, copy_len, op)
}

pub fn sys_process_vm_readv(
    pid: i32,
    local_iov: *const IoVec,
    local_iovcnt: usize,
    remote_iov: *const IoVec,
    remote_iovcnt: usize,
    flags: usize,
) -> AxResult<isize> {
    sys_process_vm_rw(
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
    pid: i32,
    local_iov: *const IoVec,
    local_iovcnt: usize,
    remote_iov: *const IoVec,
    remote_iovcnt: usize,
    flags: usize,
) -> AxResult<isize> {
    sys_process_vm_rw(
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
    validate_process_madvise_behavior(behavior)?;

    let (remote, total_len) = read_iovecs(iovs, iovcnt)?;
    if total_len == 0 {
        return Ok(0);
    }

    let target = PidFd::from_fd(pidfd)?.process_data()?;
    check_process_madvise_permission(&target)?;
    validate_remote_iovecs(&target, &remote)?;
    Ok(total_len as isize)
}
