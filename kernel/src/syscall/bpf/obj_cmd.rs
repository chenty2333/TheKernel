//! BPF object info query command handlers.

use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::FsPathBuf;
use linux_raw_sys::general::{AT_FDCWD, O_CREAT, O_EXCL, O_RDONLY};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

use crate::{
    bpf::{
        PinnedObject, defs::*, pinned_object, publish_pin, read_bpf_attr, read_user_bytes,
        require_bpf_attr_range, reserve_pin_slot, write_bpf_attr_value,
    },
    bpf_security::BpfAuthority,
    file::{
        FileLike,
        bpf::{
            BpfBtfFd, BpfIterLink, BpfLsmLink, BpfMapFd, BpfNetworkLink, BpfNetworkLinkInfo,
            BpfPerfEventLink, BpfProgFd, BpfRawTracepointLink,
        },
        get_file_like, resolve_at,
    },
    mounts,
    syscall::fs::openat_inner,
};

fn object_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pointer: u64,
) -> AxResult<FsPathBuf> {
    if pointer == 0 {
        return Err(AxError::BadAddress);
    }
    let bytes = memory
        .load_until_nul(pointer as *const u8)
        .map_err(crate::mm::map_usercopy_error)?;
    let path = FsPathBuf::from_vec(bytes);
    crate::file::validate_pathname(&path)?;
    if path.as_bytes().is_empty() {
        return Err(AxError::NotFound);
    }
    Ok(path)
}

fn require_bpffs(location: &axfs_ng_vfs::Location) -> AxResult<()> {
    if mounts::metadata_for_location(location)?.fs_type == "bpf" {
        Ok(())
    } else {
        Err(LinuxError::EINVAL.into())
    }
}

fn pin_parent(path: &FsPathBuf) -> AxResult<FsPathBuf> {
    let bytes = path.as_bytes();
    let Some(last_slash) = bytes.iter().rposition(|byte| *byte == b'/') else {
        return Ok(FsPathBuf::from_vec(b".".to_vec()));
    };
    if last_slash == 0 {
        return Ok(FsPathBuf::from_vec(b"/".to_vec()));
    }
    Ok(FsPathBuf::from_vec(bytes[..last_slash].to_vec()))
}

fn object_path_context(attr: &BpfAttrObj, attr_size: u32) -> AxResult<(i32, u32)> {
    let known = BPF_F_RDONLY | BPF_F_WRONLY | BPF_F_PATH_FD;
    if attr.file_flags & !known != 0
        || attr.file_flags & (BPF_F_RDONLY | BPF_F_WRONLY) == (BPF_F_RDONLY | BPF_F_WRONLY)
    {
        return Err(AxError::InvalidInput);
    }
    let path_fd = if attr.file_flags & BPF_F_PATH_FD != 0 {
        require_bpf_attr_range::<BpfAttrObj>(
            attr_size,
            offset_of!(BpfAttrObj, path_fd) + size_of::<i32>(),
        )?;
        if attr.path_fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        attr.path_fd
    } else {
        if attr.path_fd != 0 {
            return Err(AxError::InvalidInput);
        }
        AT_FDCWD
    };
    Ok((path_fd, attr.file_flags & (BPF_F_RDONLY | BPF_F_WRONLY)))
}

