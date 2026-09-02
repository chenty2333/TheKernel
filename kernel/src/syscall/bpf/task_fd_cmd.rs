//! Cross-task BPF/perf descriptor introspection.

use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult, LinuxError};
use linux_raw_sys::general::CAP_SYS_ADMIN;
use thekernel_linux_bpf::{BPF_FD_TYPE_RAW_TRACEPOINT, BpfAttrTaskFdQuery};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::{
    bpf::{read_bpf_attr, require_bpf_attr_range, write_bpf_attr_value, write_user_bytes},
    file::{bpf::BpfRawTracepointLink, perf::PerfEventFile},
    task::{AsThread, get_task},
};

struct TaskFdQueryResult<'a> {
    prog_id: u32,
    fd_type: u32,
    name: Option<&'a [u8]>,
    probe_offset: u64,
    probe_addr: u64,
}

/// Implements Linux's NUL-terminating `bpf_copy_to_user()` convention.
/// `Ok(true)` means a truncated, NUL-terminated name was published and the
/// caller must return `ENOSPC` after publishing the scalar output fields.
fn copy_query_name<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pointer: u64,
    capacity: u32,
    name: Option<&[u8]>,
) -> AxResult<bool> {
    if capacity == 0 || pointer == 0 {
        return Ok(false);
    }
    let pointer = usize::try_from(pointer).map_err(|_| AxError::BadAddress)?;
    let name = name.unwrap_or(&[]);
    if name.is_empty() {
        write_user_bytes(memory, pointer, &[0])?;
        return Ok(false);
    }

    let capacity = capacity as usize;
    if capacity > name.len() {
        write_user_bytes(memory, pointer, name)?;
        let terminator = pointer.checked_add(name.len()).ok_or(AxError::BadAddress)?;
        write_user_bytes(memory, terminator, &[0])?;
        return Ok(false);
    }

    let prefix = capacity - 1;
    if prefix != 0 {
        write_user_bytes(memory, pointer, &name[..prefix])?;
    }
    let terminator = pointer.checked_add(prefix).ok_or(AxError::BadAddress)?;
    write_user_bytes(memory, terminator, &[0])?;
    Ok(true)
}

fn publish_query_result<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
    attr: &BpfAttrTaskFdQuery,
    result: TaskFdQueryResult<'_>,
) -> AxResult<isize> {
    let name_len =
        u32::try_from(result.name.map_or(0, <[u8]>::len)).map_err(|_| AxError::InvalidInput)?;
    write_bpf_attr_value::<BpfAttrTaskFdQuery, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrTaskFdQuery, buf_len),
        &name_len,
    )?;

    // Linux still publishes all scalar outputs after a short-name-buffer
    // ENOSPC.  EFAULT is terminal and takes priority over those later writes.
    let truncated = copy_query_name(memory, attr.buf, attr.buf_len, result.name)?;
    for (offset, value) in [
        (
            offset_of!(BpfAttrTaskFdQuery, prog_id),
            result.prog_id.to_ne_bytes(),
        ),
        (
            offset_of!(BpfAttrTaskFdQuery, fd_type),
            result.fd_type.to_ne_bytes(),
        ),
    ] {
        write_user_bytes(
            memory,
            attr_ptr.checked_add(offset).ok_or(AxError::BadAddress)?,
            &value,
        )?;
    }
    write_bpf_attr_value::<BpfAttrTaskFdQuery, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrTaskFdQuery, probe_offset),
        &result.probe_offset,
    )?;
    write_bpf_attr_value::<BpfAttrTaskFdQuery, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrTaskFdQuery, probe_addr),
        &result.probe_addr,
    )?;
    if truncated {
        Err(LinuxError::ENOSPC.into())
    } else {
        Ok(0)
    }
}

pub fn bpf_task_fd_query<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrTaskFdQuery>(
        attr_size,
        offset_of!(BpfAttrTaskFdQuery, probe_addr) + size_of::<u64>(),
    )?;
    let attr: BpfAttrTaskFdQuery = read_bpf_attr(memory, attr_ptr, attr_size)?;

    let current = axtask::current();
    let actor = current.as_thread();
    if !actor.has_effective_capability(CAP_SYS_ADMIN) {
        return Err(AxError::OperationNotPermitted);
    }
    if attr.flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let kernel_pid = actor
        .pid_ns()
        .resolve_visible_pid(attr.pid)
        .ok_or(AxError::NotFound)?;
    let target = get_task(kernel_pid).map_err(|_| AxError::NotFound)?;
    let thread = target.try_as_thread().ok_or(AxError::NotFound)?;
    let files = thread.try_fd_table().ok_or(AxError::BadFileDescriptor)?;
    let description = files.get_description_number(attr.fd)?;
    let file = description.file_handle();

    if let Ok(raw) = file.downcast::<BpfRawTracepointLink>() {
        let (prog_id, name) = raw.task_fd_query()?;
        return publish_query_result(
            memory,
            attr_ptr,
            attr_size,
            &attr,
            TaskFdQueryResult {
                prog_id,
                fd_type: BPF_FD_TYPE_RAW_TRACEPOINT,
                name: Some(name.as_bytes()),
                probe_offset: 0,
                probe_addr: 0,
            },
        );
    }
    if let Ok(perf) = file.downcast::<PerfEventFile>() {
        let result = perf.bpf_task_fd_query()?;
        let name = result.name.as_ref().map(|name| name.as_bytes());
        return publish_query_result(
            memory,
            attr_ptr,
            attr_size,
            &attr,
            TaskFdQueryResult {
                prog_id: result.prog_id,
                fd_type: result.fd_type,
                name,
                probe_offset: result.probe_offset,
                probe_addr: result.probe_addr,
            },
        );
    }

    Err(AxError::OperationNotSupported)
}
