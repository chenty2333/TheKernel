//! BPF object info query command handlers.

use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult};

use crate::{
    bpf::{defs::*, read_bpf_attr, require_bpf_attr_range, write_bpf_attr_value},
    file::{
        FileLike,
        bpf::{BpfMapFd, BpfProgFd},
        get_file_like,
    },
};

pub fn bpf_obj_get_info_by_fd(attr_ptr: usize, attr_size: u32) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrGetInfoByFd>(
        attr_size,
        offset_of!(BpfAttrGetInfoByFd, info) + size_of::<u64>(),
    )?;
    let attr: BpfAttrGetInfoByFd = read_bpf_attr(attr_ptr, attr_size)?;
    debug!("bpf_obj_get_info_by_fd: fd={}", attr.bpf_fd);

    let fd_obj = get_file_like(attr.bpf_fd as _)?;

    if let Some(map_fd) = fd_obj.downcast_ref::<BpfMapFd>() {
        return write_map_info(map_fd, attr.info, attr.info_len, attr_ptr, attr_size);
    }
    if let Some(prog_fd) = fd_obj.downcast_ref::<BpfProgFd>() {
        return write_prog_info(prog_fd, attr.info, attr.info_len, attr_ptr, attr_size);
    }

    Err(AxError::InvalidInput)
}

fn write_map_info(
    map_fd: &BpfMapFd,
    info_ptr: u64,
    info_len: u32,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    let map = &map_fd.map;

    let info = BpfMapInfo {
        type_: map.map_type(),
        id: map_fd.map_id,
        key_size: map.key_size(),
        value_size: map.value_size(),
        max_entries: map.max_entries(),
        map_flags: map.map_flags(),
        name: map.name(),
        ..Default::default()
    };

    let info_bytes = unsafe {
        core::slice::from_raw_parts(
            &info as *const BpfMapInfo as *const u8,
            core::mem::size_of::<BpfMapInfo>(),
        )
    };

    let copy_len = (info_len as usize).min(info_bytes.len());
    starry_vm::vm_write_slice(info_ptr as *mut u8, &info_bytes[..copy_len])
        .map_err(|_| AxError::BadAddress)?;

    write_bpf_attr_value::<BpfAttrGetInfoByFd, _>(
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrGetInfoByFd, info_len),
        &(copy_len as u32),
    )?;

    Ok(0)
}

fn write_prog_info(
    prog_fd: &BpfProgFd,
    info_ptr: u64,
    info_len: u32,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    let prog = &prog_fd.prog;

    // Compute a simple tag (hash of instructions) for identification
    let mut tag = [0u8; 8];
    let insn_bytes = unsafe {
        core::slice::from_raw_parts(
            prog.insns.as_ptr() as *const u8,
            prog.insns.len() * core::mem::size_of::<BpfInsn>(),
        )
    };
    // Simple FNV-1a hash truncated to 8 bytes
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in insn_bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    tag.copy_from_slice(&hash.to_ne_bytes());

    let xlated_len = (prog.insns.len() * core::mem::size_of::<BpfInsn>()) as u32;

    let info = BpfProgInfo {
        type_: prog.prog_type,
        id: prog.prog_id,
        tag,
        jited_prog_len: 0, // no JIT
        xlated_prog_len: xlated_len,
        name: prog.name,
        gpl_compatible: prog.gpl_compatible as u32,
        nr_map_ids: prog.maps.len() as u32,
        ..Default::default()
    };

    let info_bytes = unsafe {
        core::slice::from_raw_parts(
            &info as *const BpfProgInfo as *const u8,
            core::mem::size_of::<BpfProgInfo>(),
        )
    };

    let copy_len = (info_len as usize).min(info_bytes.len());
    starry_vm::vm_write_slice(info_ptr as *mut u8, &info_bytes[..copy_len])
        .map_err(|_| AxError::BadAddress)?;

    write_bpf_attr_value::<BpfAttrGetInfoByFd, _>(
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrGetInfoByFd, info_len),
        &(copy_len as u32),
    )?;

    Ok(0)
}