/// Creates one bpffs dentry and transfers a single reference to the BPF
/// object registry.  The registry reservation precedes O_EXCL creation, so
/// memory pressure never leaves an unpinned ordinary file at the target.
pub fn bpf_obj_pin<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrObj>(
        attr_size,
        offset_of!(BpfAttrObj, file_flags) + size_of::<u32>(),
    )?;
    let attr: BpfAttrObj = read_bpf_attr(memory, attr_ptr, attr_size)?;
    let (path_fd, descriptor_flags) = object_path_context(&attr, attr_size)?;
    if descriptor_flags != 0 {
        return Err(AxError::InvalidInput);
    }
    if !BpfAuthority::current().bpf_capable() {
        return Err(AxError::OperationNotPermitted);
    }
    let path = object_path(memory, attr.pathname)?;
    // bpffs deliberately reserves dot-bearing names for internal dentry
    // conventions; Linux rejects them at BPF_OBJ_PIN time rather than
    // creating a node that cannot later be addressed through the object API.
    if path.as_bytes().contains(&b'.') {
        return Err(AxError::InvalidInput);
    }
    let source = get_file_like(attr.bpf_fd as _)?;
    let object = if let Some(map) = source.downcast_ref::<BpfMapFd>() {
        PinnedObject::Map {
            map: map.map.clone(),
            id: map.map_id,
            name: map.name,
            btf: map.btf.clone(),
            charge: map.memory_charge.clone(),
        }
    } else if let Some(program) = source.downcast_ref::<BpfProgFd>() {
        PinnedObject::Program(program.prog.clone())
    } else if let Some(btf) = source.downcast_ref::<BpfBtfFd>() {
        PinnedObject::Btf(btf.object.clone())
    } else if source.downcast_ref::<BpfPerfEventLink>().is_some() {
        PinnedObject::PerfEventLink(BpfPerfEventLink::from_fd(attr.bpf_fd as _)?.clone_object())
    } else if source.downcast_ref::<BpfIterLink>().is_some() {
        PinnedObject::IterLink(BpfIterLink::from_fd(attr.bpf_fd as _)?.clone_object())
    } else if source.downcast_ref::<BpfLsmLink>().is_some() {
        PinnedObject::LsmLink(BpfLsmLink::from_fd(attr.bpf_fd as _)?.clone_object())
    } else if source.downcast_ref::<BpfNetworkLink>().is_some() {
        PinnedObject::NetworkLink(BpfNetworkLink::from_fd(attr.bpf_fd as _)?.clone_object())
    } else if source.downcast_ref::<BpfRawTracepointLink>().is_some() {
        PinnedObject::RawTracepointLink(
            BpfRawTracepointLink::from_fd(attr.bpf_fd as _)?.clone_object(),
        )
    } else {
        return Err(AxError::InvalidInput);
    };

    // Validate the containing mount before creating the final dentry.  bpffs
    // is an object filesystem, never an xattr convention applied to an
    // arbitrary tmpfs/ext4 file.
    let parent_path = pin_parent(&path)?;
    let parent = resolve_at(path_fd, Some(&parent_path), 0)?
        .into_file()
        .ok_or(AxError::NotADirectory)?;
    parent.check_is_dir()?;
    require_bpffs(&parent)?;
    let mut reservation = reserve_pin_slot()?;
    let fd = openat_inner(path_fd, &path, (O_RDONLY | O_CREAT | O_EXCL) as i32, 0o600)?;
    // The just-created inode cannot be raced through this pathname because
    // O_EXCL made it ours. Resolve it again to obtain the authoritative mount
    // and generation-aware dentry identity used by the pin registry.
    let target = resolve_at(path_fd, Some(&path), 0)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    require_bpffs(&target)?;
    publish_pin(&mut reservation, &target, object)?;
    crate::file::close_file_like(fd as i32)?;
    Ok(0)
}

