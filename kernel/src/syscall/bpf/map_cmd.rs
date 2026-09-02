//! BPF map syscall command handlers.

use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult};
use thekernel_linux_bpf::{BpfAttr as LinuxBpfAttr, BpfCommand, MapCreateRequest};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::{
    bpf::{alloc_map_id, defs::*, map, read_bpf_attr, register_map_id, require_bpf_attr_range},
    bpf_security::{authorize_map_create, authorize_token_map_create, reserve_memory},
    file::{FileLike, bpf::BpfTokenFd, get_typed_file},
};

pub fn bpf_map_create<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapCreate>(
        attr_size,
        offset_of!(BpfAttrMapCreate, max_entries) + size_of::<u32>(),
    )?;
    let attr: BpfAttrMapCreate = read_bpf_attr(memory, attr_ptr, attr_size)?;
    debug!(
        "bpf_map_create: type={}, key_size={}, value_size={}, max_entries={}",
        attr.map_type, attr.key_size, attr.value_size, attr.max_entries
    );

    validate_map_create_attr(&attr)?;
    // Linux ABI admission belongs to the policy crate.  The kernel retains
    // raw attr decoding, user-copy and FD publication below.
    let struct_ops_btf = if attr.map_type == BPF_MAP_TYPE_STRUCT_OPS {
        if attr.key_size != 0
            || attr.value_size == 0
            || attr.max_entries != 1
            || attr.btf_fd == 0
            || attr.btf_vmlinux_value_type_id == 0
        {
            return Err(AxError::InvalidInput);
        }
        Some(
            crate::file::get_typed_file::<crate::file::bpf::BpfBtfFd>(attr.btf_fd as i32)?
                .object
                .clone(),
        )
    } else {
        None
    };
    let reservation_bytes = if struct_ops_btf.is_some() {
        attr.value_size as usize
    } else {
        let request = MapCreateRequest::from_attr(LinuxBpfAttr {
            command: BpfCommand::MapCreate,
            object_type: attr.map_type,
            key_size: attr.key_size,
            value_size: attr.value_size,
            max_entries: attr.max_entries,
            flags: attr.map_flags,
        })
        .map_err(|_| AxError::InvalidInput)?;
        let bytes = request.reservation_bytes().map_err(|_| AxError::NoMemory)?;
        let bytes = if matches!(attr.map_type, BPF_MAP_TYPE_HASH | BPF_MAP_TYPE_LRU_HASH) {
            let buckets = (attr.max_entries as usize)
                .checked_next_power_of_two()
                .ok_or(AxError::NoMemory)?
                .max(1);
            if buckets > u32::MAX as usize {
                return Err(AxError::NoMemory);
            }
            let metadata = buckets
                .checked_mul(axbpf::HASH_MAP_BUCKET_BYTES)
                .ok_or(AxError::NoMemory)?;
            let owned_keys = (attr.max_entries as usize)
                .checked_mul(attr.key_size as usize)
                .ok_or(AxError::NoMemory)?;
            let entry_slots = (attr.max_entries as usize)
                .checked_mul(axbpf::HASH_MAP_SLOT_BYTES)
                .ok_or(AxError::NoMemory)?;
            // LRU recency is intrusive in the preallocated node slots; no
            // separate key payload or VecDeque capacity exists at runtime.
            bytes
                .checked_add(metadata)
                .and_then(|total| total.checked_add(entry_slots))
                .and_then(|total| total.checked_add(owned_keys))
                .ok_or(AxError::NoMemory)?
        } else {
            bytes
        };
        if matches!(
            attr.map_type,
            BPF_MAP_TYPE_PERCPU_HASH | BPF_MAP_TYPE_PERCPU_ARRAY
        ) {
            bytes
                .checked_mul(axhal::cpu_num().max(1))
                .ok_or(AxError::NoMemory)?
        } else {
            bytes
        }
    };
    // `map_token_fd` is an FD capability, not a boolean bypass.  A supplied
    // token selects its namespace-anchored grant; an absent token retains the
    // ordinary ambient-capability path.
    if attr.map_token_fd == 0 {
        authorize_map_create(attr.map_type)?;
    } else {
        if attr.map_token_fd < 0 {
            return Err(AxError::InvalidInput);
        }
        let token = get_typed_file::<BpfTokenFd>(attr.map_token_fd)?;
        authorize_token_map_create(&token.grant, attr.map_type)?;
    }
    let memory_charge = reserve_memory(reservation_bytes)?;

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
    // The ID registry and the descriptor each retain the BTF association for
    // the map's lifetime.  Keep independent Arc references rather than
    // transferring the descriptor's only reference into the registry.
    register_map_id(
        id,
        &map,
        &memory_charge,
        attr.map_name,
        struct_ops_btf.clone(),
    )?;

    // Create fd for the map.
    use crate::file::bpf::BpfMapFd;
    BpfMapFd::new(map, id, attr.map_name, memory_charge, struct_ops_btf)
        .add_to_fd_table(false)
        .map(|fd| fd as isize)
}

