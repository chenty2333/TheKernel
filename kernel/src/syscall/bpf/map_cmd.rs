//! BPF map syscall command handlers.

use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult};

use crate::{
    bpf::{
        alloc_map_id,
        defs::*,
        map::{self, BpfMap},
        read_bpf_attr, require_bpf_attr_range,
    },
    file::FileLike,
};

pub fn bpf_map_create(attr_ptr: usize, attr_size: u32) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapCreate>(
        attr_size,
        offset_of!(BpfAttrMapCreate, max_entries) + size_of::<u32>(),
    )?;
    let attr: BpfAttrMapCreate = read_bpf_attr(attr_ptr, attr_size)?;
    debug!(
        "bpf_map_create: type={}, key_size={}, value_size={}, max_entries={}",
        attr.map_type, attr.key_size, attr.value_size, attr.max_entries
    );

    validate_map_create_attr(&attr)?;

    let id = alloc_map_id();
    let map = map::create_map(
        attr.map_type,
        attr.key_size,
        attr.value_size,
        attr.max_entries,
        attr.map_flags,
        attr.map_name,
        id,
    )?;

    // Create fd for the map.
    use crate::file::bpf::BpfMapFd;
    BpfMapFd::new(map, id, attr.map_name)
        .add_to_fd_table(false)
        .map(|fd| fd as isize)
}

pub fn bpf_map_lookup_elem(attr_ptr: usize, attr_size: u32) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(attr_size, size_of::<BpfAttrMapElem>())?;
    let attr: BpfAttrMapElem = read_bpf_attr(attr_ptr, attr_size)?;
    validate_map_lookup_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    let map = &map_fd.map;

    let key_size = map.key_size() as usize;
    let value_size = map.value_size() as usize;

    let key = starry_vm::vm_load(attr.key as *const u8, key_size)?;
    let value = map.lookup(&key).ok_or(AxError::NotFound)?;

    starry_vm::vm_write_slice(attr.value_or_next_key as *mut u8, &value[..value_size])?;
    Ok(0)
}

pub fn bpf_map_update_elem(attr_ptr: usize, attr_size: u32) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(attr_size, size_of::<BpfAttrMapElem>())?;
    let attr: BpfAttrMapElem = read_bpf_attr(attr_ptr, attr_size)?;
    validate_map_update_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    let map = &map_fd.map;

    let key_size = map.key_size() as usize;
    let value_size = map.value_size() as usize;

    let key = starry_vm::vm_load(attr.key as *const u8, key_size)?;
    let value = starry_vm::vm_load(attr.value_or_next_key as *const u8, value_size)?;

    map.update(&key, &value, attr.flags)?;
    Ok(0)
}

pub fn bpf_map_delete_elem(attr_ptr: usize, attr_size: u32) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(attr_size, size_of::<BpfAttrMapElem>())?;
    let attr: BpfAttrMapElem = read_bpf_attr(attr_ptr, attr_size)?;
    validate_map_delete_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    let map = &map_fd.map;

    let key = starry_vm::vm_load(attr.key as *const u8, map.key_size() as usize)?;
    map.delete(&key)?;
    Ok(0)
}

pub fn bpf_map_get_next_key(attr_ptr: usize, attr_size: u32) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(attr_size, size_of::<BpfAttrMapElem>())?;
    let attr: BpfAttrMapElem = read_bpf_attr(attr_ptr, attr_size)?;
    validate_map_get_next_key_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    let map = &map_fd.map;

    let key = if attr.key == 0 {
        None
    } else {
        Some(starry_vm::vm_load(
            attr.key as *const u8,
            map.key_size() as usize,
        )?)
    };

    let next = map.get_next_key(key.as_deref()).ok_or(AxError::NotFound)?;

    starry_vm::vm_write_slice(attr.value_or_next_key as *mut u8, &next)?;
    Ok(0)
}

pub fn bpf_map_freeze(attr_ptr: usize, attr_size: u32) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(
        attr_size,
        offset_of!(BpfAttrMapElem, map_fd) + size_of::<u32>(),
    )?;
    let attr: BpfAttrMapElem = read_bpf_attr(attr_ptr, attr_size)?;
    validate_map_freeze_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    map_fd.map.freeze()?;
    Ok(0)
}

fn validate_map_create_attr(attr: &BpfAttrMapCreate) -> AxResult<()> {
    if attr.inner_map_fd != 0
        || attr.numa_node != 0
        || attr.map_ifindex != 0
        || attr.btf_fd != 0
        || attr.btf_key_type_id != 0
        || attr.btf_value_type_id != 0
        || attr.btf_vmlinux_value_type_id != 0
        || attr.map_extra != 0
        || attr.value_type_btf_obj_fd != 0
        || attr.map_token_fd != 0
        || attr.excl_prog_hash != 0
        || attr.excl_prog_hash_size != 0
    {
        return Err(AxError::InvalidInput);
    }

    Ok(())
}

fn validate_map_elem_common(attr: &BpfAttrMapElem) -> AxResult<()> {
    if attr._pad0 != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_map_lookup_attr(attr: &BpfAttrMapElem) -> AxResult<()> {
    validate_map_elem_common(attr)?;
    if attr.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_map_update_attr(attr: &BpfAttrMapElem) -> AxResult<()> {
    validate_map_elem_common(attr)?;
    if !matches!(attr.flags, BPF_ANY | BPF_NOEXIST | BPF_EXIST) {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_map_delete_attr(attr: &BpfAttrMapElem) -> AxResult<()> {
    validate_map_elem_common(attr)?;
    if attr.value_or_next_key != 0 || attr.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_map_get_next_key_attr(attr: &BpfAttrMapElem) -> AxResult<()> {
    validate_map_elem_common(attr)?;
    if attr.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_map_freeze_attr(attr: &BpfAttrMapElem) -> AxResult<()> {
    validate_map_elem_common(attr)?;
    if attr.key != 0 || attr.value_or_next_key != 0 || attr.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}