pub fn bpf_obj_get<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrObj>(
        attr_size,
        offset_of!(BpfAttrObj, file_flags) + size_of::<u32>(),
    )?;
    let attr: BpfAttrObj = read_bpf_attr(memory, attr_ptr, attr_size)?;
    let (path_fd, descriptor_flags) = object_path_context(&attr, attr_size)?;
    if attr.bpf_fd != 0 {
        return Err(AxError::InvalidInput);
    }
    if !BpfAuthority::current().bpf_capable() {
        return Err(AxError::OperationNotPermitted);
    }
    let path = object_path(memory, attr.pathname)?;
    let target = resolve_at(path_fd, Some(&path), 0)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    require_bpffs(&target)?;
    match pinned_object(&target).ok_or(LinuxError::ENOENT)? {
        PinnedObject::Map {
            map,
            id,
            name,
            btf,
            charge,
        } => BpfMapFd::new_with_file_flags(map, id, name, charge, descriptor_flags, btf)?
            .add_to_fd_table(false)
            .map(|fd| fd as isize),
        PinnedObject::Program(program) => {
            if descriptor_flags != 0 {
                return Err(AxError::InvalidInput);
            }
            BpfProgFd::new(program)
                .add_to_fd_table(false)
                .map(|fd| fd as isize)
        }
        PinnedObject::Btf(object) => {
            if descriptor_flags != 0 {
                return Err(AxError::InvalidInput);
            }
            BpfBtfFd::new(object)
                .add_to_fd_table(false)
                .map(|fd| fd as isize)
        }
        PinnedObject::PerfEventLink(link) => {
            if descriptor_flags != 0 {
                return Err(AxError::InvalidInput);
            }
            crate::file::add_file_like(link, false).map(|fd| fd as isize)
        }
        PinnedObject::IterLink(link) => {
            if descriptor_flags != 0 {
                return Err(AxError::InvalidInput);
            }
            crate::file::add_file_like(link, false).map(|fd| fd as isize)
        }
        PinnedObject::LsmLink(link) => {
            if descriptor_flags != 0 {
                return Err(AxError::InvalidInput);
            }
            crate::file::add_file_like(link, false).map(|fd| fd as isize)
        }
        PinnedObject::NetworkLink(link) => {
            if descriptor_flags != 0 {
                return Err(AxError::InvalidInput);
            }
            crate::file::add_file_like(link, false).map(|fd| fd as isize)
        }
        PinnedObject::RawTracepointLink(link) => {
            if descriptor_flags != 0 {
                return Err(AxError::InvalidInput);
            }
            crate::file::add_file_like(link, false).map(|fd| fd as isize)
        }
    }
}

pub fn bpf_obj_get_info_by_fd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    require_bpf_attr_range::<BpfAttrGetInfoByFd>(
        attr_size,
        offset_of!(BpfAttrGetInfoByFd, info) + size_of::<u64>(),
    )?;
    let attr: BpfAttrGetInfoByFd = read_bpf_attr(memory, attr_ptr, attr_size)?;
    debug!("bpf_obj_get_info_by_fd: fd={}", attr.bpf_fd);

    let fd_obj = get_file_like(attr.bpf_fd as _)?;

    if let Some(map_fd) = fd_obj.downcast_ref::<BpfMapFd>() {
        return write_map_info(
            memory,
            map_fd,
            attr.info,
            attr.info_len,
            attr_ptr,
            attr_size,
        );
    }
    if let Some(prog_fd) = fd_obj.downcast_ref::<BpfProgFd>() {
        return write_prog_info(
            memory,
            prog_fd,
            attr.info,
            attr.info_len,
            attr_ptr,
            attr_size,
        );
    }
    if let Some(btf_fd) = fd_obj.downcast_ref::<BpfBtfFd>() {
        return write_btf_info(
            memory,
            btf_fd,
            attr.info,
            attr.info_len,
            attr_ptr,
            attr_size,
        );
    }
    if let Some(link) = fd_obj.downcast_ref::<BpfNetworkLink>() {
        return write_link_info(
            memory,
            crate::bpf::link_id_for_network(link),
            link.program_id(),
            Some(link.link_info()),
            None,
            None,
            attr.info,
            attr.info_len,
            attr_ptr,
            attr_size,
        );
    }
    if let Some(link) = fd_obj.downcast_ref::<BpfIterLink>() {
        let mut data = [0u8; 48];
        data[12..16].copy_from_slice(&link.map_id().to_ne_bytes());
        return write_link_info(
            memory,
            crate::bpf::link_id_for_iter(link),
            link.program_id(),
            None,
            Some(data),
            None,
            attr.info,
            attr.info_len,
            attr_ptr,
            attr_size,
        );
    }
    if let Some(link) = fd_obj.downcast_ref::<BpfLsmLink>() {
        let mut data = [0u8; 48];
        data[..4].copy_from_slice(&crate::bpf::prog::BPF_LSM_MAC.to_ne_bytes());
        data[8..12].copy_from_slice(&link.tracing_target().to_ne_bytes());
        return write_link_info(
            memory,
            crate::bpf::link_id_for_lsm(link),
            link.program_id(),
            None,
            Some(data),
            None,
            attr.info,
            attr.info_len,
            attr_ptr,
            attr_size,
        );
    }
    if let Some(link) = fd_obj.downcast_ref::<BpfPerfEventLink>() {
        let name = link.link_info_name()?;
        return write_link_info(
            memory,
            crate::bpf::link_id_for_perf(link),
            link.program_id(),
            None,
            Some(link.link_info_data()?),
            name,
            attr.info,
            attr.info_len,
            attr_ptr,
            attr_size,
        );
    }
    if let Some(link) = fd_obj.downcast_ref::<BpfRawTracepointLink>() {
        let (name, cookie) = link.metadata()?;
        let mut data = [0u8; 48];
        data[16..24].copy_from_slice(&cookie.to_ne_bytes());
        return write_link_info(
            memory,
            crate::bpf::link_id_for_raw_tracepoint(link),
            link.program_id(),
            None,
            Some(data),
            Some(name),
            attr.info,
            attr.info_len,
            attr_ptr,
            attr_size,
        );
    }

    Err(AxError::InvalidInput)
}