pub fn bpf_map_lookup_elem<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(attr_size, size_of::<BpfAttrMapElem>())?;
    let attr: BpfAttrMapElem = read_bpf_attr(memory, attr_ptr, attr_size)?;
    validate_map_lookup_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    map_fd.require_read()?;
    let map = &map_fd.map;

    let key_size = map.key_size() as usize;
    let value_size = map.user_value_size();

    let key = crate::bpf::read_user_bytes(memory, attr.key as usize, key_size)?;
    let value = map.lookup_user(&key).ok_or(AxError::NotFound)?;

    crate::bpf::write_user_bytes(
        memory,
        attr.value_or_next_key as usize,
        &value[..value_size],
    )?;
    Ok(0)
}

pub fn bpf_map_update_elem<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(attr_size, size_of::<BpfAttrMapElem>())?;
    let attr: BpfAttrMapElem = read_bpf_attr(memory, attr_ptr, attr_size)?;
    validate_map_update_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    let map = &map_fd.map;
    let _write_active = crate::bpf::map::map_write_active(&**map)?;
    map_fd.require_write()?;

    let key_size = map.key_size() as usize;
    let value_size = map.user_value_size();

    let key = crate::bpf::read_user_bytes(memory, attr.key as usize, key_size)?;
    let value = crate::bpf::read_user_bytes(memory, attr.value_or_next_key as usize, value_size)?;

    map.update_user(&key, &value, attr.flags)?;
    Ok(0)
}

pub fn bpf_map_delete_elem<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(attr_size, size_of::<BpfAttrMapElem>())?;
    let attr: BpfAttrMapElem = read_bpf_attr(memory, attr_ptr, attr_size)?;
    validate_map_delete_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    let map = &map_fd.map;
    let _write_active = crate::bpf::map::map_write_active(&**map)?;
    map_fd.require_write()?;

    let key = crate::bpf::read_user_bytes(memory, attr.key as usize, map.key_size() as usize)?;
    map.delete(&key)?;
    Ok(0)
}

/// `BPF_MAP_LOOKUP_AND_DELETE_ELEM` keeps the map ownership and usercopy
/// ordering explicit: validate/copy the key, take the value, then retire that
/// exact key before copying the returned value to userspace.
pub fn bpf_map_lookup_and_delete_elem<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(attr_size, size_of::<BpfAttrMapElem>())?;
    let attr: BpfAttrMapElem = read_bpf_attr(memory, attr_ptr, attr_size)?;
    validate_map_lookup_attr(&attr)?;
    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    let map = &map_fd.map;
    let _write_active = crate::bpf::map::map_write_active(&**map)?;
    map_fd.require_read()?;
    map_fd.require_write()?;
    let key = crate::bpf::read_user_bytes(memory, attr.key as usize, map.key_size() as usize)?;
    let value = map.lookup_and_delete(&key)?;
    crate::bpf::write_user_bytes(memory, attr.value_or_next_key as usize, &value)?;
    Ok(0)
}

pub fn bpf_map_get_next_key<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(attr_size, size_of::<BpfAttrMapElem>())?;
    let attr: BpfAttrMapElem = read_bpf_attr(memory, attr_ptr, attr_size)?;
    validate_map_get_next_key_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    map_fd.require_read()?;
    let map = &map_fd.map;

    let key = if attr.key == 0 {
        None
    } else {
        Some(crate::bpf::read_user_bytes(
            memory,
            attr.key as usize,
            map.key_size() as usize,
        )?)
    };

    let next = map.get_next_key(key.as_deref()).ok_or(AxError::NotFound)?;

    crate::bpf::write_user_bytes(memory, attr.value_or_next_key as usize, &next)?;
    Ok(0)
}

pub fn bpf_map_freeze<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrMapElem>(
        attr_size,
        offset_of!(BpfAttrMapElem, map_fd) + size_of::<u32>(),
    )?;
    let attr: BpfAttrMapElem = read_bpf_attr(memory, attr_ptr, attr_size)?;
    validate_map_freeze_attr(&attr)?;

    let map_fd = crate::file::bpf::BpfMapFd::from_fd(attr.map_fd as _)?;
    if map_fd.map.map_type() == crate::bpf::defs::BPF_MAP_TYPE_STRUCT_OPS {
        return Err(AxError::OperationNotSupported);
    }
    map_fd.require_write()?;
    crate::bpf::map::map_freeze_active(&*map_fd.map)?;
    map_fd.map.freeze()?;
    Ok(0)
}

fn validate_map_create_attr(attr: &BpfAttrMapCreate) -> AxResult<()> {
    if attr.inner_map_fd != 0
        || attr.numa_node != 0
        || attr.map_ifindex != 0
        || (attr.map_type != BPF_MAP_TYPE_STRUCT_OPS && attr.btf_fd != 0)
        || attr.btf_key_type_id != 0
        || attr.btf_value_type_id != 0
        || (attr.map_type != BPF_MAP_TYPE_STRUCT_OPS && attr.btf_vmlinux_value_type_id != 0)
        || attr.map_extra != 0
        || attr.value_type_btf_obj_fd != 0
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
