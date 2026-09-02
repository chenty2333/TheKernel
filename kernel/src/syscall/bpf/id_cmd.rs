use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::{
    bpf::{
        link_by_id, map_by_id, next_link_id, next_map_id, next_prog_id, prog_by_id, read_bpf_attr,
        read_user_bytes, require_bpf_attr_range, write_bpf_attr_value,
    },
    bpf_security::BpfAuthority,
    file::{
        FileLike,
        bpf::{BpfMapFd, BpfProgFd, BpfTokenFd},
        get_typed_file,
    },
};

#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct IdQuery {
    start_id: u32,
    next_id: u32,
    open_flags: u32,
    fd_by_id_token_fd: i32,
}
const _: [(); 16] = [(); size_of::<IdQuery>()];
fn query<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: usize,
    size: u32,
) -> AxResult<IdQuery> {
    require_bpf_attr_range::<IdQuery>(size, size_of::<IdQuery>())?;
    let value: IdQuery = read_bpf_attr(memory, attr, size)?;
    if value.open_flags != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(value)
}
fn link_query<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: usize,
    size: u32,
    minimum: usize,
) -> AxResult<IdQuery> {
    require_bpf_attr_range::<IdQuery>(size, minimum)?;
    if size as usize > minimum {
        let tail = read_user_bytes(
            memory,
            attr.checked_add(minimum).ok_or(AxError::InvalidInput)?,
            size as usize - minimum,
        )?;
        if tail.iter().any(|byte| *byte != 0) {
            return Err(AxError::InvalidInput);
        }
    }
    let value: IdQuery = read_bpf_attr(memory, attr, size)?;
    Ok(value)
}
fn authorize_by_id(token_fd: i32) -> AxResult<()> {
    if token_fd == 0 {
        if BpfAuthority::current().bpf_capable() {
            Ok(())
        } else {
            Err(AxError::OperationNotPermitted)
        }
    } else if token_fd < 0 {
        Err(AxError::BadFileDescriptor)
    } else {
        get_typed_file::<BpfTokenFd>(token_fd)
            .and_then(|token| token.grant.authorize_by_id_lookup())
    }
}
fn next<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: usize,
    size: u32,
    lookup: impl FnOnce(u32) -> Option<u32>,
) -> AxResult<isize> {
    let value = query(memory, attr, size)?;
    if value.fd_by_id_token_fd != 0 {
        return Err(AxError::InvalidInput);
    }
    let id = lookup(value.start_id).ok_or(LinuxError::ENOENT)?;
    write_bpf_attr_value::<IdQuery, _, _>(memory, attr, size, 4, &id)?;
    Ok(0)
}
pub fn bpf_map_get_next_id<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    next(m, a, s, next_map_id)
}
pub fn bpf_prog_get_next_id<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    next(m, a, s, next_prog_id)
}
pub fn bpf_map_get_fd_by_id<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    let q = query(m, a, s)?;
    authorize_by_id(q.fd_by_id_token_fd)?;
    let (map, charge, name, btf) = map_by_id(q.start_id).ok_or(LinuxError::ENOENT)?;
    BpfMapFd::new(map, q.start_id, name, charge, btf)
        .add_to_fd_table(false)
        .map(|fd| fd as isize)
}
pub fn bpf_prog_get_fd_by_id<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    let q = query(m, a, s)?;
    authorize_by_id(q.fd_by_id_token_fd)?;
    BpfProgFd::new(prog_by_id(q.start_id).ok_or(LinuxError::ENOENT)?)
        .add_to_fd_table(false)
        .map(|fd| fd as isize)
}
pub fn bpf_link_get_next_id<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    let q = link_query(m, a, s, 8)?;
    if q.start_id >= i32::MAX as u32 {
        return Err(AxError::InvalidInput);
    }
    if !BpfAuthority::current().bpf_capable() {
        return Err(AxError::OperationNotPermitted);
    }
    let id = next_link_id(q.start_id).ok_or(LinuxError::ENOENT)?;
    write_bpf_attr_value::<IdQuery, _, _>(m, a, s, 4, &id)?;
    Ok(0)
}
pub fn bpf_link_get_fd_by_id<M: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, M>,
    a: usize,
    s: u32,
) -> AxResult<isize> {
    let q = link_query(m, a, s, 4)?;
    authorize_by_id(0)?;
    link_by_id(q.start_id)
        .ok_or(LinuxError::ENOENT)?
        .add_to_fd_table(true)
        .map(|fd| fd as isize)
}