fn write_link_info<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    id: Option<u32>,
    program: AxResult<u32>,
    network: Option<BpfNetworkLinkInfo>,
    extra: Option<[u8; 48]>,
    name: Option<&str>,
    info_ptr: u64,
    info_len: u32,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    let id = id.ok_or(AxError::NotFound)?;
    let (type_, registry_program) = crate::bpf::link_by_id(id)
        .ok_or(AxError::NotFound)?
        .link_type_and_program()?;
    let mut info = BpfLinkInfo {
        type_,
        id,
        prog_id: program?,
        ..Default::default()
    };
    match network {
        Some(BpfNetworkLinkInfo::Cgroup { id, attach_type }) => {
            info.data[..8].copy_from_slice(&id.to_ne_bytes());
            info.data[8..12].copy_from_slice(&attach_type.to_ne_bytes());
        }
        Some(BpfNetworkLinkInfo::Netfilter {
            pf,
            hook,
            priority,
            flags,
        }) => {
            info.data[..4].copy_from_slice(&pf.to_ne_bytes());
            info.data[4..8].copy_from_slice(&hook.to_ne_bytes());
            info.data[8..12].copy_from_slice(&priority.to_ne_bytes());
            info.data[12..16].copy_from_slice(&flags.to_ne_bytes());
        }
        Some(BpfNetworkLinkInfo::Xdp { ifindex }) => {
            info.data[..4].copy_from_slice(&ifindex.to_ne_bytes());
        }
        Some(BpfNetworkLinkInfo::Socket) | None => {}
    }
    if let Some(data) = extra {
        info.data = data;
    }
    if let Some(name) = name {
        // The raw-tracepoint and perf-tracepoint union arms carry an in/out
        // pointer and capacity.  Read them before overwriting the outer info
        // object, copy no more than the caller's capacity, then report the
        // full NUL-inclusive name length exactly as Linux does.
        const LINK_DATA_OFFSET: usize = offset_of!(BpfLinkInfo, data);
        const NAME_FIELDS: usize = size_of::<u64>() + size_of::<u32>();
        if (info_len as usize) >= LINK_DATA_OFFSET + NAME_FIELDS {
            let request = read_user_bytes(
                memory,
                (info_ptr as usize)
                    .checked_add(LINK_DATA_OFFSET)
                    .ok_or(AxError::BadAddress)?,
                NAME_FIELDS,
            )?;
            let pointer = u64::from_ne_bytes(request[..8].try_into().unwrap());
            let capacity = u32::from_ne_bytes(request[8..12].try_into().unwrap()) as usize;
            let full_len = name.len().checked_add(1).ok_or(AxError::OutOfRange)?;
            if pointer != 0 && capacity != 0 {
                let copied = core::cmp::min(capacity, full_len);
                let mut at = pointer as usize;
                let text = &name.as_bytes()[..core::cmp::min(copied, name.len())];
                if !text.is_empty() {
                    memory
                        .write_bytes(at, text)
                        .map_err(crate::mm::map_usercopy_error)?;
                    at = at.checked_add(text.len()).ok_or(AxError::BadAddress)?;
                }
                if copied > text.len() {
                    memory
                        .write_bytes(at, &[0])
                        .map_err(crate::mm::map_usercopy_error)?;
                }
            }
            info.data[..8].copy_from_slice(&pointer.to_ne_bytes());
            info.data[8..12].copy_from_slice(&(full_len as u32).to_ne_bytes());
        }
    }
    if info.prog_id != registry_program {
        return Err(AxError::NotFound);
    }
    let copied = (info_len as usize).min(size_of::<BpfLinkInfo>());
    memory
        .write_bytes(info_ptr as usize, &bytemuck::bytes_of(&info)[..copied])
        .map_err(crate::mm::map_usercopy_error)?;
    write_bpf_attr_value::<BpfAttrGetInfoByFd, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrGetInfoByFd, info_len),
        &(copied as u32),
    )?;
    Ok(0)
}

fn write_btf_info<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    btf_fd: &BpfBtfFd,
    info_ptr: u64,
    info_len: u32,
    attr_ptr: usize,
    attr_size: u32,
) -> AxResult<isize> {
    let object = &btf_fd.object;
    let info = BpfBtfInfo {
        btf_size: object
            .bytes
            .len()
            .try_into()
            .map_err(|_| AxError::NoMemory)?,
        id: object.id,
        ..Default::default()
    };
    let copied = (info_len as usize).min(size_of::<BpfBtfInfo>());
    memory
        .write_bytes(info_ptr as usize, &bytemuck::bytes_of(&info)[..copied])
        .map_err(crate::mm::map_usercopy_error)?;
    write_bpf_attr_value::<BpfAttrGetInfoByFd, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrGetInfoByFd, info_len),
        &(copied as u32),
    )?;
    Ok(0)
}

fn write_map_info<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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
        btf_id: map_fd.btf.as_ref().map_or(0, |object| object.id),
        ..Default::default()
    };

    let copy_len = (info_len as usize).min(size_of::<BpfMapInfo>());
    let info_bytes = bytemuck::bytes_of(&info);
    memory
        .write_bytes(info_ptr as usize, &info_bytes[..copy_len])
        .map_err(crate::mm::map_usercopy_error)?;

    write_bpf_attr_value::<BpfAttrGetInfoByFd, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrGetInfoByFd, info_len),
        &(copy_len as u32),
    )?;

    Ok(0)
}

fn write_prog_info<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
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
            prog.mechanism.instructions().as_ptr() as *const u8,
            core::mem::size_of_val(prog.mechanism.instructions()),
        )
    };
    // Simple FNV-1a hash truncated to 8 bytes
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in insn_bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    tag.copy_from_slice(&hash.to_ne_bytes());

    let xlated_len = core::mem::size_of_val(prog.mechanism.instructions()) as u32;

    let info = BpfProgInfo {
        type_: prog.prog_type,
        id: prog.prog_id,
        tag,
        jited_prog_len: 0, // no JIT
        xlated_prog_len: xlated_len,
        name: prog.name,
        gpl_compatible: prog.gpl_compatible as u32,
        nr_map_ids: prog.maps.len() as u32,
        run_time_ns: prog.run_time_ns.load(core::sync::atomic::Ordering::Relaxed),
        run_cnt: prog.run_cnt.load(core::sync::atomic::Ordering::Relaxed),
        ..Default::default()
    };

    let copy_len = (info_len as usize).min(size_of::<BpfProgInfo>());
    let info_bytes = bytemuck::bytes_of(&info);
    memory
        .write_bytes(info_ptr as usize, &info_bytes[..copy_len])
        .map_err(crate::mm::map_usercopy_error)?;

    write_bpf_attr_value::<BpfAttrGetInfoByFd, _, _>(
        memory,
        attr_ptr,
        attr_size,
        offset_of!(BpfAttrGetInfoByFd, info_len),
        &(copy_len as u32),
    )?;

    Ok(0)
}
