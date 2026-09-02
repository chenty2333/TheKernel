use alloc::{borrow::Cow, format, string::String, sync::Arc, vec::Vec};
use core::{
    ffi::{c_char, c_void},
    mem::{offset_of, size_of},
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{
    FatMountOptions, FsContext, NfsFilesystem, NfsMount, NfsMountOptions, NfsSecurityFlavor,
    OpenBlockDeviceError, OverlayFilesystem, OverlayMountOptions, OverlayTopology,
    PathwalkComponent, PathwalkPolicy, RpcAuth, RpcSysAuth, XfsFilesystem, XfsMountMembers,
    block_device_is_read_only, block_device_names, new_block_filesystem,
    new_block_filesystem_with_fat_options, new_btrfs_filesystem_with_members, open_block_device,
};
use axfs_ng_vfs::{
    DeviceId, DirEntry, ExportHandle, ExportHandleDecodeMode, ExportHandleMode, Filesystem,
    FilesystemOps, FsPath, FsPathBuf, Location, Mountpoint, NodePermission, NodeType, StatFs,
    VfsResult,
};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use axtask::current;
use bytemuck::{Pod, Zeroable};
use hashbrown::{HashMap, HashSet};
use linux_raw_sys::general::{CAP_SYS_ADMIN, mount_attr};
use thekernel_linux_mount::*;
use thekernel_linux_usercopy::{
    CopyStructError, UserMemory, UserMemoryContext, VmPtr, copy_struct_from_user, vm_load,
    vm_load_until_nul, vm_write_slice,
};

use crate::{
    file::{
        Directory, File, FileLike, Kstat, get_file_like, get_typed_file,
        permission::{
            SecurityFsContextExt, VfsSecurityContext,
            check_pathwalk_search_permission_with_vfs_security,
        },
        resolve_at_with_security, validate_pathname, with_path_fs,
    },
    mm::map_usercopy_error,
    mounts,
    pseudofs::{
        MemoryFs, ProcNamespaceKind, ProcNamespaceObject, ProcNamespaceTarget, cgroup,
        dev::fuse::{FuseConnection, FuseDeviceFile},
        namespace_target_from_proc_file, trace,
    },
    task::{
        AsThread, DacCredentialView, current_fs_context, fs_context_publication, ns_capable,
        try_tasks,
    },
};

fn current_mount_namespace() -> Arc<crate::task::MountNamespace> {
    current().as_thread().mount_ns()
}

/// Linux's `may_mount()` is scoped to the caller's current mount namespace,
/// not the initial user namespace and not a source path's retained topology.
fn may_mount(security: &VfsSecurityContext) -> bool {
    let mount_ns = current_mount_namespace();
    ns_capable(security.actor(), mount_ns.owner_user_ns(), CAP_SYS_ADMIN)
}

fn current_may_mount() -> bool {
    let curr = current();
    let mount_ns = curr.as_thread().mount_ns();
    ns_capable(
        &curr.as_thread().current_cred(),
        mount_ns.owner_user_ns(),
        CAP_SYS_ADMIN,
    )
}

/// One authoritative filesystem-type registry for both legacy mount and the
/// fsopen family.  A descriptor deliberately names a provider *kind* rather
/// than duplicating creation code: fsconfig/fsmount and mount(2) dispatch into
/// the same helpers below.
#[derive(Clone, Copy, Eq, PartialEq)]
enum FsContextOps {
    Pseudo,
    Block,
    Fuse,
    Nfs,
    Overlay,
}

#[derive(Clone, Copy)]
struct FilesystemType {
    name: &'static str,
    ops: FsContextOps,
}

const FILESYSTEM_TYPES: &[FilesystemType] = &[
    FilesystemType {
        name: "tmpfs",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "hugetlbfs",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "bpf",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "cgroup",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "cgroup2",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "tracefs",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "debugfs",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "mqueue",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "rpc_pipefs",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "proc",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "sysfs",
        ops: FsContextOps::Pseudo,
    },
    FilesystemType {
        name: "vfat",
        ops: FsContextOps::Block,
    },
    FilesystemType {
        name: "ext4",
        ops: FsContextOps::Block,
    },
    FilesystemType {
        name: "btrfs",
        ops: FsContextOps::Block,
    },
    FilesystemType {
        name: "xfs",
        ops: FsContextOps::Block,
    },
    FilesystemType {
        name: "fuse",
        ops: FsContextOps::Fuse,
    },
    FilesystemType {
        name: "nfs4",
        ops: FsContextOps::Nfs,
    },
    FilesystemType {
        name: "overlay",
        ops: FsContextOps::Overlay,
    },
];

fn filesystem_type(name: &str) -> Option<FilesystemType> {
    let canonical = match name {
        // The FAT driver exposes Linux's traditional spelling aliases while
        // retaining one provider and one fs-context implementation.
        "fat" | "msdos" => "vfat",
        _ => name,
    };
    FILESYSTEM_TYPES
        .iter()
        .copied()
        .find(|entry| entry.name == canonical)
}

/// Resolve a v6.18 `mnt_id_req.ns_id` before looking at a mount.  A caller
/// may query another live mount namespace only with CAP_SYS_ADMIN in that
/// namespace's owning user namespace; the topology itself remains isolated.
fn mount_namespace_for_request(req: MntIdReq) -> AxResult<Arc<crate::task::MountNamespace>> {
    validate_mnt_id_request(req).map_err(map_mount_uapi)?;
    let current_ns = current_mount_namespace();
    let mount_ns = if req.ns_id == 0 {
        if req.mnt_ns_fd == 0 {
            current_ns.clone()
        } else {
            if req.mnt_ns_fd > i32::MAX as u32 {
                return Err(AxError::BadFileDescriptor);
            }
            let file = get_file_like(req.mnt_ns_fd as i32)?
                .downcast::<File>()
                .map_err(|_| AxError::InvalidInput)?;
            let ProcNamespaceTarget::Live(
                ProcNamespaceKind::Mount,
                ProcNamespaceObject::Mount(mount_ns),
            ) = namespace_target_from_proc_file(file.inner().location())
            else {
                return Err(AxError::InvalidInput);
            };
            mount_ns
        }
    } else {
        crate::task::MountNamespace::lookup(req.ns_id)?
    };
    let cross_namespace = req.ns_id != 0 && mount_ns.id() != current_ns.id();
    let actor = current().as_thread().current_cred();
    if cross_namespace && !ns_capable(&actor, mount_ns.owner_user_ns(), CAP_SYS_ADMIN) {
        // ID-based lookup deliberately hides an otherwise live namespace
        // from an unauthorized caller.  An fd-selected namespace instead
        // carries its own possession authority and does not take this gate.
        return Err(AxError::NotFound);
    }
    Ok(mount_ns)
}

fn reconcile_current_mount_topology() -> AxResult<()> {
    current_mount_namespace().topology().reconcile_vfs_records()
}

// linux-raw-sys intentionally leaves its UAPI types without bytemuck marker
// traits.  Keep this local representation byte-for-byte identical so the
// shared versioned-structure copier can enforce Linux's tail-before-prefix
// fault ordering.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MountAttrUser {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

impl From<MountAttrUser> for mount_attr {
    fn from(value: MountAttrUser) -> Self {
        Self {
            attr_set: value.attr_set,
            attr_clr: value.attr_clr,
            propagation: value.propagation,
            userns_fd: value.userns_fd,
        }
    }
}

const _: () = assert!(size_of::<MountAttrUser>() == size_of::<mount_attr>());

fn copy_mount_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    source: *const mount_attr,
    size: usize,
) -> AxResult<mount_attr> {
    copy_struct_from_user::<_, MountAttrUser>(memory, source.cast(), size)
        .map(Into::into)
        .map_err(|error| match error {
            CopyStructError::UserCopy(_) => AxError::BadAddress,
            CopyStructError::NonZeroTrailing => LinuxError::E2BIG.into(),
        })
}

fn mount_setattr_is_noop(attr: mount_attr) -> bool {
    attr.attr_set == 0 && attr.attr_clr == 0 && attr.propagation == 0
}

/// Converts the ABI object into the namespace-local topology transaction.
/// The generic mount ABI intentionally does not own namespace FDs, so the
/// idmap half is resolved here before any VFS state is changed.
fn mount_setattr_request_with_replace(
    attr: mount_attr,
    idmap_replace: bool,
) -> AxResult<mounts::MountSetattrRequest> {
    let idmap_bit = MOUNT_ATTR_IDMAP as u64;
    if attr.attr_clr & idmap_bit != 0 && !idmap_replace
        || attr.attr_set & !(MOUNT_ATTR_SUPPORTED as u64) != 0
        || attr.attr_clr & !(MOUNT_ATTR_SUPPORTED as u64) != 0
        || attr.propagation.count_ones() > 1
        || attr.propagation & !(MS_PROPAGATION_FLAGS as u64) != 0
        || attr.userns_fd != 0 && attr.attr_set & idmap_bit == 0
    {
        return Err(AxError::InvalidInput);
    }
    // Preserve the shared ABI's exact atime and flag validation while leaving
    // propagation/idmap to the topology transaction.
    apply_mount_attr_flags(
        0,
        attr.attr_set & !idmap_bit,
        attr.attr_clr & !idmap_bit,
        0,
        0,
    )
    .map_err(map_mount_uapi)?;

    let idmap = if attr.attr_set & idmap_bit != 0 {
        if attr.userns_fd > i32::MAX as u64 {
            return Err(AxError::BadFileDescriptor);
        }
        let file = get_file_like(attr.userns_fd as i32)?
            .downcast::<File>()
            .map_err(|_| AxError::InvalidInput)?;
        let ProcNamespaceTarget::Live(ProcNamespaceKind::User, ProcNamespaceObject::User(user_ns)) =
            namespace_target_from_proc_file(file.inner().location())
        else {
            return Err(AxError::InvalidInput);
        };
        // A mount idmap is a persistent kernel credential mapping.  Store its
        // lower side in kernel-global (initial-user-namespace) IDs; the
        // caller-relative view is produced only when statmount renders it.
        let viewer = crate::task::security::initial_user_namespace(&user_ns);
        let uid = user_ns.try_uid_map_rows(&viewer)?;
        let gid = user_ns.try_gid_map_rows(&viewer)?;
        let uid = uid
            .iter()
            .map(|row| mounts::MountIdmapRange {
                inside: row.first,
                outside: row.lower_first,
                length: row.count,
            })
            .collect::<Vec<_>>();
        let gid = gid
            .iter()
            .map(|row| mounts::MountIdmapRange {
                inside: row.first,
                outside: row.lower_first,
                length: row.count,
            })
            .collect::<Vec<_>>();
        Some(Some(mounts::MountIdmap::try_new(user_ns, &uid, &gid)?))
    } else if attr.attr_clr & idmap_bit != 0 {
        Some(None)
    } else {
        None
    };
    Ok(mounts::MountSetattrRequest {
        attr_set: attr.attr_set & !idmap_bit,
        attr_clr: attr.attr_clr & !idmap_bit,
        propagation: attr.propagation,
        idmap,
        idmap_replace,
    })
}

fn mount_setattr_request(attr: mount_attr) -> AxResult<mounts::MountSetattrRequest> {
    mount_setattr_request_with_replace(attr, false)
}

fn read_mnt_id_req<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    req: *const u8,
) -> AxResult<MntIdReq> {
    let size = VmPtr::vm_read(req.cast::<u32>(), memory).map_err(map_usercopy_error)? as usize;
    if size < MNT_ID_REQ_SIZE_VER0 {
        return Err(LinuxError::EINVAL.into());
    }
    if size > PAGE_SIZE {
        return Err(LinuxError::E2BIG.into());
    }
    let copy_size = size.min(MNT_ID_REQ_SIZE_VER1);
    let src = vm_load(memory, req, copy_size).map_err(map_usercopy_error)?;
    let mut bytes = [0u8; MNT_ID_REQ_SIZE_VER1];
    bytes[..copy_size].copy_from_slice(&src);
    if size > MNT_ID_REQ_SIZE_VER1 {
        let extra = vm_load(
            memory,
            req.wrapping_add(MNT_ID_REQ_SIZE_VER1),
            size - MNT_ID_REQ_SIZE_VER1,
        )
        .map_err(map_usercopy_error)?;
        if extra.iter().any(|byte| *byte != 0) {
            return Err(LinuxError::E2BIG.into());
        }
    }
    MntIdReq::decode(&bytes[..copy_size]).map_err(|_| AxError::InvalidInput)
}

fn mount_point_under_root<'a>(root: &FsPath, target: &'a FsPath) -> Option<&'a FsPath> {
    let root = root.as_bytes();
    let target_bytes = target.as_bytes();
    if root == b"/" {
        return Some(target);
    }
    if target_bytes == root {
        return Some(FsPath::new(b"/"));
    }
    target_bytes
        .strip_prefix(root)
        .filter(|suffix| suffix.starts_with(b"/"))
        .map(FsPath::new)
}

fn current_mount_root() -> AxResult<FsPathBuf> {
    let root = current_fs_context()
        .lock()
        .root_dir()
        .absolute_path()
        .map_err(|_| AxError::Io)?;
    Ok(root)
}

fn visible_mount_point(target: &FsPath, root: &FsPath) -> AxResult<Option<FsPathBuf>> {
    mount_point_under_root(root, target)
        .map(|path| FsPathBuf::from_vec(path.as_bytes().to_vec()))
        .map(Ok)
        .transpose()
}

/// Select the root from which {stat,list}mount may observe a namespace.
///
/// Linux keeps a namespace-owned top-level mount which is not itself the
/// foreign caller's visible root.  `grab_requested_root()` selects the first
/// direct child in mount-ID order.  Our topology ledger represents that
/// top-level mount explicitly, so mirror the same selection here instead of
/// granting an nsfs descriptor an unrestricted `/` view.
fn requested_namespace_root(
    mount_ns: &Arc<crate::task::MountNamespace>,
    topology: &mounts::MountTopologySnapshot,
) -> AxResult<(FsPathBuf, Option<u64>)> {
    if mount_ns.id() == current_mount_namespace().id() {
        return Ok((current_mount_root()?, None));
    }
    let top = topology
        .mounts
        .iter()
        .find(|mount| mount.parent.is_none())
        .ok_or(AxError::NotFound)?;
    let visible = topology
        .mounts
        .iter()
        .filter(|mount| mount.parent == Some(top.id))
        .min_by_key(|mount| mount.id)
        .ok_or(AxError::NotFound)?;
    Ok((visible.target.clone(), Some(visible.id)))
}

fn append_statmount_bytes(bytes: &mut Vec<u8>, value: &[u8]) -> AxResult<u32> {
    // Linux reserves str[0] as the empty-string sentinel, so an unset offset
    // remains distinguishable from the first populated string.
    if bytes.len() == STATMOUNT_PREFIX_SIZE {
        bytes.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        bytes.push(0);
    }
    let offset = u32::try_from(bytes.len() - STATMOUNT_PREFIX_SIZE)
        .map_err(|_| AxError::from(LinuxError::EOVERFLOW))?;
    bytes
        .try_reserve(value.len() + 1)
        .map_err(|_| AxError::NoMemory)?;
    bytes.extend_from_slice(value);
    bytes.push(0);
    Ok(offset)
}

fn append_statmount_string(bytes: &mut Vec<u8>, value: &str) -> AxResult<u32> {
    append_statmount_bytes(bytes, value.as_bytes())
}

fn append_statmount_option_array(bytes: &mut Vec<u8>, options: &str) -> AxResult<(u32, u32)> {
    if options.is_empty() {
        return Ok((0, 0));
    }
    let offset = {
        // The individual option strings share the ordinary statmount string
        // area, but are not comma-separated in the UAPI representation.
        if bytes.len() == STATMOUNT_PREFIX_SIZE {
            bytes.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            bytes.push(0);
        }
        u32::try_from(bytes.len() - STATMOUNT_PREFIX_SIZE)
            .map_err(|_| AxError::from(LinuxError::EOVERFLOW))?
    };
    let mut count = 0u32;
    for option in options.split(',').filter(|option| !option.is_empty()) {
        bytes
            .try_reserve(option.len() + 1)
            .map_err(|_| AxError::NoMemory)?;
        bytes.extend_from_slice(option.as_bytes());
        bytes.push(0);
        count = count.checked_add(1).ok_or(LinuxError::EOVERFLOW)?;
    }
    Ok((offset, count))
}

fn append_statmount_idmap(
    bytes: &mut Vec<u8>,
    ranges: &[crate::task::IdMapInputExtent],
) -> AxResult<(u32, u32)> {
    if ranges.is_empty() {
        return Ok((0, 0));
    }
    let offset = if bytes.len() == STATMOUNT_PREFIX_SIZE {
        bytes.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        bytes.push(0);
        1
    } else {
        u32::try_from(bytes.len() - STATMOUNT_PREFIX_SIZE)
            .map_err(|_| AxError::from(LinuxError::EOVERFLOW))?
    };
    for range in ranges {
        // Render the mount-idmap user namespace through the syscall caller's
        // current user namespace, as Linux's statmount_mnt_idmap() does.
        let row = format!("{} {} {}", range.first, range.lower_first, range.count);
        bytes
            .try_reserve(row.len() + 1)
            .map_err(|_| AxError::NoMemory)?;
        bytes.extend_from_slice(row.as_bytes());
        bytes.push(0);
    }
    Ok((
        offset,
        u32::try_from(ranges.len()).map_err(|_| AxError::from(LinuxError::EOVERFLOW))?,
    ))
}

fn put_statmount<T: bytemuck::NoUninit>(bytes: &mut [u8], offset: usize, value: T) {
    bytes[offset..offset + size_of::<T>()].copy_from_slice(bytemuck::bytes_of(&value));
}

fn mount_options(data: &str) -> AxResult<String> {
    // STATMOUNT_MNT_OPTS is the filesystem's show_options output.  Per-mount
    // policy (ro/nosuid/nodev/noexec) is reported separately in mnt_attr.
    try_string(data)
}

const STATMOUNT_STRING_REQ: u64 = STATMOUNT_MNT_ROOT
    | STATMOUNT_MNT_POINT
    | STATMOUNT_FS_TYPE
    | STATMOUNT_MNT_OPTS
    | STATMOUNT_FS_SUBTYPE
    | STATMOUNT_SB_SOURCE
    | STATMOUNT_OPT_ARRAY
    | STATMOUNT_OPT_SEC_ARRAY
    | STATMOUNT_MNT_UIDMAP
    | STATMOUNT_MNT_GIDMAP;

pub fn sys_statmount<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    req: *const u8,
    buf: *mut u8,
    bufsize: usize,
    flags: u32,
) -> AxResult<isize> {
    validate_statmount_flags(flags).map_err(map_mount_uapi)?;
    let req = read_mnt_id_req(memory, req)?;
    let mount_ns = mount_namespace_for_request(req)?;
    memory
        .validate_write_range(buf as usize, bufsize)
        .map_err(map_usercopy_error)?;
    let requested = req.param;
    let mask = requested & STATMOUNT_SUPPORTED;
    let requested_strings = mask & STATMOUNT_STRING_REQ != 0;
    // prepare_kstatmount() has one deliberately narrow rejection: exactly a
    // fixed-size buffer cannot carry the string stream.  A shorter buffer is
    // still a valid short-prefix request when every requested provider string
    // is empty (the reserved zero-offset sentinel is copied separately).
    if requested_strings && bufsize == STATMOUNT_PREFIX_SIZE {
        return Err(LinuxError::EOVERFLOW.into());
    }
    let _mount_operation = mounts::namespace_operation();
    let topology = mount_ns.topology().try_snapshot()?;
    let (fs_root, _) = requested_namespace_root(&mount_ns, &topology)?;
    let mount = topology
        .mounts
        .iter()
        .find(|mount| mount.id == req.mnt_id)
        .ok_or(AxError::NotFound)?;
    let actor = current().as_thread().current_cred();
    let visible_point = match visible_mount_point(&mount.target, &fs_root)? {
        Some(point) => Some(point),
        // Capability admits the query, but seq_path_root() still skips an
        // unreachable pathname.  Leave MNT_POINT and its mask bit unset.
        None if ns_capable(&actor, mount_ns.owner_user_ns(), CAP_SYS_ADMIN) => None,
        None => return Err(LinuxError::EPERM.into()),
    };
    let mut returned_mask = 0u64;
    let mut output = Vec::new();
    output
        .try_reserve(STATMOUNT_PREFIX_SIZE)
        .map_err(|_| AxError::NoMemory)?;
    output.resize(STATMOUNT_PREFIX_SIZE, 0);
    if requested_strings {
        output.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        // str[0] is the universal unset-offset sentinel, including when every
        // requested provider string is empty.
        output.push(0);
    }
    let dev = DeviceId(mount.dev);
    if mask & STATMOUNT_SB_BASIC != 0 {
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, sb_dev_major),
            dev.major(),
        );
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, sb_dev_minor),
            dev.minor(),
        );
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, sb_magic),
            mount
                .mountpoint()?
                .root_location()
                .filesystem()
                .stat()?
                .fs_type as u64,
        );
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, sb_flags),
            topology
                .mounts
                .iter()
                .find(|candidate| candidate.id == mount.id)
                .map_or(mount.flags, |candidate| candidate.flags)
                & (MS_RDONLY | MS_SYNCHRONOUS | MS_DIRSYNC | MS_LAZYTIME),
        );
        returned_mask |= STATMOUNT_SB_BASIC;
    }
    if mask & STATMOUNT_MNT_BASIC != 0 {
        put_statmount(&mut output, offset_of!(StatmountPrefix, mnt_id), mount.id);
        returned_mask |= STATMOUNT_MNT_BASIC;
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, mnt_parent_id),
            mount.parent.unwrap_or(mount.id),
        );
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, mnt_id_old),
            mount.mount_id_old,
        );
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, mnt_parent_id_old),
            mount
                .parent
                .and_then(|parent| {
                    topology
                        .mounts
                        .iter()
                        .find(|candidate| candidate.id == parent)
                })
                .map_or(mount.mount_id_old, |parent| parent.mount_id_old),
        );
    }
    if mask & STATMOUNT_PROPAGATE_FROM != 0 {
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, propagate_from),
            mount.peer_group.and_then(|group| group.master).unwrap_or(0),
        );
        returned_mask |= STATMOUNT_PROPAGATE_FROM;
    }
    if mask & STATMOUNT_MNT_NS_ID != 0 {
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, mnt_ns_id),
            mount_ns.id(),
        );
        returned_mask |= STATMOUNT_MNT_NS_ID;
    }
    if mask & STATMOUNT_MNT_BASIC != 0 {
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, mnt_attr),
            statmount_attr(
                topology
                    .mounts
                    .iter()
                    .find(|candidate| candidate.id == mount.id)
                    .map_or(mount.flags, |candidate| candidate.flags),
            ),
        );
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, mnt_propagation),
            topology
                .mounts
                .iter()
                .find(|candidate| candidate.id == mount.id)
                .map(|candidate| candidate.propagation())
                .unwrap_or(MS_PRIVATE as u64),
        );
        let peer = mount.peer_group;
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, mnt_peer_group),
            peer.map_or(0, |group| group.id),
        );
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, mnt_master),
            peer.and_then(|group| group.master).unwrap_or(0),
        );
    }
    // Keep the variable stream byte-for-byte ordered like Linux v6.18's
    // do_statmount(): offsets are observable even though fields are
    // independently addressable.
    if mask & STATMOUNT_FS_TYPE != 0 {
        let offset = append_statmount_bytes(&mut output, mount.fs_type.as_bytes())?;
        put_statmount(&mut output, offset_of!(StatmountPrefix, fs_type), offset);
        returned_mask |= STATMOUNT_FS_TYPE;
    }
    if mask & STATMOUNT_MNT_ROOT != 0 {
        let offset = append_statmount_bytes(&mut output, mount.root.as_bytes())?;
        put_statmount(&mut output, offset_of!(StatmountPrefix, mnt_root), offset);
        returned_mask |= STATMOUNT_MNT_ROOT;
    }
    if mask & STATMOUNT_MNT_POINT != 0
        && let Some(visible_point) = visible_point.as_ref()
    {
        let offset = append_statmount_bytes(&mut output, visible_point.as_bytes())?;
        put_statmount(&mut output, offset_of!(StatmountPrefix, mnt_point), offset);
        returned_mask |= STATMOUNT_MNT_POINT;
    }
    if mask & STATMOUNT_MNT_OPTS != 0 {
        let options = mount_options(&mount.data)?;
        if !options.is_empty() {
            let offset = append_statmount_string(&mut output, &options)?;
            put_statmount(&mut output, offset_of!(StatmountPrefix, mnt_opts), offset);
            returned_mask |= STATMOUNT_MNT_OPTS;
        }
    }
    if mask & STATMOUNT_OPT_ARRAY != 0 {
        let (offset, count) = append_statmount_option_array(&mut output, &mount.data)?;
        if count != 0 {
            put_statmount(&mut output, offset_of!(StatmountPrefix, opt_array), offset);
            put_statmount(&mut output, offset_of!(StatmountPrefix, opt_num), count);
            returned_mask |= STATMOUNT_OPT_ARRAY;
        }
    }
    // No provider emitted security options, so Linux leaves OPT_SEC_ARRAY
    // fields and its return-mask bit clear.
    // statmount_string() likewise leaves both FS_SUBTYPE fields clear when
    // the provider emits no subtype; no registered provider currently does.
    if mask & STATMOUNT_SB_SOURCE != 0 && !mount.source.as_bytes().is_empty() {
        let offset = append_statmount_bytes(&mut output, mount.source.as_bytes())?;
        put_statmount(&mut output, offset_of!(StatmountPrefix, sb_source), offset);
        returned_mask |= STATMOUNT_SB_SOURCE;
    }
    if let Some(idmap) = mount.idmap.as_ref() {
        let viewer = current().as_thread().current_cred().user_ns().clone();
        if mask & STATMOUNT_MNT_UIDMAP != 0 {
            let rows = idmap.user_namespace().try_uid_map_rows(&viewer)?;
            let (offset, count) = append_statmount_idmap(&mut output, &rows)?;
            put_statmount(&mut output, offset_of!(StatmountPrefix, mnt_uidmap), offset);
            put_statmount(
                &mut output,
                offset_of!(StatmountPrefix, mnt_uidmap_num),
                count,
            );
            // A valid idmapped mount sets the bit even when no row is visible
            // from the caller's user namespace.
            returned_mask |= STATMOUNT_MNT_UIDMAP;
        }
        if mask & STATMOUNT_MNT_GIDMAP != 0 {
            let rows = idmap.user_namespace().try_gid_map_rows(&viewer)?;
            let (offset, count) = append_statmount_idmap(&mut output, &rows)?;
            put_statmount(&mut output, offset_of!(StatmountPrefix, mnt_gidmap), offset);
            put_statmount(
                &mut output,
                offset_of!(StatmountPrefix, mnt_gidmap_num),
                count,
            );
            returned_mask |= STATMOUNT_MNT_GIDMAP;
        }
    }
    if mask & STATMOUNT_SUPPORTED_MASK != 0 {
        put_statmount(
            &mut output,
            offset_of!(StatmountPrefix, supported_mask),
            STATMOUNT_SUPPORTED,
        );
        returned_mask |= STATMOUNT_SUPPORTED_MASK;
    }
    put_statmount(
        &mut output,
        offset_of!(StatmountPrefix, mask),
        returned_mask,
    );
    let tail_len = output.len().saturating_sub(STATMOUNT_PREFIX_SIZE);
    let copied_prefix = bufsize.min(STATMOUNT_PREFIX_SIZE);
    let size = u32::try_from(copied_prefix.saturating_add(tail_len))
        .map_err(|_| AxError::from(LinuxError::EOVERFLOW))?;
    put_statmount(&mut output, offset_of!(StatmountPrefix, size), size);
    // An emitted string is accepted only when its terminating byte fits.  The
    // lone reserved sentinel is special: Linux copies it to `buf +
    // sizeof(statmount)` even for a short-prefix buffer, and then copies the
    // fixed prefix.  Preserve that usercopy order and resulting EFAULT.
    if tail_len > 1 && bufsize < output.len() {
        return Err(LinuxError::EOVERFLOW.into());
    }
    if tail_len != 0 {
        let tail = buf.wrapping_add(STATMOUNT_PREFIX_SIZE);
        vm_write_slice(memory, tail, &output[STATMOUNT_PREFIX_SIZE..])
            .map_err(map_usercopy_error)?;
    }
    vm_write_slice(memory, buf, &output[..copied_prefix]).map_err(map_usercopy_error)?;
    Ok(0)
}

pub fn sys_listmount<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    req: *const u8,
    ids: *mut u64,
    nr_ids: usize,
    flags: u32,
) -> AxResult<isize> {
    let reverse = validate_listmount_flags(flags).map_err(map_mount_uapi)?;
    if nr_ids > 1_000_000 {
        return Err(LinuxError::EOVERFLOW.into());
    }
    let bytes = nr_ids
        .checked_mul(size_of::<u64>())
        .ok_or(LinuxError::EOVERFLOW)?;
    memory
        .validate_write_range(ids as usize, bytes)
        .map_err(map_usercopy_error)?;
    let req = read_mnt_id_req(memory, req)?;
    validate_mnt_id_request(req).map_err(map_mount_uapi)?;
    if req.param != 0 {
        validate_unique_mount_id(req.param).map_err(map_mount_uapi)?;
    }
    let mount_ns = mount_namespace_for_request(req)?;
    let _mount_operation = mounts::namespace_operation();
    let topology = mount_ns.topology().try_snapshot()?;
    let (fs_root, foreign_visible_root) = requested_namespace_root(&mount_ns, &topology)?;
    let root_id = if req.mnt_id == LSMT_ROOT {
        foreign_visible_root.or_else(|| {
            topology
                .mounts
                .iter()
                .find(|mount| mount.parent.is_none())
                .map(|mount| mount.id)
        })
    } else {
        Some(req.mnt_id)
    }
    .ok_or(AxError::NotFound)?;
    if req.mnt_id != LSMT_ROOT {
        validate_unique_mount_id(req.mnt_id).map_err(map_mount_uapi)?;
    }
    if !topology.mounts.iter().any(|mount| mount.id == root_id) {
        return Err(AxError::NotFound);
    }
    let root_visible = visible_mount_point(
        &topology
            .mounts
            .iter()
            .find(|mount| mount.id == root_id)
            .ok_or(AxError::Io)?
            .target,
        &fs_root,
    )?
    .is_some();
    let outside_view_with_capability = if req.mnt_id != LSMT_ROOT && !root_visible {
        let actor = current().as_thread().current_cred();
        if !ns_capable(&actor, mount_ns.owner_user_ns(), CAP_SYS_ADMIN) {
            return Err(LinuxError::EPERM.into());
        }
        true
    } else {
        false
    };
    let mut selected = Vec::new();
    selected
        .try_reserve(topology.mounts.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut pending = Vec::new();
    pending
        .try_reserve(topology.mounts.len())
        .map_err(|_| AxError::NoMemory)?;
    pending.push(root_id);
    while let Some(parent) = pending.pop() {
        let mount = topology
            .mounts
            .iter()
            .find(|mount| mount.id == parent)
            .ok_or(AxError::Io)?;
        if (req.mnt_id == LSMT_ROOT || parent != root_id)
            && (outside_view_with_capability
                || visible_mount_point(&mount.target, &fs_root)?.is_some())
        {
            selected.push(parent);
        }
        for child in topology
            .mounts
            .iter()
            .filter(|mount| mount.parent == Some(parent))
        {
            pending.push(child.id);
        }
    }
    selected.sort_unstable();
    if reverse {
        selected.reverse();
    }
    let start = req.param;
    selected.retain(|id| {
        if reverse {
            start == 0 || *id < start
        } else {
            *id > start
        }
    });
    selected.truncate(nr_ids);
    vm_write_slice(memory, ids, &selected).map_err(map_usercopy_error)?;
    Ok(selected.len() as isize)
}

#[cfg(test)]
mod statmount_tests {
    use super::*;

    #[test]
    fn linux_618_mask_includes_option_arrays_supported_mask_and_idmaps() {
        assert_eq!(STATMOUNT_SUPPORTED, 0x7fff);
    }

    #[test]
    fn unique_mount_id_floor_is_not_a_lookup_miss() {
        assert!(validate_unique_mount_id(1u64 << 31).is_err());
        assert!(validate_unique_mount_id((1u64 << 31) + 1).is_ok());
    }

    #[test]
    fn mount_options_preserve_mount_policy() {
        assert_eq!(mount_options("").unwrap(), "");
        assert_eq!(
            mount_options("journal_checksum").unwrap(),
            "journal_checksum"
        );
    }

    #[test]
    fn chroot_mount_points_are_relative_and_do_not_escape() {
        assert_eq!(
            mount_point_under_root(FsPath::new(b"/jail"), FsPath::new(b"/jail")),
            Some(FsPath::new(b"/"))
        );
        assert_eq!(
            mount_point_under_root(FsPath::new(b"/jail"), FsPath::new(b"/jail/tmp")),
            Some(FsPath::new(b"/tmp"))
        );
        assert_eq!(
            mount_point_under_root(FsPath::new(b"/jail"), FsPath::new(b"/jailbreak")),
            None
        );
        assert_eq!(
            mount_point_under_root(FsPath::new(b"/jail"), FsPath::new(b"/")),
            None
        );
    }
}

/// Pathname arguments are opaque Linux byte strings.  Keep them that way all
/// the way to the VFS; only mount type/options are text protocols.
fn load_user_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const c_char,
) -> AxResult<FsPathBuf> {
    let path = FsPathBuf::from_vec(
        vm_load_until_nul(memory, ptr.cast::<u8>()).map_err(map_usercopy_error)?,
    );
    validate_pathname(path.as_ref())?;
    Ok(path)
}

/// Mount option, filesystem-type, and legacy source-name grammars are textual
/// UAPIs.  This conversion is deliberately not used for a pathname walk.
fn load_user_string<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const c_char,
) -> AxResult<String> {
    String::from_utf8(vm_load_until_nul(memory, ptr.cast::<u8>()).map_err(map_usercopy_error)?)
        .map_err(|_| AxError::IllegalBytes)
}

fn try_string(value: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Ok(owned)
}

struct FsOpenState {
    fs_type: String,
    source: Option<FsPathBuf>,
    data: String,
    config_len: usize,
    created: bool,
    /// `/dev/fuse` is an OFD, so fsconfig retains its connection directly;
    /// a numeric fd must never be re-resolved after the caller closes or
    /// reuses that descriptor.
    fuse_connection: Option<Arc<FuseConnection>>,
    /// NFS retains a typed TCP OFD for the complete superblock lifetime; a
    /// source string is descriptive metadata only and is never re-resolved.
    nfs_transport: Option<Arc<crate::nfs_transport::NfsSocketTransport>>,
    nfs_options: NfsMountOptions,
    overlay: Option<OverlayMountOptions>,
    /// Typed fsconfig values outlive userspace buffers.  Providers consume
    /// these resolved values at create/reconfigure instead of reparsing a
    /// pathname or silently discarding a binary option.
    binary: Vec<(String, Vec<u8>)>,
    paths: Vec<(String, axfs_ng_vfs::Location)>,
    reconfigure_mount: Option<axfs_ng_vfs::Location>,
}

/// One immutable lower mount-view snapshot retained by an overlay superblock.
/// The mount id is the identity used by every copied-up lower `Location`.
struct OverlayLowerIdmap {
    mount_id: u64,
    idmap: Option<Arc<mounts::MountIdmap>>,
}

/// Provider-local copy-up projection.  Overlay ids are never projected
/// through an arbitrary "first lower": each lower view is translated to the
/// kernel id space, then through the upper view selected in the same topology
/// snapshot used for mount admission.
struct OverlayMountIdMapper {
    lower: Vec<OverlayLowerIdmap>,
    upper: Option<Arc<mounts::MountIdmap>>,
}

fn idmap_view_to_kernel(
    value: u32,
    idmap: Option<&mounts::MountIdmap>,
    uid: bool,
) -> VfsResult<u32> {
    let Some(idmap) = idmap else {
        return Ok(value);
    };
    let rows = if uid { &idmap.uid } else { &idmap.gid };
    rows.iter()
        .find_map(|row| {
            let end = row.inside.checked_add(row.length)?;
            (value >= row.inside && value < end)
                .then_some(row.outside.checked_add(value - row.inside))
                .flatten()
        })
        .ok_or(axfs_ng_vfs::VfsError::InvalidInput)
}

fn idmap_kernel_to_view(
    value: u32,
    idmap: Option<&mounts::MountIdmap>,
    uid: bool,
) -> VfsResult<u32> {
    let Some(idmap) = idmap else {
        return Ok(value);
    };
    let rows = if uid { &idmap.uid } else { &idmap.gid };
    rows.iter()
        .find_map(|row| {
            let end = row.outside.checked_add(row.length)?;
            (value >= row.outside && value < end)
                .then_some(row.inside.checked_add(value - row.outside))
                .flatten()
        })
        .ok_or(axfs_ng_vfs::VfsError::InvalidInput)
}

impl axfs::OverlayIdMapper for OverlayMountIdMapper {
    fn lower_uid_to_upper(&self, lower: &axfs_ng_vfs::Location, uid: u32) -> VfsResult<u32> {
        let lower = self
            .lower
            .iter()
            .find(|entry| entry.mount_id == lower.mountpoint().mount_id())
            .ok_or(axfs_ng_vfs::VfsError::InvalidInput)?;
        let kernel = idmap_view_to_kernel(uid, lower.idmap.as_deref(), true)?;
        idmap_kernel_to_view(kernel, self.upper.as_deref(), true)
    }
    fn lower_gid_to_upper(&self, lower: &axfs_ng_vfs::Location, gid: u32) -> VfsResult<u32> {
        let lower = self
            .lower
            .iter()
            .find(|entry| entry.mount_id == lower.mountpoint().mount_id())
            .ok_or(axfs_ng_vfs::VfsError::InvalidInput)?;
        let kernel = idmap_view_to_kernel(gid, lower.idmap.as_deref(), false)?;
        idmap_kernel_to_view(kernel, self.upper.as_deref(), false)
    }
    fn lower_kernel_uid_to_visible(
        &self,
        lower: &axfs_ng_vfs::Location,
        uid: u32,
    ) -> VfsResult<u32> {
        let lower = self
            .lower
            .iter()
            .find(|entry| entry.mount_id == lower.mountpoint().mount_id())
            .ok_or(axfs_ng_vfs::VfsError::InvalidInput)?;
        idmap_kernel_to_view(uid, lower.idmap.as_deref(), true)
    }
    fn lower_kernel_gid_to_visible(
        &self,
        lower: &axfs_ng_vfs::Location,
        gid: u32,
    ) -> VfsResult<u32> {
        let lower = self
            .lower
            .iter()
            .find(|entry| entry.mount_id == lower.mountpoint().mount_id())
            .ok_or(axfs_ng_vfs::VfsError::InvalidInput)?;
        idmap_kernel_to_view(gid, lower.idmap.as_deref(), false)
    }
    fn upper_visible_uid_to_kernel(&self, uid: u32) -> VfsResult<u32> {
        idmap_view_to_kernel(uid, self.upper.as_deref(), true)
    }
    fn upper_visible_gid_to_kernel(&self, gid: u32) -> VfsResult<u32> {
        idmap_view_to_kernel(gid, self.upper.as_deref(), false)
    }
}

fn overlay_mapper_for(
    lower: &[axfs_ng_vfs::Location],
    upper: Option<&axfs_ng_vfs::Location>,
    work: Option<&axfs_ng_vfs::Location>,
) -> AxResult<Arc<OverlayMountIdMapper>> {
    let snapshot = current_mount_namespace().topology().try_snapshot()?;
    let find = |location: &axfs_ng_vfs::Location| -> AxResult<&mounts::Mount> {
        let mount = snapshot
            .mounts
            .iter()
            .find(|mount| mount.id == location.mountpoint().mount_id())
            .ok_or(AxError::NotFound)?;
        if !Arc::ptr_eq(&mount.mountpoint()?, location.mountpoint()) {
            return Err(AxError::NotFound);
        }
        Ok(mount)
    };
    let mut lower_maps = Vec::new();
    lower_maps
        .try_reserve_exact(lower.len())
        .map_err(|_| AxError::NoMemory)?;
    for location in lower {
        let mount = find(location)?;
        lower_maps.push(OverlayLowerIdmap {
            mount_id: mount.id,
            idmap: mount.idmap.clone(),
        });
    }
    let upper_mount = upper.map(find).transpose()?;
    if let (Some(upper), Some(work)) = (upper_mount, work.map(find).transpose()?) {
        // This compares the namespace ledger's shared superblock object, not
        // incidental device numbers.  It remains true for separate mount
        // instances of one filesystem and false for lookalike devices.
        if upper.superblock.identity != work.superblock.identity
            || upper.superblock.fs_type != work.superblock.fs_type
        {
            return Err(AxError::InvalidInput);
        }
    }
    Arc::try_new(OverlayMountIdMapper {
        lower: lower_maps,
        upper: upper_mount.and_then(|mount| mount.idmap.clone()),
    })
    .map_err(|_| AxError::NoMemory)
}

fn resolve_overlay_filesystem(
    options: &OverlayMountOptions,
    configured_paths: &[(String, axfs_ng_vfs::Location)],
    security: &VfsSecurityContext,
) -> AxResult<Filesystem> {
    options.validate_shape().map_err(AxError::from)?;
    let mut lower = Vec::new();
    lower
        .try_reserve(options.lowerdirs.len())
        .map_err(|_| AxError::NoMemory)?;
    for (_, location) in configured_paths.iter().filter(|(key, _)| key == "lowerdir") {
        lower.push(location.clone());
    }
    if lower.is_empty() {
        for path in &options.lowerdirs {
            lower.push(
                resolve_at_with_security(
                    linux_raw_sys::general::AT_FDCWD,
                    Some(path.as_ref()),
                    0,
                    security,
                )?
                .into_file()
                .ok_or(AxError::InvalidInput)?,
            );
        }
    }
    let upper =
        if let Some((_, location)) = configured_paths.iter().find(|(key, _)| key == "upperdir") {
            Some(location.clone())
        } else if let Some(path) = options.upperdir.as_ref() {
            Some(
                resolve_at_with_security(
                    linux_raw_sys::general::AT_FDCWD,
                    Some(path.as_ref()),
                    0,
                    security,
                )?
                .into_file()
                .ok_or(AxError::InvalidInput)?,
            )
        } else {
            None
        };
    let work =
        if let Some((_, location)) = configured_paths.iter().find(|(key, _)| key == "workdir") {
            Some(location.clone())
        } else if let Some(path) = options.workdir.as_ref() {
            Some(
                resolve_at_with_security(
                    linux_raw_sys::general::AT_FDCWD,
                    Some(path.as_ref()),
                    0,
                    security,
                )?
                .into_file()
                .ok_or(AxError::InvalidInput)?,
            )
        } else {
            None
        };
    let mapper = overlay_mapper_for(&lower, upper.as_ref(), work.as_ref())?;
    OverlayFilesystem::new(OverlayTopology::try_new_with_id_mapper(
        options, lower, upper, work, mapper,
    )?)
    .map_err(AxError::from)
}

fn legacy_overlay_options(data: &[u8]) -> AxResult<OverlayMountOptions> {
    let mut options = OverlayMountOptions::empty();
    if data.is_empty() {
        return Err(AxError::InvalidInput);
    }
    for option in data.split(|byte| *byte == b',') {
        let Some(separator) = option.iter().position(|byte| *byte == b'=') else {
            return Err(AxError::InvalidInput);
        };
        let (key, value) = (&option[..separator], &option[separator + 1..]);
        options.set_option(key, value).map_err(AxError::from)?;
    }
    Ok(options)
}

fn overlay_options_from_record(data: &str) -> AxResult<OverlayMountOptions> {
    let raw = decode_overlay_record_data(data.as_bytes())?;
    legacy_overlay_options(&raw)
}

/// Mount records are currently textual, whereas legacy overlay option values
/// are raw bytes.  Retain a reversible canonical spelling in the ledger so a
/// remount/statmount/fspick never observes an empty option set merely because
/// a layer name was non-UTF-8.  Ordinary printable bytes remain unchanged;
/// every other byte (including a backslash) is written as `\\xNN`.
fn overlay_record_data(data: &[u8]) -> AxResult<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut record = String::new();
    for byte in data {
        if matches!(*byte, b' '..=b'~') && *byte != b'\\' {
            record.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            record.push(*byte as char);
        } else {
            record.try_reserve(4).map_err(|_| AxError::NoMemory)?;
            record.push('\\');
            record.push('x');
            record.push(HEX[usize::from(*byte >> 4)] as char);
            record.push(HEX[usize::from(*byte & 0x0f)] as char);
        }
    }
    Ok(record)
}

fn overlay_options_record_data(options: &OverlayMountOptions) -> AxResult<String> {
    let mut raw = Vec::new();
    let mut push = |bytes: &[u8]| -> AxResult<()> {
        raw.try_reserve(bytes.len())
            .map_err(|_| AxError::NoMemory)?;
        raw.extend_from_slice(bytes);
        Ok(())
    };
    push(b"lowerdir=")?;
    for (index, path) in options.lowerdirs.iter().enumerate() {
        if index != 0 {
            push(b":")?;
        }
        for byte in path.as_bytes() {
            if matches!(*byte, b':' | b'\\') {
                push(b"\\")?;
            }
            push(core::slice::from_ref(byte))?;
        }
    }
    if let Some(path) = &options.upperdir {
        push(b",upperdir=")?;
        push(path.as_bytes())?;
    }
    if let Some(path) = &options.workdir {
        push(b",workdir=")?;
        push(path.as_bytes())?;
    }
    for (name, enabled) in [
        (b"redirect_dir".as_slice(), options.features.redirect_dir),
        (b"index".as_slice(), options.features.index),
        (b"xino".as_slice(), options.features.xino),
        (b"metacopy".as_slice(), options.features.metacopy),
        (b"nfs_export".as_slice(), options.features.nfs_export),
        (b"volatile".as_slice(), options.features.volatile),
    ] {
        if enabled {
            push(b",")?;
            push(name)?;
            push(b"=on")?;
        }
    }
    overlay_record_data(&raw)
}

fn decode_overlay_record_data(data: &[u8]) -> AxResult<Vec<u8>> {
    let mut raw = Vec::new();
    raw.try_reserve(data.len()).map_err(|_| AxError::NoMemory)?;
    let mut index = 0;
    while index < data.len() {
        if data[index] == b'\\' && data.get(index + 1) == Some(&b'x') {
            let high = data
                .get(index + 2)
                .and_then(|byte| (*byte as char).to_digit(16))
                .ok_or(AxError::InvalidInput)?;
            let low = data
                .get(index + 3)
                .and_then(|byte| (*byte as char).to_digit(16))
                .ok_or(AxError::InvalidInput)?;
            raw.push(u8::try_from((high << 4) | low).map_err(|_| AxError::InvalidInput)?);
            index += 4;
        } else {
            raw.push(data[index]);
            index += 1;
        }
    }
    Ok(raw)
}

// Recovery replaces NfsMount's active transport.  Keep the original typed
// transport alive as the mount-registration lifetime token so an fsopen FD
// closing after recovery cannot prune the rpc_pipefs client through a stale
// Weak reference.
static NFS_MOUNT_TRANSPORTS: Mutex<
    Vec<(
        u64,
        Arc<crate::nfs_transport::NfsSocketTransport>,
        NfsMountOptions,
        axfs_ng_vfs::FsNameBuf,
    )>,
> = Mutex::new(Vec::new());
/// NFS registrations are namespace-mount-instance state.  Keep a removal
/// reservation while a cross-namespace unmount is prepared so two receipts
/// cannot both retire the same rpc_pipefs client.
static NFS_PENDING_MOUNT_REMOVALS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
static NEXT_NFS_TEARDOWN_RECEIPT: AtomicU64 = AtomicU64::new(1);

fn register_nfs_mount(
    mount_id: u64,
    transport: &Arc<crate::nfs_transport::NfsSocketTransport>,
    options: &NfsMountOptions,
    client: axfs_ng_vfs::FsNameBuf,
) -> AxResult<()> {
    let mut mounts = NFS_MOUNT_TRANSPORTS.lock();
    mounts.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    mounts.push((mount_id, transport.clone(), options.clone(), client));
    Ok(())
}
fn nfs_mount_transport(
    mount_id: u64,
) -> Option<(
    Arc<crate::nfs_transport::NfsSocketTransport>,
    NfsMountOptions,
)> {
    let mounts = NFS_MOUNT_TRANSPORTS.lock();
    mounts
        .iter()
        .find(|entry| entry.0 == mount_id)
        .map(|entry| (entry.1.clone(), entry.2.clone()))
}
pub(crate) fn clone_nfs_mount_registration(source_id: u64, clone_id: u64) -> AxResult<()> {
    let (transport, options) = nfs_mount_transport(source_id).ok_or(LinuxError::ENODEV)?;
    let client = crate::pseudofs::rpc_pipefs::register_nfs_client()?;
    if let Err(error) = register_nfs_mount(clone_id, &transport, &options, client.clone()) {
        crate::pseudofs::rpc_pipefs::unregister_nfs_client(client.as_name());
        return Err(error);
    }
    Ok(())
}
pub(crate) fn unregister_nfs_mount(mount_id: u64) {
    let mut mounts = NFS_MOUNT_TRANSPORTS.lock();
    // A propagation receipt owns this exact mount registration until its VFS
    // and namespace commits agree.  Do not let an unrelated rollback consume
    // the client in the middle of that receipt.
    if NFS_PENDING_MOUNT_REMOVALS
        .lock()
        .iter()
        .any(|(_, pending_id)| *pending_id == mount_id)
    {
        return;
    }
    if let Some(index) = mounts.iter().position(|entry| entry.0 == mount_id) {
        let (_, transport, _, client) = mounts.remove(index);
        crate::pseudofs::rpc_pipefs::unregister_nfs_client(client.as_name());
        if !mounts.iter().any(|entry| Arc::ptr_eq(&entry.1, &transport)) {
            transport.shutdown();
        }
    }
}

/// A prepared NFS registration retirement.  It performs every allocation and
/// duplicate-removal check before VFS detach; commit is only registry erasure
/// and rpc_pipefs client release.
pub(crate) struct PreparedNfsMountTeardown {
    receipt_id: u64,
    mount_ids: Vec<u64>,
    clients: Vec<axfs_ng_vfs::FsNameBuf>,
    // A registration is not a provider lifetime token.  Keep the transport
    // alive across VFS detach and rpc_pipefs retirement.
    _transports: Vec<Arc<crate::nfs_transport::NfsSocketTransport>>,
    active: bool,
}

impl PreparedNfsMountTeardown {
    pub(crate) fn commit(mut self) {
        let mut mounts = NFS_MOUNT_TRANSPORTS.lock();
        let mut retired = Vec::new();
        let mut index = 0;
        while index < mounts.len() {
            if self.mount_ids.contains(&mounts[index].0) {
                retired.push(mounts.remove(index).1);
            } else {
                index += 1;
            }
        }
        let mut pending = NFS_PENDING_MOUNT_REMOVALS.lock();
        pending.retain(|(receipt, _)| *receipt != self.receipt_id);
        drop(pending);
        for client in &self.clients {
            crate::pseudofs::rpc_pipefs::unregister_nfs_client(client.as_name());
        }
        for transport in retired {
            if !mounts.iter().any(|entry| Arc::ptr_eq(&entry.1, &transport)) {
                transport.shutdown();
            }
        }
        drop(mounts);
        self.active = false;
    }
}

impl Drop for PreparedNfsMountTeardown {
    fn drop(&mut self) {
        if self.active {
            NFS_PENDING_MOUNT_REMOVALS
                .lock()
                .retain(|(receipt, _)| *receipt != self.receipt_id);
        }
    }
}

pub(crate) fn prepare_nfs_mount_teardown(
    mount_ids: Vec<u64>,
) -> AxResult<PreparedNfsMountTeardown> {
    let receipt_id = NEXT_NFS_TEARDOWN_RECEIPT
        .fetch_add(1, Ordering::Relaxed)
        .max(1);
    let mounts = NFS_MOUNT_TRANSPORTS.lock();
    let mut pending = NFS_PENDING_MOUNT_REMOVALS.lock();
    pending
        .try_reserve(mount_ids.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut clients = Vec::new();
    clients
        .try_reserve(mount_ids.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut transports = Vec::new();
    transports
        .try_reserve(mount_ids.len())
        .map_err(|_| AxError::NoMemory)?;
    for mount_id in &mount_ids {
        if pending.iter().any(|(_, pending_id)| pending_id == mount_id) {
            return Err(AxError::ResourceBusy);
        }
        let entry = mounts
            .iter()
            .find(|entry| entry.0 == *mount_id)
            .ok_or(AxError::NotFound)?;
        clients.push(entry.3.clone());
        transports.push(entry.1.clone());
    }
    pending.extend(
        mount_ids
            .iter()
            .copied()
            .map(|mount_id| (receipt_id, mount_id)),
    );
    Ok(PreparedNfsMountTeardown {
        receipt_id,
        mount_ids,
        clients,
        _transports: transports,
        active: true,
    })
}

pub(crate) struct FsOpenFd(Mutex<FsOpenState>);

impl FileLike for FsOpenFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[fsopen]",
        )))
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // fsconfig operations are synchronous; FileDescription stores the
        // status bit for generic fcntl semantics.
        Ok(())
    }
}

impl Pollable for FsOpenFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        _context: &mut core::task::Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

/// A detached tree has no namespace `MountTopology` until `move_mount(2)`
/// publishes it.  Retain its complete mount graph separately instead of
/// rediscovering children through the caller's current namespace.
#[derive(Clone)]
struct DetachedTreeMount {
    mountpoint: Arc<Mountpoint>,
    parent: Option<u64>,
    idmap: Option<Arc<mounts::MountIdmap>>,
    peer_group: Option<mounts::PeerGroup>,
    unbindable: bool,
}

struct DetachedTreeLedger {
    mounts: Vec<DetachedTreeMount>,
}

impl DetachedTreeLedger {
    fn singleton(
        mountpoint: Arc<Mountpoint>,
        idmap: Option<Arc<mounts::MountIdmap>>,
        peer_group: Option<mounts::PeerGroup>,
        unbindable: bool,
    ) -> AxResult<Self> {
        let mut mounts = Vec::new();
        mounts.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        mounts.push(DetachedTreeMount {
            mountpoint,
            parent: None,
            idmap,
            peer_group,
            unbindable,
        });
        Ok(Self { mounts })
    }

    fn try_clone(&self) -> AxResult<Self> {
        let mut mounts = Vec::new();
        mounts
            .try_reserve_exact(self.mounts.len())
            .map_err(|_| AxError::NoMemory)?;
        mounts.extend(self.mounts.iter().cloned());
        Ok(Self { mounts })
    }

    fn mount(&self, id: u64) -> AxResult<&DetachedTreeMount> {
        self.mounts
            .iter()
            .find(|mount| mount.mountpoint.mount_id() == id)
            .ok_or(AxError::InvalidInput)
    }

    fn propagation(&self) -> AxResult<Vec<mounts::DetachedMountPropagation>> {
        let mut propagation = Vec::new();
        propagation
            .try_reserve_exact(self.mounts.len())
            .map_err(|_| AxError::NoMemory)?;
        for mount in &self.mounts {
            propagation.push(mounts::DetachedMountPropagation {
                mount_id: mount.mountpoint.mount_id(),
                peer_group: mount.peer_group,
                unbindable: mount.unbindable,
            });
        }
        Ok(propagation)
    }
}

fn detached_ledger_from_topology(
    topology: &Arc<mounts::MountTopology>,
) -> AxResult<DetachedTreeLedger> {
    let snapshot = topology.try_snapshot()?;
    let mut mounts = Vec::new();
    mounts
        .try_reserve_exact(snapshot.mounts.len())
        .map_err(|_| AxError::NoMemory)?;
    for mount in snapshot.mounts {
        mounts.push(DetachedTreeMount {
            mountpoint: mount.mountpoint()?,
            parent: mount.parent,
            idmap: mount.idmap,
            peer_group: mount.peer_group,
            unbindable: mount.unbindable,
        });
    }
    Ok(DetachedTreeLedger { mounts })
}

struct RetainedBindSubmount {
    source: Location,
    relative_path: FsPathBuf,
    source_id: u64,
    metadata: mounts::MountMetadata,
    flags: u32,
    idmap: Option<Arc<mounts::MountIdmap>>,
    peer_group: Option<mounts::PeerGroup>,
    unbindable: bool,
}

/// The retained variant of `recursive_bind_submounts`.  The global helper is
/// deliberately current-namespace scoped, which is wrong after setns and has
/// no records at all for an unmounted FsMountFd tree.
fn retained_recursive_submounts(
    ledger: &DetachedTreeLedger,
    source: &Location,
) -> AxResult<Vec<RetainedBindSubmount>> {
    let source_id = source.mountpoint().mount_id();
    let source_path = source.absolute_path().map_err(|_| AxError::Io)?;
    ledger.mount(source_id)?;

    let mut admitted = HashMap::new();
    admitted
        .try_reserve(ledger.mounts.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut visited = HashSet::new();
    visited
        .try_reserve(ledger.mounts.len())
        .map_err(|_| AxError::NoMemory)?;
    let mut selected = Vec::new();
    selected
        .try_reserve(ledger.mounts.len())
        .map_err(|_| AxError::NoMemory)?;
    admitted.insert(source_id, 0usize);

    loop {
        let visited_before = visited.len();
        for mount in &ledger.mounts {
            let mount_id = mount.mountpoint.mount_id();
            if mount_id == source_id || visited.contains(&mount_id) {
                continue;
            }
            let Some(parent_depth) = mount
                .parent
                .and_then(|parent| admitted.get(&parent).copied())
            else {
                continue;
            };
            let attachment = mount.mountpoint.location().ok_or(AxError::Io)?;
            if mount.parent != Some(attachment.mountpoint().mount_id()) {
                return Err(AxError::Io);
            }
            if mount.parent == Some(source_id)
                && !source.entry().is_ancestor_of(attachment.entry())?
            {
                visited.insert(mount_id);
                continue;
            }
            visited.insert(mount_id);
            if mount.unbindable {
                continue;
            }
            let depth = parent_depth.checked_add(1).ok_or(AxError::Io)?;
            admitted.insert(mount_id, depth);
            let child_root = mount.mountpoint.root_location();
            let absolute = child_root.absolute_path().map_err(|_| AxError::Io)?;
            let relative_path = (source_path.as_bytes() == b"/")
                .then_some(absolute.as_ref())
                .or_else(|| {
                    absolute
                        .as_bytes()
                        .strip_prefix(source_path.as_bytes())
                        .filter(|suffix| suffix.starts_with(b"/"))
                        .map(FsPath::new)
                })
                .ok_or(AxError::Io)
                .and_then(|path| {
                    let mut bytes = Vec::new();
                    bytes
                        .try_reserve_exact(path.as_bytes().len())
                        .map_err(|_| AxError::NoMemory)?;
                    bytes.extend_from_slice(path.as_bytes());
                    Ok(FsPathBuf::from_vec(bytes))
                })?;
            selected.push((
                depth,
                RetainedBindSubmount {
                    metadata: mounts::clone_metadata_for_bind(&child_root)?,
                    flags: mounts::flags_for_location(&child_root)?,
                    source: child_root,
                    relative_path,
                    source_id: mount_id,
                    idmap: mount.idmap.clone(),
                    peer_group: mount.peer_group,
                    unbindable: mount.unbindable,
                },
            ));
        }
        if visited.len() == visited_before {
            break;
        }
    }
    selected.sort_by_key(|(depth, _)| *depth);
    let mut mounts = Vec::new();
    mounts
        .try_reserve_exact(selected.len())
        .map_err(|_| AxError::NoMemory)?;
    for (_, mount) in selected {
        mounts.push(mount);
    }
    Ok(mounts)
}

/// One detached tree may be retained by many `FsMountFd`s produced without
/// OPEN_TREE_CLONE.  This state is shared by those descriptors: a detached
/// mount is consumed exactly once by move_mount, and its rollback runs only
/// when the final retained descriptor disappears.
struct FsMountTreeState {
    /// Serializes detached-tree mutation, attach consumption, and retained
    /// clone snapshots before either idmap or ledger state is observed.
    operation: Mutex<()>,
    /// Per-mount idmaps retained while this tree is detached.  Recursive
    /// open_tree clones may carry distinct mappings on nested mountpoints;
    /// move_mount publishes this whole map in the structural transaction.
    idmaps: Mutex<HashMap<u64, Arc<mounts::MountIdmap>>>,
    /// Namespace ledger which owns this tree after attachment.  It lets a
    /// mount FD used after setns resolve idmaps for nested mounts added after
    /// the FD itself was created.
    topology: Mutex<Option<Arc<mounts::MountTopology>>>,
    /// Full topology retained before first attachment.  A detached tree's
    /// VFS mountpoints alone are insufficient for recursive `open_tree`: its
    /// per-mount idmaps and propagation graph are namespace authority too.
    detached_ledger: Mutex<Option<DetachedTreeLedger>>,
    rollback_fuse_mount_ids: Mutex<Vec<u64>>,
    rollback_nfs_mount_ids: Mutex<Vec<u64>>,
}

impl FsMountTreeState {
    fn try_new(
        idmaps: HashMap<u64, Arc<mounts::MountIdmap>>,
        topology: Option<Arc<mounts::MountTopology>>,
        detached_ledger: Option<DetachedTreeLedger>,
        rollback_fuse_mount_ids: Vec<u64>,
        rollback_nfs_mount_ids: Vec<u64>,
    ) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            operation: Mutex::new(()),
            idmaps: Mutex::new(idmaps),
            topology: Mutex::new(topology),
            detached_ledger: Mutex::new(detached_ledger),
            rollback_fuse_mount_ids: Mutex::new(rollback_fuse_mount_ids),
            rollback_nfs_mount_ids: Mutex::new(rollback_nfs_mount_ids),
        })
        .map_err(|_| AxError::NoMemory)
    }
}

/// Owns all effects performed while a detached tree is still unpublished.
/// It covers the allocation gap before `FsMountTreeState` becomes the shared
/// owner, including provider registrations that otherwise outlive ENOMEM.
struct DetachedTreeBuildGuard {
    root: Arc<Mountpoint>,
    fuse: Vec<u64>,
    nfs: Vec<u64>,
    active: bool,
}

impl DetachedTreeBuildGuard {
    fn new(root: Arc<Mountpoint>) -> Self {
        Self {
            root,
            fuse: Vec::new(),
            nfs: Vec::new(),
            active: true,
        }
    }

    fn into_registrations(mut self) -> (Vec<u64>, Vec<u64>) {
        self.active = false;
        (
            core::mem::take(&mut self.fuse),
            core::mem::take(&mut self.nfs),
        )
    }
}

impl Drop for DetachedTreeBuildGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        for mount_id in self.fuse.drain(..) {
            crate::pseudofs::dev::fuse::unregister_mount_connection(mount_id);
        }
        for mount_id in self.nfs.drain(..) {
            unregister_nfs_mount(mount_id);
        }
        let _ = self.root.root_location().lazy_unmount();
    }
}

pub(crate) struct FsMountFd {
    root: axfs_ng_vfs::Location,
    tree: Arc<FsMountTreeState>,
}

impl Drop for FsMountFd {
    fn drop(&mut self) {
        if Arc::strong_count(&self.tree) != 1 || self.root.mountpoint().is_attached() {
            return;
        }
        for mount_id in self.tree.rollback_fuse_mount_ids.lock().drain(..) {
            crate::pseudofs::dev::fuse::unregister_mount_connection(mount_id);
        }
        for mount_id in self.tree.rollback_nfs_mount_ids.lock().drain(..) {
            unregister_nfs_mount(mount_id);
        }
        let _ = self.root.lazy_unmount();
    }
}

struct BindFilesystem {
    root: DirEntry,
    name: String,
    source: Filesystem,
}

impl BindFilesystem {
    fn try_new(root: DirEntry, name: String, source: &Filesystem) -> AxResult<Filesystem> {
        let ops = Arc::try_new(Self {
            root,
            name,
            source: source.clone(),
        })
        .map_err(|_| AxError::NoMemory)?;
        Filesystem::try_new_view(ops, source)
    }
}

impl FilesystemOps for BindFilesystem {
    fn name(&self) -> &str {
        &self.name
    }

    fn root_dir(&self) -> DirEntry {
        self.root.clone()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        self.root.filesystem().stat()
    }

    fn flush(&self) -> VfsResult<()> {
        self.root.filesystem().flush()
    }

    // A bind mount is a distinct mount view, not a new superblock.  Preserve
    // the source export operations and their opaque handle/error semantics.
    fn encode_export_handle(
        &self,
        entry: &DirEntry,
        mode: ExportHandleMode,
    ) -> VfsResult<ExportHandle> {
        self.source.encode_export_handle(entry, mode)
    }

    fn decode_export_handle(&self, handle_type: i32, bytes: &[u8]) -> VfsResult<DirEntry> {
        self.source.decode_export_handle(handle_type, bytes)
    }

    fn decode_export_handle_with_mode(
        &self,
        handle_type: i32,
        bytes: &[u8],
        mode: ExportHandleDecodeMode,
    ) -> VfsResult<DirEntry> {
        self.source
            .decode_export_handle_with_mode(handle_type, bytes, mode)
    }

    fn export_handle_is_descendant(
        &self,
        ancestor: &DirEntry,
        descendant: &DirEntry,
    ) -> VfsResult<bool> {
        self.source
            .export_handle_is_descendant(ancestor, descendant)
    }
}

fn map_mount_uapi(error: UapiError) -> AxError {
    match error {
        UapiError::Invalid => AxError::InvalidInput,
        UapiError::Unsupported => AxError::OperationNotSupported,
        UapiError::TooBig => LinuxError::E2BIG.into(),
        UapiError::NotFound => AxError::NotFound,
    }
}

fn block_device_name_for_rdev(rdev: DeviceId) -> AxResult<Option<String>> {
    if rdev == mounts::ROOT_BLOCK_DEVICE_ID {
        return Ok(Some(try_string(axfs::ROOT_BLOCK_DEVICE_NAME)?));
    }

    Ok(block_device_names()
        .into_iter()
        .enumerate()
        .find_map(|(index, name)| {
            (mounts::extra_block_device_id(index) == Some(rdev)).then_some(name)
        }))
}

/// Btrfs has one ordered member set rather than XFS's named device roles.
/// Keep this option grammar shared by legacy mount data and fsconfig so the
/// mount metadata retains the exact resolved-at-publication device ledger.
#[derive(Default)]
struct BtrfsMountOptions {
    devices: Vec<FsPathBuf>,
}

struct BtrfsBlockMember {
    name: String,
    rdev: DeviceId,
}

fn parse_btrfs_mount_options(data: &str) -> AxResult<BtrfsMountOptions> {
    let mut options = BtrfsMountOptions::default();
    if data.is_empty() {
        return Ok(options);
    }
    for option in data.split(',') {
        let (key, value) = option.split_once('=').ok_or(AxError::InvalidInput)?;
        if key != "device" || value.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let path = FsPathBuf::from_vec(value.as_bytes().to_vec());
        validate_pathname(path.as_ref())?;
        options
            .devices
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        options.devices.push(path);
    }
    Ok(options)
}

fn append_btrfs_mount_option(data: &mut String, key: &str, value: &FsPath) -> AxResult<()> {
    if key != "device" {
        return Err(AxError::InvalidInput);
    }
    // Validate through the same grammar before mutating the fsopen ledger.
    let value = core::str::from_utf8(value.as_bytes()).map_err(|_| AxError::IllegalBytes)?;
    let candidate = if data.is_empty() {
        format!("device={value}")
    } else {
        format!("{data},device={value}")
    };
    let _ = parse_btrfs_mount_options(&candidate)?;
    *data = candidate;
    Ok(())
}

fn resolve_btrfs_block_member(
    source: &FsPath,
    security: &VfsSecurityContext,
) -> AxResult<BtrfsBlockMember> {
    let location = current_fs_context()
        .lock()
        .resolve_security(source, security)?;
    let metadata = location.metadata()?;
    if metadata.node_type != NodeType::BlockDevice {
        return Err(LinuxError::ENOTBLK.into());
    }
    if mounts::is_nodev(&location)? {
        return Err(AxError::PermissionDenied);
    }
    let name = block_device_name_for_rdev(metadata.rdev)?.ok_or(AxError::NoSuchDevice)?;
    Ok(BtrfsBlockMember {
        name,
        rdev: metadata.rdev,
    })
}

fn claim_btrfs_block_member(member: &BtrfsBlockMember) -> AxResult<axfs::MountedBlockDevice> {
    open_block_device(&member.name).map_err(|error| match error {
        OpenBlockDeviceError::NotFound => AxError::NoSuchDevice,
        OpenBlockDeviceError::Busy => AxError::ResourceBusy,
    })
}

fn new_btrfs_filesystem(
    source: &FsPath,
    data: &str,
    security: &VfsSecurityContext,
    read_only_mount: bool,
) -> AxResult<(Filesystem, DeviceId, Vec<DeviceId>)> {
    let options = parse_btrfs_mount_options(data)?;
    let member_count = options
        .devices
        .len()
        .checked_add(1)
        .ok_or(AxError::NoMemory)?;
    let mut members = Vec::new();
    members
        .try_reserve_exact(member_count)
        .map_err(|_| AxError::NoMemory)?;
    members.push(resolve_btrfs_block_member(source, security)?);
    for path in &options.devices {
        let member = resolve_btrfs_block_member(path.as_ref(), security)?;
        if members.iter().any(|existing| existing.rdev == member.rdev) {
            return Err(AxError::InvalidInput);
        }
        members.push(member);
    }

    // Resolve and validate every member before obtaining any claim.  A
    // claimed vector then gives failures during later claims/factory setup a
    // typed RAII rollback path without exposing a partial volume.
    for member in &members {
        if block_device_is_read_only(&member.name).ok_or(AxError::NoSuchDevice)? && !read_only_mount
        {
            return Err(AxError::PermissionDenied);
        }
    }
    let source_rdev = members[0].rdev;
    let mut member_rdevs = Vec::new();
    member_rdevs
        .try_reserve_exact(members.len())
        .map_err(|_| AxError::NoMemory)?;
    for member in &members {
        member_rdevs.push(member.rdev);
    }
    let mut claims = Vec::new();
    claims
        .try_reserve_exact(members.len())
        .map_err(|_| AxError::NoMemory)?;
    for member in &members {
        claims.push(claim_btrfs_block_member(member)?);
    }
    let filesystem = new_btrfs_filesystem_with_members(claims)?;
    Ok((filesystem, source_rdev, member_rdevs))
}

/// XFS has three independently claimed device roles.  Keep the textual
/// grammar shared by legacy mount data and fsconfig SET_STRING, then resolve
/// every role before claiming any one of them.  That makes duplicate roles
/// and non-block paths fail without a transient mount claim escaping the
/// failed transaction.  Physical read-only capability remains attached to
/// each claim for the XFS factory to decide whether recovery is safe.
#[derive(Default)]
struct XfsMountOptions {
    logdev: Option<FsPathBuf>,
    rtdev: Option<FsPathBuf>,
    norecovery: bool,
}

struct XfsBlockMember {
    name: String,
    rdev: DeviceId,
}

fn parse_xfs_mount_options(data: &str) -> AxResult<XfsMountOptions> {
    let mut options = XfsMountOptions::default();
    if data.is_empty() {
        return Ok(options);
    }
    for option in data.split(',') {
        if option == "norecovery" && !options.norecovery {
            options.norecovery = true;
            continue;
        }
        let (key, value) = option.split_once('=').ok_or(AxError::InvalidInput)?;
        if value.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let path = FsPathBuf::from_vec(value.as_bytes().to_vec());
        validate_pathname(path.as_ref())?;
        match key {
            "logdev" if options.logdev.is_none() => options.logdev = Some(path),
            "rtdev" if options.rtdev.is_none() => options.rtdev = Some(path),
            "logdev" | "rtdev" | "norecovery" => return Err(AxError::InvalidInput),
            _ => return Err(AxError::InvalidInput),
        }
    }
    Ok(options)
}

fn same_xfs_mount_options(left: &XfsMountOptions, right: &XfsMountOptions) -> bool {
    left.norecovery == right.norecovery
        && left.logdev.as_deref().map(|path| path.as_bytes())
            == right.logdev.as_deref().map(|path| path.as_bytes())
        && left.rtdev.as_deref().map(|path| path.as_bytes())
            == right.rtdev.as_deref().map(|path| path.as_bytes())
}

fn append_xfs_mount_option(data: &mut String, key: &str, value: &FsPath) -> AxResult<()> {
    // Validate through the same grammar before mutating the fsopen ledger.
    let candidate = if data.is_empty() {
        format!(
            "{key}={}",
            core::str::from_utf8(value.as_bytes()).map_err(|_| AxError::IllegalBytes)?
        )
    } else {
        format!(
            "{data},{key}={}",
            core::str::from_utf8(value.as_bytes()).map_err(|_| AxError::IllegalBytes)?
        )
    };
    let _ = parse_xfs_mount_options(&candidate)?;
    *data = candidate;
    Ok(())
}

fn resolve_xfs_block_member(
    source: &FsPath,
    security: &VfsSecurityContext,
) -> AxResult<XfsBlockMember> {
    let location = current_fs_context()
        .lock()
        .resolve_security(source, security)?;
    let metadata = location.metadata()?;
    if metadata.node_type != NodeType::BlockDevice {
        return Err(LinuxError::ENOTBLK.into());
    }
    if mounts::is_nodev(&location)? {
        return Err(AxError::PermissionDenied);
    }
    let name = block_device_name_for_rdev(metadata.rdev)?.ok_or(AxError::NoSuchDevice)?;
    Ok(XfsBlockMember {
        name,
        rdev: metadata.rdev,
    })
}

fn claim_xfs_block_member(member: &XfsBlockMember) -> AxResult<axfs::MountedBlockDevice> {
    open_block_device(&member.name).map_err(|error| match error {
        OpenBlockDeviceError::NotFound => AxError::NoSuchDevice,
        OpenBlockDeviceError::Busy => AxError::ResourceBusy,
    })
}

fn new_xfs_filesystem(
    source: &FsPath,
    data: &str,
    security: &VfsSecurityContext,
    read_only_mount: bool,
) -> AxResult<(Filesystem, DeviceId)> {
    let options = parse_xfs_mount_options(data)?;
    if options.norecovery && !read_only_mount {
        return Err(AxError::InvalidInput);
    }
    let data_member = resolve_xfs_block_member(source, security)?;
    let log_member = options
        .logdev
        .as_deref()
        .map(|path| resolve_xfs_block_member(path, security))
        .transpose()?;
    let rt_member = options
        .rtdev
        .as_deref()
        .map(|path| resolve_xfs_block_member(path, security))
        .transpose()?;

    if log_member
        .as_ref()
        .is_some_and(|member| member.rdev == data_member.rdev)
        || rt_member
            .as_ref()
            .is_some_and(|member| member.rdev == data_member.rdev)
        || matches!((&log_member, &rt_member), (Some(log), Some(rt)) if log.rdev == rt.rdev)
    {
        return Err(AxError::InvalidInput);
    }

    // Claim every resolved role before provider construction.  The claim
    // records the physical RO snapshot and releases all earlier claims by
    // RAII if a later role/factory operation fails.
    let data_claim = claim_xfs_block_member(&data_member)?;
    let log_claim = log_member
        .as_ref()
        .map(claim_xfs_block_member)
        .transpose()?;
    let rt_claim = rt_member.as_ref().map(claim_xfs_block_member).transpose()?;
    let members = XfsMountMembers::with_mount_options(
        data_claim,
        log_claim,
        rt_claim,
        read_only_mount,
        options.norecovery,
    )?;
    let filesystem = XfsFilesystem::new_with_members(members)?;
    Ok((filesystem, data_member.rdev))
}

fn parse_tmpfs_size_component(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || value.ends_with('%') {
        return None;
    }

    let (digits, scale) = match value.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1024),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        Some(b'b' | b'B') => (&value[..value.len() - 1], 1),
        Some(_) => (value, 1),
        None => return None,
    };

    digits.trim().parse::<u64>().ok()?.checked_mul(scale)
}

fn parse_tmpfs_size(data: &str) -> Option<u64> {
    data.split(',').find_map(|option| {
        option
            .trim()
            .strip_prefix("size=")
            .and_then(parse_tmpfs_size_component)
    })
}

fn parse_fat_mask(value: &str) -> Option<u16> {
    let value = value.strip_prefix("0o").unwrap_or(value);
    u16::from_str_radix(value, 8)
        .ok()
        .filter(|mask| *mask & !0o777 == 0)
}

fn parse_fat_mount_options(
    data: &str,
    credentials: &DacCredentialView,
    umask: u16,
) -> AxResult<FatMountOptions> {
    parse_fat_mount_options_with_defaults(
        data,
        credentials.uid().into_raw(),
        credentials.gid().into_raw(),
        umask,
    )
}

fn parse_fat_mount_options_with_defaults(
    data: &str,
    mut uid: u32,
    mut gid: u32,
    umask: u16,
) -> AxResult<FatMountOptions> {
    let mut file_mask = umask & 0o777;
    let mut dir_mask = file_mask;

    for raw in data.split(',') {
        let option = raw.trim();
        if option.is_empty()
            || matches!(
                option,
                "rw" | "ro"
                    | "suid"
                    | "nosuid"
                    | "dev"
                    | "nodev"
                    | "exec"
                    | "noexec"
                    | "atime"
                    | "noatime"
                    | "diratime"
                    | "nodiratime"
                    | "relatime"
                    | "strictatime"
            )
        {
            continue;
        }

        let Some((key, value)) = option.split_once('=') else {
            return Err(AxError::OperationNotSupported);
        };
        match key {
            "uid" => uid = value.parse().map_err(|_| AxError::InvalidInput)?,
            "gid" => gid = value.parse().map_err(|_| AxError::InvalidInput)?,
            "umask" => {
                let mask = parse_fat_mask(value).ok_or(AxError::InvalidInput)?;
                file_mask = mask;
                dir_mask = mask;
            }
            "fmask" => file_mask = parse_fat_mask(value).ok_or(AxError::InvalidInput)?,
            "dmask" => dir_mask = parse_fat_mask(value).ok_or(AxError::InvalidInput)?,
            _ => return Err(AxError::OperationNotSupported),
        }
    }

    Ok(FatMountOptions {
        uid,
        gid,
        file_mode: NodePermission::from_bits_truncate(0o777 & !file_mask),
        dir_mode: NodePermission::from_bits_truncate(0o777 & !dir_mask),
    })
}

fn tmpfs_for_mount(data: &str) -> AxResult<Filesystem> {
    MemoryFs::new_with_capacity(parse_tmpfs_size(data))
}

fn bind_mount_flags(
    source: &axfs_ng_vfs::Location,
    target: &axfs_ng_vfs::Location,
    requested: u32,
) -> AxResult<u32> {
    let source_flags = mounts::flags_for_location(source)?;
    let target_flags = mounts::flags_for_location(target)?;
    if (requested | source_flags | target_flags) & MS_PROPAGATION_FLAGS != 0 {
        return Err(AxError::OperationNotSupported);
    }
    Ok(source_flags & MS_INHERITED_BIND_FLAGS)
}

fn bind_filesystem_for(loc: &axfs_ng_vfs::Location, fs_type: &str) -> AxResult<Filesystem> {
    let source = loc.mountpoint().filesystem_handle();
    BindFilesystem::try_new(loc.entry().clone(), try_string(fs_type)?, &source)
}

fn do_bind_mount(
    source: &FsPath,
    target: &axfs_ng_vfs::Location,
    flags: u32,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    if source.as_bytes().is_empty() {
        return Err(AxError::InvalidInput);
    }

    let source_loc = current_fs_context()
        .lock()
        .resolve_security(source, security)?;
    if source_loc.is_dir() != target.is_dir() {
        return Err(AxError::InvalidInput);
    }
    source_loc.check_is_dir()?;
    target.check_is_dir()?;

    let metadata = mounts::clone_metadata_for_bind(&source_loc)?;
    let fs = bind_filesystem_for(&source_loc, &metadata.fs_type)?;
    let mount_flags = bind_mount_flags(&source_loc, target, flags)?;
    let mountpoint = mounts::new_detached_with_flags(&fs, mount_flags, metadata)?;

    if flags & MS_REC != 0 {
        let detached_context = FsContext::new(mountpoint.root_location());
        let children = mounts::recursive_bind_submounts(&source_loc)?;
        for child in children {
            let child_target = detached_context
                .resolve(&child.relative_path)
                .map_err(|_| AxError::Io)?;
            let child_source = child.source;
            if child_source.is_dir() != child_target.is_dir() || !child_source.is_dir() {
                return Err(AxError::Io);
            }
            let child_fs = bind_filesystem_for(&child_source, &child.metadata.fs_type)?;
            let child_flags =
                bind_mount_flags(&child_source, &child_target, child.flags | MS_BIND | MS_REC)?;
            mounts::mount_with_flags(&child_target, &child_fs, child_flags, child.metadata)?;
        }
    }

    mounts::bind_tree_and_record_from(&mountpoint, target, source_loc.mountpoint().mount_id())?;
    Ok(())
}

fn do_move_mount_old(
    source: &FsPath,
    target: &axfs_ng_vfs::Location,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    if source.as_bytes().is_empty() {
        return Err(AxError::InvalidInput);
    }

    let old = current_fs_context()
        .lock()
        .resolve_security(source, security)?;
    if !old.is_root_of_mount() || old.is_root() || old.is_dir() != target.is_dir() {
        return Err(AxError::InvalidInput);
    }
    // sys_mount holds namespace_operation() before reaching MS_MOVE, so this
    // VFS state and the following topology transaction are one placement
    // decision.  Do not let legacy mount(2) bypass MNT_LOCKED.
    if old.mountpoint().is_placement_locked() {
        return Err(AxError::InvalidInput);
    }
    mounts::move_tree_and_records(&old, target)?;
    Ok(())
}

fn pseudo_fs_for_mount(source: &str, fs_type: &str, data: &str) -> AxResult<Option<Filesystem>> {
    Ok(match fs_type {
        // bpffs has the normal named-dentry mechanics of an in-memory
        // filesystem; the mount metadata is its type authority and the BPF
        // object layer supplies the non-file payload/lifetime.
        "tmpfs" | "bpf" => Some(tmpfs_for_mount(data)?),
        "hugetlbfs" => Some(crate::pseudofs::hugetlb::new_hugetlbfs(data)?),
        // A cgroup superblock is global hierarchy state, while the root
        // exposed by this mount belongs to the caller's cgroup namespace.
        // Capture that immutable root before the mount is published; later
        // setns/unshare must not retarget an already mounted view.
        "cgroup" => {
            let controllers = cgroup::parse_v1_controllers(source, data)?;
            let roots = current().as_thread().cgroup_ns().roots().clone();
            Some(cgroup::new_cgroup_v1_for_namespace(controllers, &roots)?)
        }
        "cgroup2" => {
            let roots = current().as_thread().cgroup_ns().roots().clone();
            Some(cgroup::new_cgroup_v2_for_namespace(&roots)?)
        }
        "tracefs" | "debugfs" => Some(trace::new_tracefs()),
        "proc" => Some(crate::pseudofs::proc::new_procfs()),
        "sysfs" => Some(crate::pseudofs::sys::new_sysfs()),
        "mqueue" => Some(crate::pseudofs::mqueue::new_mqueuefs(
            current().as_thread().ipc_ns(),
        )),
        "rpc_pipefs" => Some(crate::pseudofs::rpc_pipefs::new_rpc_pipefs()?),
        _ => None,
    })
}

impl FileLike for FsMountFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[fsmount]",
        )))
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // This detached mount handle has no blocking data operation.
        Ok(())
    }
}

impl Pollable for FsMountFd {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        _context: &mut core::task::Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

pub fn sys_fsopen<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fs_name: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let fs_name = load_user_string(memory, fs_name)?;
    debug!("sys_fsopen <= fs_name: {fs_name:?}, flags: {flags:#x}");

    let cloexec = validate_fsopen_flags(flags).map_err(map_mount_uapi)?;
    if !current_may_mount() {
        return Err(LinuxError::EPERM.into());
    }
    if filesystem_type(&fs_name).is_none() {
        return Err(AxError::NoSuchDevice);
    }
    let overlay = fs_name == "overlay";

    FsOpenFd(Mutex::new(FsOpenState {
        fs_type: fs_name,
        source: None,
        data: String::new(),
        config_len: 0,
        created: false,
        fuse_connection: None,
        nfs_transport: None,
        nfs_options: {
            // AUTH_SYS credentials belong to the superblock, not whichever
            // thread later happens to issue an RPC through this mount.
            let cred = current().as_thread().current_cred();
            let ids = cred.ids();
            let mut groups = Vec::new();
            groups
                .try_reserve_exact(cred.groups().as_slice().len())
                .map_err(|_| AxError::NoMemory)?;
            for group in cred.groups().as_slice() {
                groups.push(group.into_raw());
            }
            NfsMountOptions {
                auth_sys: RpcSysAuth::new(
                    b"thekernel".to_vec(),
                    ids.fsuid.into_raw(),
                    ids.fsgid.into_raw(),
                    groups,
                )
                .map_err(|_| AxError::InvalidInput)?,
                ..NfsMountOptions::default()
            }
        },
        overlay: if overlay {
            Some(OverlayMountOptions::empty())
        } else {
            None
        },
        binary: Vec::new(),
        paths: Vec::new(),
        reconfigure_mount: None,
    }))
    .add_to_fd_table(cloexec)
    .map(|fd| fd as isize)
}

pub fn sys_fsconfig<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    cmd: u32,
    key: *const c_char,
    value: *const c_void,
    aux: i32,
) -> AxResult<isize> {
    if fd < 0 {
        return Err(AxError::InvalidInput);
    }

    validate_fsconfig_shape(
        cmd,
        !key.is_null(),
        !value.is_null(),
        aux,
        linux_raw_sys::general::AT_FDCWD,
    )
    .map_err(map_mount_uapi)?;

    let file = get_file_like(fd)?;
    let fsopen = file
        .downcast_ref::<FsOpenFd>()
        .ok_or(AxError::InvalidInput)?;
    let mut state = fsopen.0.lock();

    // A context returned by fspick is already bound to a superblock but is
    // specifically allowed to accept reconfiguration parameters.  A created
    // fsopen context, in contrast, is sealed against further SET commands.
    if state.created
        && state.reconfigure_mount.is_none()
        && matches!(
            cmd,
            FSCONFIG_SET_FLAG
                | FSCONFIG_SET_STRING
                | FSCONFIG_SET_BINARY
                | FSCONFIG_SET_PATH
                | FSCONFIG_SET_PATH_EMPTY
                | FSCONFIG_SET_FD
        )
    {
        return Err(AxError::ResourceBusy);
    }

    match cmd {
        FSCONFIG_SET_FLAG => {
            let key = load_user_string(memory, key)?;
            if key.is_empty() || !value.is_null() {
                return Err(AxError::InvalidInput);
            }
            if state.fs_type == "overlay" {
                // Overlay's feature vocabulary is explicitly `key=on|off`;
                // accepting a generic flag would make the ledger diverge
                // from `OverlayMountOptions`.
                return Err(AxError::InvalidInput);
            }
            if state.fs_type == "xfs" && key != "norecovery" {
                return Err(AxError::InvalidInput);
            }
            let entry_len = key.len().checked_add(1).ok_or(AxError::NoMemory)?;
            if state.config_len.saturating_add(entry_len) > 4096 {
                return Err(AxError::InvalidInput);
            }
            if state.fs_type == "xfs" {
                let candidate = if state.data.is_empty() {
                    key.clone()
                } else {
                    format!("{},{}", state.data, key)
                };
                let _ = parse_xfs_mount_options(&candidate)?;
                state.data = candidate;
            } else {
                if !state.data.is_empty() {
                    state.data.push(',');
                }
                state.data.push_str(&key);
            }
            state.config_len += entry_len;
            Ok(0)
        }
        FSCONFIG_SET_STRING => {
            let key = load_user_string(memory, key)?;
            if state.fs_type == "overlay" {
                let value = vm_load_until_nul(memory, (value as *const c_char).cast::<u8>())
                    .map_err(map_usercopy_error)?;
                let entry_len = key
                    .len()
                    .checked_add(value.len())
                    .and_then(|len| len.checked_add(2))
                    .ok_or(AxError::NoMemory)?;
                if state.config_len.saturating_add(entry_len) > 4096 {
                    return Err(AxError::InvalidInput);
                }
                state
                    .overlay
                    .as_mut()
                    .ok_or(AxError::InvalidInput)?
                    .set_option(key.as_bytes(), &value)
                    .map_err(AxError::from)?;
                state.data = overlay_options_record_data(
                    state.overlay.as_ref().ok_or(AxError::InvalidInput)?,
                )?;
                state.config_len += entry_len;
                return Ok(0);
            }
            let value = if key == "source" {
                load_user_path(memory, value as *const c_char)?
            } else {
                FsPathBuf::from_vec(load_user_string(memory, value as *const c_char)?.into_bytes())
            };
            let entry_len = key.len() + value.as_bytes().len() + 2;
            if state.config_len.saturating_add(entry_len) > 4096 {
                return Err(AxError::InvalidInput);
            }
            match (state.fs_type.as_str(), key.as_str()) {
                (_, "source") => state.source = Some(value),
                ("nfs4", "owner") => {
                    let value = core::str::from_utf8(value.as_bytes())
                        .map_err(|_| AxError::InvalidInput)?;
                    if value.is_empty() || value.len() > 1024 {
                        return Err(AxError::InvalidInput);
                    }
                    state.nfs_options.owner = value.as_bytes().to_vec();
                }
                ("nfs4", "slots") => {
                    let value = core::str::from_utf8(value.as_bytes())
                        .map_err(|_| AxError::InvalidInput)?;
                    let slots = value.parse::<u32>().map_err(|_| AxError::InvalidInput)?;
                    if slots == 0 || slots > 1024 {
                        return Err(AxError::InvalidInput);
                    }
                    state.nfs_options.slots = slots;
                }
                ("nfs4", "sec") => {
                    state.nfs_options.security = NfsSecurityFlavor::parse(value.as_bytes())
                        .map_err(|_| AxError::InvalidInput)?;
                }
                ("xfs", "logdev" | "rtdev") => {
                    append_xfs_mount_option(&mut state.data, &key, value.as_ref())?;
                }
                ("btrfs", "device") => {
                    append_btrfs_mount_option(&mut state.data, &key, value.as_ref())?;
                }
                ("tmpfs", "size") => {
                    let value = core::str::from_utf8(value.as_bytes())
                        .map_err(|_| AxError::InvalidInput)?;
                    if parse_tmpfs_size_component(&value).is_none() {
                        return Err(AxError::InvalidInput);
                    }
                    state.data = alloc::format!("size={value}");
                }
                (
                    "hugetlbfs",
                    "size" | "pagesize" | "mode" | "uid" | "gid" | "nr_inodes" | "min_size",
                ) => {
                    let value = core::str::from_utf8(value.as_bytes())
                        .map_err(|_| AxError::InvalidInput)?;
                    let candidate = if state.data.is_empty() {
                        alloc::format!("{key}={value}")
                    } else {
                        alloc::format!("{},{}={value}", state.data, key)
                    };
                    crate::pseudofs::hugetlb::new_hugetlbfs(&candidate)?;
                    state.data = candidate;
                }
                ("overlay", _) => state
                    .overlay
                    .as_mut()
                    .ok_or(AxError::InvalidInput)?
                    .set_option(key.as_bytes(), value.as_bytes())
                    .map_err(AxError::from)?,
                _ => return Err(AxError::OperationNotSupported),
            }
            state.config_len += entry_len;
            Ok(0)
        }
        FSCONFIG_SET_FD => {
            let key = load_user_string(memory, key)?;
            if !matches!(state.fs_type.as_str(), "fuse" | "nfs4")
                || key != "fd"
                || aux < 0
                || !value.is_null()
            {
                return Err(AxError::InvalidInput);
            }
            if state.fs_type == "nfs4" {
                let socket = get_typed_file::<crate::file::Socket>(aux)?;
                // A normal TCP socket created before creator-security pinning
                // remains a valid NFS transport.  fsconfig is its one-time
                // fallback capture point; an already pinned socket keeps its
                // original creator regardless of the caller attaching it.
                let task = current();
                let thread = task.as_thread();
                socket.capture_creator_security_if_absent(
                    thread.current_cred(),
                    thread.landlock_domain(),
                );
                state.nfs_transport = Some(
                    crate::nfs_transport::NfsSocketTransport::try_new(socket)
                        .map_err(|_| AxError::InvalidInput)?,
                );
                state.config_len = state
                    .config_len
                    .checked_add(key.len() + 2)
                    .ok_or(AxError::NoMemory)?;
                return Ok(0);
            }
            let file = get_file_like(aux)?;
            let fuse = file
                .downcast_ref::<FuseDeviceFile>()
                .ok_or(AxError::InvalidInput)?;
            if fuse.connection().is_dead() {
                return Err(LinuxError::ENODEV.into());
            }
            state.fuse_connection = Some(fuse.connection());
            state.config_len = state
                .config_len
                .checked_add(key.len() + 2)
                .ok_or(AxError::NoMemory)?;
            Ok(0)
        }
        FSCONFIG_SET_PATH | FSCONFIG_SET_PATH_EMPTY if state.fs_type == "overlay" => {
            let key = load_user_string(memory, key)?;
            let path = load_user_path(memory, value as *const c_char)?;
            if cmd == FSCONFIG_SET_PATH_EMPTY && !path.as_bytes().is_empty() {
                return Err(AxError::InvalidInput);
            }
            let entry_len = key
                .len()
                .checked_add(path.as_bytes().len())
                .and_then(|len| len.checked_add(2))
                .ok_or(AxError::NoMemory)?;
            if state.config_len.saturating_add(entry_len) > 4096 {
                return Err(AxError::InvalidInput);
            }
            // fsconfig's PATH form resolves now, while the caller's dirfd and
            // namespace are authoritative.  Retain the typed result as the
            // context owns it; fsmount will not accidentally reinterpret a
            // reused fd or a path after a concurrent rename/chroot.
            let security = VfsSecurityContext::new(current().as_thread().current_cred());
            let at_flags = if cmd == FSCONFIG_SET_PATH_EMPTY {
                AT_EMPTY_PATH
            } else {
                0
            };
            let location = resolve_at_with_security(aux, Some(path.as_ref()), at_flags, &security)?
                .into_file()
                .ok_or(AxError::InvalidInput)?;
            state.paths.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            state.paths.push((key.clone(), location));
            state
                .overlay
                .as_mut()
                .ok_or(AxError::InvalidInput)?
                .set_option(key.as_bytes(), path.as_bytes())
                .map_err(AxError::from)?;
            state.data =
                overlay_options_record_data(state.overlay.as_ref().ok_or(AxError::InvalidInput)?)?;
            state.config_len += entry_len;
            Ok(0)
        }
        FSCONFIG_SET_BINARY => {
            let key = load_user_string(memory, key)?;
            let length = usize::try_from(aux).map_err(|_| AxError::InvalidInput)?;
            let bytes = vm_load(memory, value.cast(), length).map_err(map_usercopy_error)?;
            let entry_len = key
                .len()
                .checked_add(bytes.len())
                .and_then(|len| len.checked_add(1))
                .ok_or(AxError::NoMemory)?;
            if state.config_len.saturating_add(entry_len) > 64 * 1024 {
                return Err(LinuxError::E2BIG.into());
            }
            state.binary.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            state.binary.push((key, bytes));
            state.config_len += entry_len;
            Ok(0)
        }
        FSCONFIG_SET_PATH | FSCONFIG_SET_PATH_EMPTY => {
            let key = load_user_string(memory, key)?;
            // Btrfs's repeated member grammar is explicitly SET_STRING
            // `device=<path>`.  Do not successfully retain an unconsumed
            // SET_PATH member in the generic option side ledger.
            if state.fs_type == "btrfs" && key == "device" {
                return Err(AxError::InvalidInput);
            }
            let path = load_user_path(memory, value.cast())?;
            if cmd == FSCONFIG_SET_PATH_EMPTY && !path.as_bytes().is_empty() {
                return Err(AxError::InvalidInput);
            }
            let security = VfsSecurityContext::new(current().as_thread().current_cred());
            let at_flags = if cmd == FSCONFIG_SET_PATH_EMPTY {
                AT_EMPTY_PATH
            } else {
                0
            };
            let location = resolve_at_with_security(aux, Some(path.as_ref()), at_flags, &security)?
                .into_file()
                .ok_or(AxError::InvalidInput)?;
            let entry_len = key
                .len()
                .checked_add(path.as_bytes().len())
                .and_then(|len| len.checked_add(1))
                .ok_or(AxError::NoMemory)?;
            if state.config_len.saturating_add(entry_len) > 4096 {
                return Err(AxError::InvalidInput);
            }
            state.paths.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            state.paths.push((key.clone(), location));
            if state.fs_type == "overlay" {
                state
                    .overlay
                    .as_mut()
                    .ok_or(AxError::InvalidInput)?
                    .set_option(key.as_bytes(), path.as_bytes())
                    .map_err(AxError::from)?;
            }
            state.config_len += entry_len;
            Ok(0)
        }
        FSCONFIG_CMD_CREATE | FSCONFIG_CMD_CREATE_EXCL => {
            if state.created {
                return Err(AxError::ResourceBusy);
            }
            state.created = true;
            Ok(0)
        }
        FSCONFIG_CMD_RECONFIGURE => {
            // Keep the target topology, option ledger snapshot, and remount
            // commit in one namespace operation.  In particular an overlay
            // must not validate old lower/upper mounts and publish against a
            // different graph after a concurrent move/unmount.
            let _mount_operation = mounts::namespace_operation();
            let target = state
                .reconfigure_mount
                .as_ref()
                .ok_or(AxError::InvalidInput)?;
            let metadata = mounts::clone_metadata_for_bind(target)?;
            let flags = mounts::flags_for_location(target)?;
            let data = if metadata.fs_type == "xfs" {
                let current_options = parse_xfs_mount_options(&metadata.data)?;
                let requested_data = if state.data.is_empty() {
                    metadata.data.clone()
                } else {
                    try_string(&state.data)?
                };
                let requested_options = parse_xfs_mount_options(&requested_data)?;
                if !same_xfs_mount_options(&current_options, &requested_options) {
                    // fsconfig reconfigure does not reopen members or run a
                    // recovery-mode transition.  Reject instead of changing
                    // only the mount-record option ledger.
                    return Err(AxError::OperationNotSupported);
                }
                if requested_options.norecovery && flags & MS_RDONLY == 0 {
                    return Err(AxError::OperationNotSupported);
                }
                requested_data
            } else {
                try_string(&state.data)?
            };
            mounts::remount_with_data(target, metadata.source, metadata.fs_type, flags, data)?;
            reconcile_current_mount_topology()?;
            Ok(0)
        }
        _ => Err(LinuxError::EINVAL.into()),
    }
}

pub fn sys_fsmount(fd: i32, flags: u32, mount_attrs: u32) -> AxResult<isize> {
    if fd < 0 {
        return Err(AxError::BadFileDescriptor);
    }
    if !current_may_mount() {
        return Err(LinuxError::EPERM.into());
    }

    let file = get_file_like(fd)?;
    let fsopen = file
        .downcast_ref::<FsOpenFd>()
        .ok_or(AxError::BadFileDescriptor)?;
    let state = fsopen.0.lock();

    let cloexec = validate_fsmount(flags, mount_attrs).map_err(map_mount_uapi)?;
    if !state.created {
        return Err(AxError::InvalidInput);
    }
    let source = match state.source.as_deref() {
        Some(source) => FsPathBuf::from_vec(source.as_bytes().to_vec()),
        None => FsPathBuf::from_vec(b"none".to_vec()),
    };
    let fs_type = try_string(&state.fs_type)?;
    let data = try_string(&state.data)?;
    let fuse_connection = state.fuse_connection.clone();
    let nfs_transport = state.nfs_transport.clone();
    let nfs_options = state.nfs_options.clone();
    let overlay_options = state.overlay.clone();
    let configured_paths = state.paths.clone();
    drop(state);
    // Overlay resolves every constituent and takes its namespace/idmap view
    // under one operation gate.  The detached mount is not allocated until
    // that complete admission transaction has succeeded.
    // Overlay, XFS, and Btrfs resolve several mount constituents.  Keep their
    // complete resolution/claim/publication sequence in one namespace
    // operation so no role can be selected from a topology changed halfway
    // through the fsopen mount.
    let _provider_mount_operation =
        matches!(fs_type.as_str(), "overlay" | "xfs" | "btrfs").then(mounts::namespace_operation);
    let mut linux_device = None;
    let mut block_members = None;
    let fs = if fs_type == "overlay" {
        let options = overlay_options.ok_or(AxError::InvalidInput)?;
        let security = VfsSecurityContext::new(current().as_thread().current_cred());
        resolve_overlay_filesystem(&options, &configured_paths, &security)?
    } else if fs_type == "fuse" {
        let connection = fuse_connection.clone().ok_or(AxError::InvalidInput)?;
        crate::pseudofs::dev::fuse::mount_filesystem(connection)?
    } else if fs_type == "nfs4" {
        let transport = nfs_transport.clone().ok_or(AxError::InvalidInput)?;
        let auth = match nfs_options.security {
            NfsSecurityFlavor::Sys => RpcAuth::Sys(nfs_options.auth_sys.clone()),
            // Do not silently turn a requested protected flavour into AUTH_SYS.
            // The rpc.gssd bridge supplies the context object before a GSS
            // mount may be published (wired below by the rpc_pipefs provider).
            flavor @ (NfsSecurityFlavor::Krb5
            | NfsSecurityFlavor::Krb5i
            | NfsSecurityFlavor::Krb5p) => {
                let uid = current().as_thread().current_cred().ids().fsuid.into_raw();
                let queue = crate::pseudofs::rpc_pipefs::global_gssd_queue()?;
                let request = queue.submit_v1(uid, source.as_bytes(), b"nfs")?;
                let mut reply = queue.wait_reply(request)?;
                if reply.uid != uid || reply.target != source.as_bytes() || reply.service != b"nfs"
                {
                    queue.abandon_handoff(reply.daemon_generation, reply.context.key_serial);
                    return Err(LinuxError::EKEYREJECTED.into());
                }
                let lease = queue
                    .claim_handoff(reply.daemon_generation, reply.context.key_serial)
                    .map_err(|_| LinuxError::EPIPE)?;
                // The keyring record is a single-use courier, not a cache:
                // move it into the mechanism under its exact uid/target/service
                // identity and erase the store entry before publishing mount.
                let mut raw = crate::keyring::take_nfs_gss_context(
                    reply.context.key_serial,
                    uid,
                    source.as_bytes(),
                    b"nfs",
                )
                .map_err(|_| LinuxError::EKEYREJECTED)?;
                let imported_result = axfs::Krb5ImportedContext::parse(&raw);
                raw.fill(0);
                let imported = imported_result.map_err(|_| LinuxError::EKEYREJECTED)?;
                let service = match flavor {
                    NfsSecurityFlavor::Krb5 => axfs::RpcGssService::None,
                    NfsSecurityFlavor::Krb5i => axfs::RpcGssService::Integrity,
                    NfsSecurityFlavor::Krb5p => axfs::RpcGssService::Privacy,
                    _ => unreachable!(),
                };
                let wire_context = reply.context.take_wire_context();
                let mechanism = crate::nfs_gss::Krb5Gss::import(
                    imported,
                    wire_context,
                    service,
                    reply.context.timeout_seconds,
                    reply.context.window_size,
                )
                .map_err(|_| LinuxError::EKEYREJECTED)?;
                lease.consume();
                RpcAuth::Gss(Arc::try_new(mechanism).map_err(|_| AxError::NoMemory)?)
            }
        };
        let transport_impl = transport;
        let transport: Arc<dyn axfs::RpcTransport> = transport_impl.clone();
        let mount = Arc::try_new(NfsMount::new(Arc::clone(&transport), auth))
            .map_err(|_| AxError::NoMemory)?;
        transport_impl
            .install_callback_mount(Arc::downgrade(&mount))
            .map_err(|_| AxError::Io)?;
        if mount.negotiate(&nfs_options).is_err() {
            transport_impl.shutdown();
            return Err(AxError::Io);
        }
        NfsFilesystem::mount(mount).map_err(|_| AxError::Io)?
    } else if let Some(fs) = if matches!(
        fs_type.as_str(),
        "tmpfs"
            | "hugetlbfs"
            | "bpf"
            | "cgroup"
            | "cgroup2"
            | "proc"
            | "sysfs"
            | "devpts"
            | "mqueue"
            | "tracefs"
            | "debugfs"
            | "rpc_pipefs"
    ) {
        pseudo_fs_for_mount(
            core::str::from_utf8(source.as_bytes()).map_err(|_| AxError::IllegalBytes)?,
            &fs_type,
            &data,
        )
    } else {
        Ok(None)
    }? {
        fs
    } else {
        // The new mount API shares the same block-provider path as legacy
        // mount(2).  fsopen must therefore not be a tmpfs-only side route:
        // ext4/vfat receive the exact resolved device and typed FAT options.
        // XFS and Btrfs have explicitly claimed member sets, so they take
        // their shared parsers rather than this one-device path.
        let security = VfsSecurityContext::new(current().as_thread().current_cred());
        if fs_type == "xfs" {
            let (fs, device) = new_xfs_filesystem(
                source.as_ref(),
                &data,
                &security,
                mount_attrs & MOUNT_ATTR_RDONLY as u32 != 0,
            )?;
            linux_device = Some(device);
            fs
        } else if fs_type == "btrfs" {
            let (fs, device, members) = new_btrfs_filesystem(
                source.as_ref(),
                &data,
                &security,
                mount_attrs & MOUNT_ATTR_RDONLY as u32 != 0,
            )?;
            linux_device = Some(device);
            block_members = Some(members);
            fs
        } else {
            let source_loc = current_fs_context()
                .lock()
                .resolve_security(&source, &security)?;
            let device = source_loc.metadata()?;
            if device.node_type != NodeType::BlockDevice {
                return Err(LinuxError::ENOTBLK.into());
            }
            if mounts::is_nodev(&source_loc)? {
                return Err(AxError::PermissionDenied);
            }
            let name = block_device_name_for_rdev(device.rdev)?.ok_or(AxError::NoSuchDevice)?;
            let block = open_block_device(&name).map_err(|error| match error {
                OpenBlockDeviceError::NotFound => AxError::NoSuchDevice,
                OpenBlockDeviceError::Busy => AxError::ResourceBusy,
            })?;
            if block_device_is_read_only(&name).ok_or(AxError::NoSuchDevice)?
                && mount_attrs & MOUNT_ATTR_RDONLY as u32 == 0
            {
                return Err(AxError::PermissionDenied);
            }
            if fs_type == "vfat" {
                let credentials = security.credentials();
                let umask = current_fs_context().lock().umask() as u16;
                new_block_filesystem_with_fat_options(
                    &fs_type,
                    block,
                    parse_fat_mount_options(&data, credentials, umask)?,
                )?
            } else {
                if !data.is_empty() {
                    return Err(AxError::OperationNotSupported);
                }
                new_block_filesystem(&fs_type, block)?
            }
        }
    };
    let record_data = if fs_type == "cgroup"
        && data.is_empty()
        && !matches!(source.as_bytes(), b"none" | b"cgroup")
    {
        core::str::from_utf8(source.as_bytes())
            .map_err(|_| AxError::IllegalBytes)?
            .into()
    } else {
        data
    };
    let metadata = mounts::MountMetadata::new(
        source,
        fs_type,
        FsPathBuf::from_vec(b"/".to_vec()),
        record_data,
    )
    .with_block_members(block_members.unwrap_or_default());
    let mountpoint =
        mounts::new_detached_with_flags(&fs, mount_attr_to_mount_flags(mount_attrs), metadata)?;
    if let Some(linux_device) = linux_device {
        mounts::register_linux_device(mountpoint.filesystem_identity(), linux_device)?;
    }
    let mut build = DetachedTreeBuildGuard::new(mountpoint.clone());
    if fs.name() == "fuse" {
        build.fuse.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        if let Some(connection) = fuse_connection.as_ref() {
            crate::pseudofs::dev::fuse::register_mount_connection(
                mountpoint.mount_id(),
                connection,
            )?;
            build.fuse.push(mountpoint.mount_id());
        }
    }
    if fs.name() == "nfs4" {
        build.nfs.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        let client = match crate::pseudofs::rpc_pipefs::register_nfs_client() {
            Ok(client) => client,
            Err(error) => return Err(error),
        };
        let transport = match nfs_transport.as_ref() {
            Some(transport) => transport,
            None => {
                crate::pseudofs::rpc_pipefs::unregister_nfs_client(client.as_name());
                return Err(AxError::Io);
            }
        };
        if let Err(error) = register_nfs_mount(
            mountpoint.mount_id(),
            transport,
            &nfs_options,
            client.clone(),
        ) {
            crate::pseudofs::rpc_pipefs::unregister_nfs_client(client.as_name());
            return Err(error);
        }
        build.nfs.push(mountpoint.mount_id());
    }
    let tree = FsMountTreeState::try_new(
        HashMap::new(),
        None,
        Some(DetachedTreeLedger::singleton(
            mountpoint.clone(),
            None,
            None,
            false,
        )?),
        Vec::new(),
        Vec::new(),
    )?;
    let (rollback_fuse_mount_ids, rollback_nfs_mount_ids) = build.into_registrations();
    *tree.rollback_fuse_mount_ids.lock() = rollback_fuse_mount_ids;
    *tree.rollback_nfs_mount_ids.lock() = rollback_nfs_mount_ids;
    FsMountFd {
        root: mountpoint.root_location(),
        tree,
    }
    .add_to_fd_table(cloexec)
    .map(|new_fd| new_fd as isize)
}

pub fn sys_fspick<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    pathname: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    const FSPICK_CLOEXEC: u32 = 0x1;
    const FSPICK_SYMLINK_NOFOLLOW: u32 = 0x2;
    const FSPICK_NO_AUTOMOUNT: u32 = 0x4;
    const FSPICK_EMPTY_PATH: u32 = 0x8;
    if flags & !(FSPICK_CLOEXEC | FSPICK_SYMLINK_NOFOLLOW | FSPICK_NO_AUTOMOUNT | FSPICK_EMPTY_PATH)
        != 0
    {
        return Err(AxError::InvalidInput);
    }
    let security = VfsSecurityContext::new(current().as_thread().current_cred());
    if !may_mount(&security) {
        return Err(LinuxError::EPERM.into());
    }
    let path = if pathname.is_null() && flags & FSPICK_EMPTY_PATH != 0 {
        FsPathBuf::new()
    } else {
        load_user_path(memory, pathname)?
    };
    if path.as_bytes().is_empty() && flags & FSPICK_EMPTY_PATH == 0 {
        return Err(AxError::NotFound);
    }
    let resolve_flags = if flags & FSPICK_EMPTY_PATH != 0 {
        AT_EMPTY_PATH
    } else {
        0
    } | if flags & FSPICK_SYMLINK_NOFOLLOW != 0 {
        AT_SYMLINK_NOFOLLOW
    } else {
        0
    } | if flags & FSPICK_NO_AUTOMOUNT != 0 {
        AT_NO_AUTOMOUNT
    } else {
        0
    };
    let loc = resolve_at_with_security(dirfd, Some(path.as_ref()), resolve_flags, &security)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    let metadata = mounts::clone_metadata_for_bind(&loc)?;
    let fuse_connection = if metadata.fs_type == "fuse" {
        Some(
            crate::pseudofs::dev::fuse::mount_connection(loc.mountpoint().mount_id())
                .ok_or(LinuxError::ENODEV)?,
        )
    } else {
        None
    };
    let (nfs_transport, nfs_options) = if metadata.fs_type == "nfs4" {
        let pair = nfs_mount_transport(loc.mountpoint().mount_id()).ok_or(LinuxError::ENODEV)?;
        (Some(pair.0), pair.1)
    } else {
        (None, NfsMountOptions::default())
    };
    let overlay = if metadata.fs_type == "overlay" {
        Some(overlay_options_from_record(&metadata.data)?)
    } else {
        None
    };
    FsOpenFd(Mutex::new(FsOpenState {
        fs_type: try_string(&metadata.fs_type)?,
        source: Some(FsPathBuf::from_vec(metadata.source.as_bytes().to_vec())),
        data: try_string(&metadata.data)?,
        config_len: 0,
        // fspick produces a reconfiguration context, not an unconfigured
        // fsopen.  fsconfig can change it and fsmount can materialize it.
        created: true,
        fuse_connection,
        nfs_transport,
        nfs_options,
        overlay,
        binary: Vec::new(),
        paths: Vec::new(),
        reconfigure_mount: Some(loc),
    }))
    .add_to_fd_table(flags & FSPICK_CLOEXEC != 0)
    .map(|fd| fd as isize)
}

fn prepare_open_tree<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    pathname: *const c_char,
    flags: u32,
) -> AxResult<(FsMountFd, bool)> {
    let cloexec = validate_open_tree(flags).map_err(map_mount_uapi)?;
    let curr = current();
    let actor = curr.as_thread().current_cred();
    if flags & OPEN_TREE_CLONE != 0 {
        let gate_security = VfsSecurityContext::new(actor.clone());
        if !may_mount(&gate_security) {
            return Err(LinuxError::EPERM.into());
        }
    }

    // AT_EMPTY_PATH permits a NULL pathname, which is how open_tree_attr
    // applies attributes to its just-created detached mount FD.
    let path = if pathname.is_null() && flags & AT_EMPTY_PATH != 0 {
        FsPathBuf::new()
    } else {
        load_user_path(memory, pathname)?
    };
    debug!("sys_open_tree <= dirfd: {dirfd}, path: {path:?}, flags: {flags:#x}");
    // Every path that may also touch an FsMountTreeState observes the
    // namespace operation first.  move_mount and mount_setattr use the same
    // namespace -> tree ordering.
    let _mount_operation = mounts::namespace_operation();

    // A relative source can be a detached mount FD or an OFD whose opening
    // mount namespace is no longer current.  `open_tree` must retain that
    // authority for both AT_EMPTY_PATH and a component walk: resolving the
    // latter through `resolve_at_with_security` would make `dirfd` the VFS
    // root and silently rebind the path to the caller's current namespace.
    let source_file = (!path.is_absolute()
        && (!path.as_bytes().is_empty() || flags & AT_EMPTY_PATH != 0))
        .then(|| get_file_like(dirfd).ok())
        .flatten();
    // Freeze a detached source through resolution, snapshot and clone.  A
    // concurrent move_mount may otherwise consume the tree between a path
    // walk and its retained-ledger copy.
    let _source_tree_operation = source_file
        .as_ref()
        .and_then(|file| file.downcast_ref::<FsMountFd>())
        .map(|source| source.tree.operation.lock());
    let current_topology = curr.as_thread().mount_ns().topology();
    let source_topology = source_file.as_ref().and_then(|file| {
        file.downcast_ref::<FsMountFd>()
            .and_then(|source| source.tree.topology.lock().clone())
            .or_else(|| file.vfs_mount_topology())
    });

    // A normal retained VFS descriptor is created with a pinned topology.
    // Do not replace a missing provenance record with `current()`: that is
    // precisely the post-setns VFS/ledger split this resolver prevents.
    if !path.is_absolute()
        && !path.as_bytes().is_empty()
        && let Some(file) = source_file.as_ref()
        && file.downcast_ref::<FsMountFd>().is_none()
        && source_topology.is_none()
    {
        return Err(AxError::InvalidInput);
    }

    let security = VfsSecurityContext::new(actor);
    let loc = resolve_move_mount_path(
        dirfd,
        path.as_ref(),
        flags & AT_SYMLINK_NOFOLLOW == 0,
        flags & AT_NO_AUTOMOUNT == 0,
        flags & AT_EMPTY_PATH != 0,
        &security,
    )?;

    // `source_topology` is the sole namespace authority for a relative FD;
    // an absolute path is, as on Linux, resolved in the current namespace.
    // Detached FsMountFd instances have no ledger by design, so their idmap
    // map below remains the only authority in that case.
    let topology = if path.is_absolute() || dirfd == linux_raw_sys::general::AT_FDCWD {
        Some(current_topology.clone())
    } else {
        source_topology.clone()
    };
    let root_source_mount_id = loc.mountpoint().mount_id();
    let retained_topology_for =
        |source_mount_id: u64| -> AxResult<Option<Arc<mounts::MountTopology>>> {
            // The topology selected for the walk is authoritative.  In
            // particular, never consult the current topology after an
            // attached descriptor or detached mount FD selected another one.
            if let Some(topology) = topology.as_ref() {
                match topology.idmap_for_mount(source_mount_id) {
                    Ok(_) => return Ok(Some(topology.clone())),
                    // A location outside the retained ledger must not be
                    // rebound through a coincidentally matching live mount
                    // in another namespace.
                    Err(AxError::NotFound) => return Err(AxError::InvalidInput),
                    Err(error) => return Err(error),
                }
            }
            // Only a detached mount FD legitimately lacks a namespace
            // ledger.  Its frozen per-mount idmap map is consulted by
            // `retained_idmap_for`; every other descriptor was rejected
            // above if it had no retained topology for a component walk.
            Ok(None)
        };
    let retained_idmap_for = |source_mount_id: u64| -> AxResult<Option<Arc<mounts::MountIdmap>>> {
        if let Some(source_topology) = retained_topology_for(source_mount_id)? {
            return source_topology.idmap_for_mount(source_mount_id);
        }
        let Some(file) = source_file.as_ref() else {
            return Ok(None);
        };
        if let Some(source) = file.downcast_ref::<FsMountFd>()
            && let Some(idmap) = source.tree.idmaps.lock().get(&source_mount_id).cloned()
        {
            return Ok(Some(idmap));
        }
        Ok((file.vfs_mount_id() == Some(source_mount_id))
            .then(|| file.vfs_mount_idmap())
            .flatten())
    };

    if flags & OPEN_TREE_CLONE == 0 {
        // Without OPEN_TREE_CLONE, Linux returns a handle to the existing
        // attached mount tree.  It is intentionally not a move-capable clone.
        let mut idmaps = HashMap::new();
        let source_topology = retained_topology_for(root_source_mount_id)?;
        if let Some(source) = source_file
            .as_ref()
            .and_then(|file| file.downcast_ref::<FsMountFd>())
            && source_topology.is_none()
        {
            // Non-clone open_tree is another retained reference to this one
            // detached tree, not a separately owning rollback handle.
            return Ok((
                FsMountFd {
                    root: loc,
                    tree: source.tree.clone(),
                },
                cloexec,
            ));
        }
        let detached_ledger = if source_topology.is_none() {
            source_file
                .as_ref()
                .and_then(|file| file.downcast_ref::<FsMountFd>())
                .map(|source| {
                    source
                        .tree
                        .detached_ledger
                        .lock()
                        .as_ref()
                        .ok_or(AxError::InvalidInput)?
                        .try_clone()
                })
                .transpose()?
        } else {
            None
        };
        if let Some(source) = source_file
            .as_ref()
            .and_then(|file| file.downcast_ref::<FsMountFd>())
            && source_topology.is_none()
        {
            let retained = source.tree.idmaps.lock();
            idmaps
                .try_reserve(retained.len())
                .map_err(|_| AxError::NoMemory)?;
            idmaps.extend(
                retained
                    .iter()
                    .map(|(mount_id, idmap)| (*mount_id, idmap.clone())),
            );
        } else if let Some(idmap) = retained_idmap_for(root_source_mount_id)? {
            idmaps.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            idmaps.insert(root_source_mount_id, idmap);
        }
        return Ok((
            FsMountFd {
                root: loc,
                tree: FsMountTreeState::try_new(
                    idmaps,
                    source_topology,
                    detached_ledger,
                    Vec::new(),
                    Vec::new(),
                )?,
            },
            cloexec,
        ));
    }

    // Snapshot the complete selected source tree before creating any target
    // mount.  The attached case reads the retained namespace ledger; a
    // detached FsMountFd owns its equivalent private ledger.  Neither path
    // may discover descendants through `current()` after setns().
    let source_ledger = if let Some(source) = source_file
        .as_ref()
        .and_then(|file| file.downcast_ref::<FsMountFd>())
        && source.tree.topology.lock().is_none()
    {
        source
            .tree
            .detached_ledger
            .lock()
            .as_ref()
            .ok_or(AxError::InvalidInput)?
            .try_clone()?
    } else if let Some(topology) = topology.as_ref() {
        detached_ledger_from_topology(topology)?
    } else {
        return Err(AxError::InvalidInput);
    };
    let root_source = source_ledger.mount(root_source_mount_id)?;
    let inherited_idmap = retained_idmap_for(root_source_mount_id)?;
    let metadata = mounts::clone_metadata_for_bind(&loc)?;
    let filesystem = bind_filesystem_for(&loc, &metadata.fs_type)?;
    let root_is_fuse = metadata.fs_type == "fuse";
    let root_is_nfs = metadata.fs_type == "nfs4";
    let mountpoint =
        mounts::new_detached_with_flags(&filesystem, mounts::flags_for_location(&loc)?, metadata)?;
    let mut build = DetachedTreeBuildGuard::new(mountpoint.clone());
    let mut retained_idmaps = HashMap::new();
    if let Some(idmap) = inherited_idmap.as_ref() {
        retained_idmaps
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        retained_idmaps.insert(mountpoint.mount_id(), idmap.clone());
    }
    let mut detached_ledger = DetachedTreeLedger::singleton(
        mountpoint.clone(),
        inherited_idmap,
        root_source.peer_group,
        root_source.unbindable,
    )?;
    if root_is_fuse {
        let registration = (|| -> AxResult<()> {
            build.fuse.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            let connection =
                crate::pseudofs::dev::fuse::mount_connection(loc.mountpoint().mount_id())
                    .ok_or(LinuxError::ENODEV)?;
            crate::pseudofs::dev::fuse::register_mount_connection(
                mountpoint.mount_id(),
                &connection,
            )?;
            Ok(())
        })();
        registration?;
        build.fuse.push(mountpoint.mount_id());
    }
    if root_is_nfs {
        let registration = (|| -> AxResult<()> {
            build.nfs.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            clone_nfs_mount_registration(loc.mountpoint().mount_id(), mountpoint.mount_id())?;
            Ok(())
        })();
        registration?;
        build.nfs.push(mountpoint.mount_id());
    }
    if flags & AT_RECURSIVE != 0 {
        // OPEN_TREE_CLONE|AT_RECURSIVE clones the complete nested mount tree,
        // not merely the selected root filesystem view.
        let recursive_clone = (|| -> AxResult<()> {
            let detached = FsContext::new(mountpoint.root_location());
            let children = retained_recursive_submounts(&source_ledger, &loc)?;
            retained_idmaps
                .try_reserve(children.len())
                .map_err(|_| AxError::NoMemory)?;
            for child in children {
                let child_source_mount_id = child.source_id;
                let child_idmap = child.idmap.clone();
                let fuse_connection = if child.metadata.fs_type == "fuse" {
                    Some(
                        crate::pseudofs::dev::fuse::mount_connection(child_source_mount_id)
                            .ok_or(LinuxError::ENODEV)?,
                    )
                } else {
                    None
                };
                let nfs_source =
                    (child.metadata.fs_type == "nfs4").then_some(child_source_mount_id);
                let child_target = detached
                    .resolve(&child.relative_path)
                    .map_err(|_| AxError::Io)?;
                let child_fs = bind_filesystem_for(&child.source, &child.metadata.fs_type)?;
                let child_mount = mounts::mount_with_flags(
                    &child_target,
                    &child_fs,
                    child.flags,
                    child.metadata,
                )?;
                if let Some(idmap) = child_idmap.as_ref()
                    && retained_idmaps
                        .insert(child_mount.mount_id(), idmap.clone())
                        .is_some()
                {
                    return Err(AxError::Io);
                }
                detached_ledger
                    .mounts
                    .try_reserve(1)
                    .map_err(|_| AxError::NoMemory)?;
                detached_ledger.mounts.push(DetachedTreeMount {
                    mountpoint: child_mount.clone(),
                    parent: Some(child_target.mountpoint().mount_id()),
                    idmap: child_idmap,
                    peer_group: child.peer_group,
                    unbindable: child.unbindable,
                });
                if let Some(connection) = fuse_connection {
                    build.fuse.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    crate::pseudofs::dev::fuse::register_mount_connection(
                        child_mount.mount_id(),
                        &connection,
                    )?;
                    build.fuse.push(child_mount.mount_id());
                }
                if let Some(source_id) = nfs_source {
                    build.nfs.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    clone_nfs_mount_registration(source_id, child_mount.mount_id())?;
                    build.nfs.push(child_mount.mount_id());
                }
            }
            Ok(())
        })();
        recursive_clone?;
    }
    let tree = FsMountTreeState::try_new(
        retained_idmaps,
        None,
        Some(detached_ledger),
        Vec::new(),
        Vec::new(),
    )?;
    let (rollback_fuse_mount_ids, rollback_nfs_mount_ids) = build.into_registrations();
    *tree.rollback_fuse_mount_ids.lock() = rollback_fuse_mount_ids;
    *tree.rollback_nfs_mount_ids.lock() = rollback_nfs_mount_ids;
    Ok((
        FsMountFd {
            root: mountpoint.root_location(),
            tree,
        },
        cloexec,
    ))
}

pub fn sys_open_tree<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    pathname: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let (mount_fd, cloexec) = prepare_open_tree(memory, dirfd, pathname, flags)?;
    mount_fd
        .add_to_fd_table(cloexec)
        .map(|new_fd| new_fd as isize)
}

fn apply_mount_attr_to_mount_fd(
    mount_fd: &FsMountFd,
    recursive: bool,
    attr: mount_attr,
    topology_request: mounts::MountSetattrRequest,
) -> AxResult<()> {
    let _tree_operation = mount_fd.tree.operation.lock();
    let mountpoint = mount_fd.root.mountpoint();
    if mountpoint.is_attached() {
        if !mount_fd.root.is_root_of_mount() {
            return Err(AxError::InvalidInput);
        }
        let topology = mount_fd
            .tree
            .topology
            .lock()
            .clone()
            .unwrap_or_else(|| current_mount_namespace().topology());
        let prepared =
            topology.prepare_setattr(mountpoint.mount_id(), recursive, topology_request)?;
        if !mounts::try_update_flags_for_mounts(mountpoint.mount_id(), recursive, |current| {
            apply_mount_attr_flags(
                current,
                attr.attr_set & !(MOUNT_ATTR_IDMAP as u64),
                attr.attr_clr & !(MOUNT_ATTR_IDMAP as u64),
                0,
                0,
            )
            .map_err(map_mount_uapi)
        })? {
            return Err(AxError::InvalidInput);
        }
        prepared.commit()?;
        return Ok(());
    }

    if topology_request.propagation != 0 {
        return Err(AxError::InvalidInput);
    }
    let mut prepared_idmaps = if let Some(replacement) = topology_request.idmap.as_ref() {
        let current = mount_fd.tree.idmaps.lock();
        let targets = if recursive {
            mountpoint.subtree_mountpoints()?
        } else {
            let mut targets = Vec::new();
            targets.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            targets.push(mountpoint.clone());
            targets
        };
        if !topology_request.idmap_replace
            && targets
                .iter()
                .any(|target| current.contains_key(&target.mount_id()))
        {
            return Err(AxError::InvalidInput);
        }
        let mut next = HashMap::new();
        next.try_reserve(current.len().saturating_add(targets.len()))
            .map_err(|_| AxError::NoMemory)?;
        for (mount_id, idmap) in current.iter() {
            next.insert(*mount_id, idmap.clone());
        }
        match replacement {
            Some(idmap) => {
                for target in targets {
                    next.insert(target.mount_id(), idmap.clone());
                }
            }
            None => {
                for target in targets {
                    next.remove(&target.mount_id());
                }
            }
        }
        Some((current, next))
    } else {
        None
    };
    mounts::update_detached_mount_flags(mountpoint, recursive, |current| {
        apply_mount_attr_flags(
            current,
            attr.attr_set & !(MOUNT_ATTR_IDMAP as u64),
            attr.attr_clr & !(MOUNT_ATTR_IDMAP as u64),
            0,
            0,
        )
        .map_err(map_mount_uapi)
    })?;
    if let Some((mut current, next)) = prepared_idmaps.take() {
        // The detached ledger is the source of a later recursive open_tree;
        // update it atomically with the FD-visible idmap map so the clone
        // cannot inherit a stale credential projection.
        if let Some(ledger) = mount_fd.tree.detached_ledger.lock().as_mut() {
            for mount in &mut ledger.mounts {
                mount.idmap = next.get(&mount.mountpoint.mount_id()).cloned();
            }
        }
        *current = next;
    }
    Ok(())
}

/// v6.18's combined detached-tree creation and attribute transaction.
///
/// `vfs_open_tree()` prepares the file before `wants_mount_setattr()` reads the
/// optional attribute, while fd allocation happens only after both succeed.
/// Keeping the prepared `FsMountFd` unpublished reproduces that ordering and
/// lets Drop tear down a failed detached clone without exposing an fd number.
pub fn sys_open_tree_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    pathname: *const c_char,
    flags: u32,
    attr: *const mount_attr,
    size: usize,
) -> AxResult<isize> {
    if attr.is_null() && size != 0 {
        return Err(AxError::InvalidInput);
    }
    let (mount_fd, cloexec) = prepare_open_tree(memory, dirfd, pathname, flags)?;
    if !attr.is_null() {
        if size > PAGE_SIZE {
            return Err(LinuxError::E2BIG.into());
        }
        if size < size_of::<MountAttrUser>() {
            return Err(AxError::InvalidInput);
        }
        let security = VfsSecurityContext::new(current().as_thread().current_cred());
        if !may_mount(&security) {
            return Err(LinuxError::EPERM.into());
        }
        let copied_attr = copy_mount_attr(memory, attr, size)?;
        if !mount_setattr_is_noop(copied_attr) {
            let request =
                mount_setattr_request_with_replace(copied_attr, flags & OPEN_TREE_CLONE != 0)?;
            let _mount_operation = mounts::namespace_operation();
            apply_mount_attr_to_mount_fd(
                &mount_fd,
                flags & AT_RECURSIVE != 0,
                copied_attr,
                request,
            )?;
        }
    }
    mount_fd
        .add_to_fd_table(cloexec)
        .map(|new_fd| new_fd as isize)
}

pub fn sys_mount_setattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    pathname: *const c_char,
    flags: u32,
    attr_ptr: *const mount_attr,
    size: usize,
) -> AxResult<isize> {
    validate_mount_setattr_flags(flags, size).map_err(map_mount_uapi)?;
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if !may_mount(&security) {
        return Err(LinuxError::EPERM.into());
    }

    let attr = copy_mount_attr(memory, attr_ptr, size)?;
    mount_setattr_from_copied(memory, dirfd, pathname, flags, attr)
}

/// Applies a mount attribute object which has already passed the ABI copier.
/// Keeping this separate is what lets `open_tree_attr` retain one atomic
/// usercopy boundary while ordinary `mount_setattr` retains its existing ABI
/// validation and capability ordering.
fn mount_setattr_from_copied<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dirfd: i32,
    pathname: *const c_char,
    flags: u32,
    attr: mount_attr,
) -> AxResult<isize> {
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    // wants_mount_setattr() returns before build_mount_kattr() for a no-op.
    // In particular, a no-op neither validates userns_fd nor reads pathname.
    if mount_setattr_is_noop(attr) {
        return Ok(0);
    }
    // Attribute-only validation must precede target lookup.  This preserves
    // Linux's errno order for an invalid attribute paired with a bad path.
    let topology_request = mount_setattr_request(attr)?;

    let path = if pathname.is_null() && flags & AT_EMPTY_PATH != 0 {
        FsPathBuf::new()
    } else {
        load_user_path(memory, pathname)?
    };
    debug!("sys_mount_setattr <= dirfd: {dirfd}, path: {path:?}, flags: {flags:#x}");

    let _mount_operation = mounts::namespace_operation();
    let mount_ns = current_mount_namespace();
    if path.as_bytes().is_empty() && flags & AT_EMPTY_PATH != 0 {
        let file = get_file_like(dirfd)?;
        if let Some(mount_fd) = file.downcast_ref::<FsMountFd>() {
            apply_mount_attr_to_mount_fd(
                mount_fd,
                flags & AT_RECURSIVE != 0,
                attr,
                topology_request,
            )?;
            return Ok(0);
        }
    }

    let loc = resolve_at_with_security(dirfd, Some(path.as_ref()), flags, &security)?
        .into_file()
        .ok_or(AxError::InvalidInput)?;
    if !loc.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    let prepared = mount_ns.topology().prepare_setattr(
        loc.mountpoint().mount_id(),
        flags & AT_RECURSIVE != 0,
        topology_request,
    )?;
    if !mounts::try_update_flags_for_mounts(
        loc.mountpoint().mount_id(),
        flags & AT_RECURSIVE != 0,
        |current| {
            apply_mount_attr_flags(
                current,
                attr.attr_set & !(MOUNT_ATTR_IDMAP as u64),
                attr.attr_clr & !(MOUNT_ATTR_IDMAP as u64),
                0,
                0,
            )
            .map_err(map_mount_uapi)
        },
    )? {
        return Err(AxError::InvalidInput);
    }
    prepared.commit()?;
    Ok(0)
}

/// A relative pathname supplied with an existing descriptor keeps the
/// descriptor's mount-namespace view after setns().  The VFS mount tree is
/// shared storage, so merely selecting retained credentials is not enough:
/// every walk edge must also remain a member of the retained topology.
struct RetainedMountPathwalk {
    topology: Arc<mounts::MountTopology>,
}

impl RetainedMountPathwalk {
    fn admit(&self, location: &Location) -> VfsResult<()> {
        self.topology
            .idmap_for_mount(location.mountpoint().mount_id())
            .map(|_| ())
    }
}

impl PathwalkPolicy for RetainedMountPathwalk {
    fn component(
        &mut self,
        directory: &Location,
        _component: PathwalkComponent<'_>,
    ) -> VfsResult<()> {
        self.admit(directory)
    }

    fn cross_mount(&mut self, from: &Location, to: &Location) -> VfsResult<()> {
        self.admit(from)?;
        self.admit(to)
    }

    fn absolute_root(&mut self, from: &Location, root: &Location) -> VfsResult<()> {
        self.admit(from)?;
        self.admit(root)
    }

    fn escape_root(&mut self, root: &Location) -> VfsResult<()> {
        self.admit(root)
    }
}

/// Detached mount trees deliberately have no namespace ledger yet.  Their
/// root confines the walk, while the detached-idmap security context supplies
/// DAC projection; no current-namespace membership check may be injected.
struct DetachedMountPathwalk;

impl PathwalkPolicy for DetachedMountPathwalk {}

fn resolve_move_mount_detached_path(
    fs: &FsContext,
    path: &FsPath,
    follow_symlinks: bool,
    automount: bool,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    let mut policy = DetachedMountPathwalk;
    let mut admission = |directory: &Location| {
        check_pathwalk_search_permission_with_vfs_security(directory, security)
    };
    if follow_symlinks {
        if automount {
            fs.resolve_with_automount_policy(path, &mut admission, &mut policy)
        } else {
            fs.resolve_with_policy(path, &mut admission, &mut policy)
        }
    } else if automount {
        fs.resolve_no_follow_with_automount_policy(path, &mut admission, &mut policy)
    } else {
        fs.resolve_no_follow_with_policy(path, &mut admission, &mut policy)
    }
}

fn resolve_move_mount_retained_path(
    fs: &FsContext,
    path: &FsPath,
    follow_symlinks: bool,
    automount: bool,
    topology: Arc<mounts::MountTopology>,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    let mut policy = RetainedMountPathwalk { topology };
    let mut admission = |directory: &Location| {
        check_pathwalk_search_permission_with_vfs_security(directory, security)
    };
    if follow_symlinks {
        if automount {
            fs.resolve_with_automount_policy(path, &mut admission, &mut policy)
        } else {
            fs.resolve_with_policy(path, &mut admission, &mut policy)
        }
    } else if automount {
        fs.resolve_no_follow_with_automount_policy(path, &mut admission, &mut policy)
    } else {
        fs.resolve_no_follow_with_policy(path, &mut admission, &mut policy)
    }
}

fn resolve_move_mount_path(
    dirfd: i32,
    path: &FsPath,
    follow_symlinks: bool,
    automount: bool,
    empty_path: bool,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    if path.as_bytes().is_empty() {
        // LOOKUP_EMPTY is a property of the pathname walk, not of the
        // descriptor.  Check it before acquiring a dirfd so `open_tree(fd,
        // "", 0)` reports ENOENT even for a bad or anonymous FD, while
        // MOVE_MOUNT_*_EMPTY_PATH retains its independently selected path.
        if !empty_path {
            return Err(AxError::NotFound);
        }
        // `getname_maybe_null(..., AT_EMPTY_PATH)` takes the exact retained
        // file path.  A detached mount FD has the same retained authority,
        // but is not a normal VFS `FileLike`, so resolve it here rather than
        // asking the generic metadata-fd resolver to manufacture a pathname.
        if dirfd != linux_raw_sys::general::AT_FDCWD {
            let file = get_file_like(dirfd)?;
            if let Some(mount_fd) = file.downcast_ref::<FsMountFd>() {
                return Ok(mount_fd.root.clone());
            }
            // Linux's `fd_file(...)->f_path` is not restricted to this
            // kernel's File/Directory wrappers.  Accept every descriptor
            // that retains a concrete VFS location (including O_PATH), and
            // report EINVAL for anonymous descriptors instead of inventing
            // EBADF after the fd itself was successfully acquired.
            return file.vfs_location().cloned().ok_or(AxError::InvalidInput);
        }
        return resolve_at_with_security(
            dirfd,
            Some(path),
            if empty_path { AT_EMPTY_PATH } else { 0 },
            security,
        )?
        .into_file()
        .ok_or(AxError::InvalidInput);
    }
    if !path.is_absolute() && dirfd != linux_raw_sys::general::AT_FDCWD {
        let fd = get_file_like(dirfd)?;
        let (base, topology, detached_idmaps) =
            if let Some(mount_fd) = fd.downcast_ref::<FsMountFd>() {
                let topology = mount_fd
                    .tree
                    .topology
                    .lock()
                    .clone()
                    .or_else(|| fd.vfs_mount_topology());
                let detached_idmaps = if topology.is_none() {
                    let idmaps = mount_fd.tree.idmaps.lock();
                    let mut retained = Vec::new();
                    retained
                        .try_reserve_exact(idmaps.len())
                        .map_err(|_| AxError::NoMemory)?;
                    retained.extend(
                        idmaps
                            .iter()
                            .map(|(mount_id, idmap)| (*mount_id, idmap.clone())),
                    );
                    Some(retained)
                } else {
                    None
                };
                (mount_fd.root.clone(), topology, detached_idmaps)
            } else if let Some(file) = fd.downcast_ref::<File>() {
                (
                    file.inner().location().clone(),
                    fd.vfs_mount_topology(),
                    None,
                )
            } else if let Some(directory) = fd.downcast_ref::<Directory>() {
                (directory.inner().clone(), fd.vfs_mount_topology(), None)
            } else {
                return Err(AxError::BadFileDescriptor);
            };
        let lookup_security = if let Some(topology) = topology.as_ref() {
            VfsSecurityContext::with_execution_authority(
                security.actor_arc().clone(),
                topology.clone(),
                current().as_thread().landlock_domain(),
            )
        } else if let Some(idmaps) = detached_idmaps {
            VfsSecurityContext::with_detached_mount_authority(
                security.actor_arc().clone(),
                idmaps,
                current().as_thread().landlock_domain(),
            )?
        } else {
            security.clone()
        };
        let fs = if let Some(topology) = topology.as_ref() {
            FsContext::new(topology.root_location()?).with_current_dir(base)?
        } else {
            // A detached tree has no parent namespace.  Its mount root is
            // both root and cwd until move_mount publishes it.
            FsContext::new(base)
        };
        let location = if let Some(topology) = topology {
            resolve_move_mount_retained_path(
                &fs,
                path,
                follow_symlinks,
                automount,
                topology,
                &lookup_security,
            )
        } else {
            resolve_move_mount_detached_path(
                &fs,
                path,
                follow_symlinks,
                automount,
                &lookup_security,
            )
        }?;
        return Ok(location);
    }
    let location = with_path_fs(dirfd, path, |fs| {
        if follow_symlinks {
            if automount {
                fs.resolve_with_automount_policy(
                    path,
                    &mut |directory| {
                        check_pathwalk_search_permission_with_vfs_security(directory, security)
                    },
                    &mut RetainedMountPathwalk {
                        topology: current_mount_namespace().topology(),
                    },
                )
            } else {
                fs.resolve_security(path, security)
            }
        } else if automount {
            fs.resolve_no_follow_with_automount_policy(
                path,
                &mut |directory| {
                    check_pathwalk_search_permission_with_vfs_security(directory, security)
                },
                &mut RetainedMountPathwalk {
                    topology: current_mount_namespace().topology(),
                },
            )
        } else {
            fs.resolve_no_follow_security(path, security)
        }
    })?;
    Ok(location)
}

fn load_move_mount_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pathname: *const c_char,
    empty_path: bool,
) -> AxResult<FsPathBuf> {
    // getname_maybe_null() treats a NULL pathname as the empty pathname only
    // for the corresponding *_EMPTY_PATH flag.  Preserve that distinction so
    // an fd-based move never attempts a user-memory read of NULL.
    if pathname.is_null() && empty_path {
        Ok(FsPathBuf::new())
    } else {
        load_user_path(memory, pathname)
    }
}

fn move_mount_target_beneath(target: &Location) -> AxResult<Location> {
    // do_lock_mount(..., beneath=true) requires path_mounted(path): the
    // supplied target itself must be a mounted root with a parent.  Using a
    // non-root dentry would incorrectly turn BENEATH into an ordinary lower
    // mount attachment.
    if !target.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    target.mountpoint().location().ok_or(AxError::InvalidInput)
}

fn ensure_current_move_mount_location(location: &Location) -> AxResult<()> {
    // The VFS object carried by an old-namespace dirfd may remain alive after
    // setns(), but move_mount's structural transaction is confined to the
    // caller's current mount namespace.  Publishing that object through the
    // current ledger would otherwise split VFS and topology state.
    match current_mount_namespace()
        .topology()
        .idmap_for_mount(location.mountpoint().mount_id())
    {
        Ok(_) => Ok(()),
        Err(AxError::NotFound) => Err(AxError::InvalidInput),
        Err(error) => Err(error),
    }
}

fn topology_mount_is_ancestor(
    topology: &mounts::MountTopologySnapshot,
    ancestor: u64,
    mut descendant: u64,
) -> AxResult<bool> {
    for _ in 0..topology.mounts.len() {
        if descendant == ancestor {
            return Ok(true);
        }
        let mount = topology
            .mounts
            .iter()
            .find(|mount| mount.id == descendant)
            .ok_or(AxError::InvalidInput)?;
        let Some(parent) = mount.parent else {
            return Ok(false);
        };
        descendant = parent;
    }
    Err(AxError::InvalidInput)
}

fn topology_propagation_would_overmount(
    topology: &mounts::MountTopologySnapshot,
    from: &mounts::Mount,
    to: &mounts::Mount,
    future_mountpoint: &Location,
) -> AxResult<bool> {
    let Some(from_group) = from.peer_group else {
        return Ok(false);
    };
    if from_group.master.is_some()
        || !to
            .mountpoint()?
            .root_location()
            .entry()
            .ptr_eq(future_mountpoint.entry())
    {
        return Ok(false);
    }
    let mut group = to.peer_group;
    for _ in 0..topology.mounts.len() {
        let Some(candidate) = group else {
            return Ok(false);
        };
        if candidate.id == from_group.id {
            return Ok(true);
        }
        group = match candidate.master {
            Some(master) => topology
                .mounts
                .iter()
                .find_map(|mount| (mount.peer_group?.id == master).then_some(mount.peer_group))
                .ok_or(AxError::InvalidInput)?,
            None => return Ok(false),
        };
    }
    Err(AxError::InvalidInput)
}

/// Mirrors `can_move_mount_beneath()` before the shared VFS tree is mutated.
/// The namespace-operation guard held by `sys_move_mount` serializes this
/// snapshot with the eventual topology transaction, which is this VFS's
/// counterpart to Linux's target mount lock.
fn validate_move_mount_beneath(
    source: Option<&Location>,
    target: &Location,
    covered_target: &Location,
) -> AxResult<()> {
    let topology = current_mount_namespace().topology().try_snapshot()?;
    let target_id = target.mountpoint().mount_id();
    let parent_id = covered_target.mountpoint().mount_id();
    let target_mount = topology
        .mounts
        .iter()
        .find(|mount| mount.id == target_id)
        .ok_or(AxError::InvalidInput)?;
    let parent_mount = topology
        .mounts
        .iter()
        .find(|mount| mount.id == parent_id)
        .ok_or(AxError::InvalidInput)?;
    let namespace_root = topology
        .mounts
        .iter()
        .find(|mount| mount.parent.is_none())
        .ok_or(AxError::InvalidInput)?;

    // Linux forbids shadowing either the caller's root mount or the mount
    // namespace's root from beneath.  `covered_target` is the future
    // mountpoint in the parent mount, so its mount is Linux's
    // `parent_mnt_to`.
    if target_mount.locked
        || target.mountpoint().is_placement_locked()
        || current_fs_context()
            .lock()
            .root_dir()
            .mountpoint()
            .mount_id()
            == target_id
        || parent_mount.id == namespace_root.id
    {
        return Err(AxError::InvalidInput);
    }

    if let Some(source) = source {
        let source_id = source.mountpoint().mount_id();
        let source_mount = topology
            .mounts
            .iter()
            .find(|mount| mount.id == source_id)
            .ok_or(AxError::InvalidInput)?;

        // A retained FD for an overmounted lower mount must not manufacture a
        // shadow mount.  The child table gives the VFS's exact current top
        // mount at the source's former mountpoint.
        if source
            .mountpoint()
            .location()
            .and_then(|location| location.mounted_child())
            .is_some_and(|top| !Arc::ptr_eq(&top, source.mountpoint()))
            || topology_mount_is_ancestor(&topology, target_id, source_id)?
        {
            return Err(AxError::InvalidInput);
        }

        if topology_propagation_would_overmount(
            &topology,
            parent_mount,
            target_mount,
            covered_target,
        )? || topology_propagation_would_overmount(
            &topology,
            parent_mount,
            source_mount,
            covered_target,
        )? {
            return Err(AxError::InvalidInput);
        }
    } else if topology_propagation_would_overmount(
        &topology,
        parent_mount,
        target_mount,
        covered_target,
    )? {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn set_move_mount_group(source: &Location, target: &Location) -> AxResult<()> {
    if !source.is_root_of_mount() || !target.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    let prepared = current_mount_namespace()
        .topology()
        .prepare_join_propagation_group(
            source.mountpoint().mount_id(),
            target.mountpoint().mount_id(),
        )?;
    prepared.commit()
}

fn move_attached_mount(source: &Location, target: &Location, flags: u32) -> AxResult<()> {
    if !source.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    ensure_current_move_mount_location(source)?;
    ensure_current_move_mount_location(target)?;
    if flags & MOVE_MOUNT_SET_GROUP != 0 {
        return set_move_mount_group(source, target);
    }
    if source.mountpoint().is_placement_locked() {
        return Err(AxError::InvalidInput);
    }
    let target = if flags & MOVE_MOUNT_BENEATH != 0 {
        let covered = move_mount_target_beneath(target)?;
        validate_move_mount_beneath(Some(source), target, &covered)?;
        covered
    } else {
        target.clone()
    };
    mounts::move_tree_and_records(source, &target)?;
    reconcile_current_mount_topology()
}

pub fn sys_move_mount<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    from_dirfd: i32,
    from_pathname: *const c_char,
    to_dirfd: i32,
    to_pathname: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if !may_mount(&security) {
        return Err(LinuxError::EPERM.into());
    }
    // v6.18 rejects impossible flag combinations before touching either user
    // pathname.  The empty-path half of validation follows each corresponding
    // getname operation below, preserving target-before-source lookup order.
    validate_move_mount(flags, false, false).map_err(map_mount_uapi)?;
    let to_path = load_move_mount_path(memory, to_pathname, flags & MOVE_MOUNT_T_EMPTY_PATH != 0)?;
    debug!(
        "sys_move_mount <= from_dirfd: {from_dirfd}, to_dirfd: {to_dirfd}, to_path: {to_path:?}, \
         flags: {flags:#x}"
    );

    validate_move_mount(flags, false, to_path.as_bytes().is_empty()).map_err(map_mount_uapi)?;

    let _mount_operation = mounts::namespace_operation();
    let target = resolve_move_mount_path(
        to_dirfd,
        to_path.as_ref(),
        flags & MOVE_MOUNT_T_SYMLINKS != 0,
        flags & MOVE_MOUNT_T_AUTOMOUNTS != 0,
        flags & MOVE_MOUNT_T_EMPTY_PATH != 0,
        &security,
    )?;
    ensure_current_move_mount_location(&target)?;

    let from_path =
        load_move_mount_path(memory, from_pathname, flags & MOVE_MOUNT_F_EMPTY_PATH != 0)?;
    validate_move_mount(
        flags,
        from_path.as_bytes().is_empty(),
        to_path.as_bytes().is_empty(),
    )
    .map_err(map_mount_uapi)?;

    if from_path.as_bytes().is_empty() {
        let file = get_file_like(from_dirfd)?;
        if let Some(mount_fd) = file.downcast_ref::<FsMountFd>() {
            if mount_fd.root.mountpoint().is_attached() {
                move_attached_mount(&mount_fd.root, &target, flags)?;
            } else {
                let _tree_operation = mount_fd.tree.operation.lock();
                // A sibling non-clone mount FD may have consumed this shared
                // detached tree while this syscall waited for the operation
                // lock.  Reclassify before attaching so the tree cannot be
                // published twice.
                if mount_fd.root.mountpoint().is_attached() {
                    move_attached_mount(&mount_fd.root, &target, flags)?;
                    return Ok(0);
                }
                if flags & MOVE_MOUNT_SET_GROUP != 0 {
                    return Err(AxError::InvalidInput);
                }
                let target = if flags & MOVE_MOUNT_BENEATH != 0 {
                    let covered = move_mount_target_beneath(&target)?;
                    validate_move_mount_beneath(None, &target, &covered)?;
                    covered
                } else {
                    target
                };
                let propagation = mount_fd
                    .tree
                    .detached_ledger
                    .lock()
                    .as_ref()
                    .map(DetachedTreeLedger::propagation)
                    .transpose()?
                    .unwrap_or_default();
                let idmaps = mount_fd.tree.idmaps.lock();
                mounts::attach_tree_and_record_with_idmaps_and_propagation(
                    mount_fd.root.mountpoint(),
                    &target,
                    &idmaps,
                    &propagation,
                )?;
                drop(idmaps);
                // Retain the immutable applied mappings for future
                // AT_EMPTY_PATH cloning after setns, and pin the ledger for
                // nested mounts added after this FD was created.
                *mount_fd.tree.topology.lock() = Some(current_mount_namespace().topology());
                *mount_fd.tree.detached_ledger.lock() = None;
                // Successful publication transfers FUSE/NFS registration
                // lifetime to the namespace mount tree.  Disarm every
                // detached rollback receipt while the shared tree operation
                // remains serialized; a later namespace unmount followed by
                // final-FD Drop must not unregister it a second time.
                mount_fd.tree.rollback_fuse_mount_ids.lock().clear();
                mount_fd.tree.rollback_nfs_mount_ids.lock().clear();
                return Ok(0);
            }
        } else {
            let source = file.vfs_location().cloned().ok_or(AxError::InvalidInput)?;
            move_attached_mount(&source, &target, flags)?;
        }
    } else {
        let source = resolve_move_mount_path(
            from_dirfd,
            from_path.as_ref(),
            flags & MOVE_MOUNT_F_SYMLINKS != 0,
            flags & MOVE_MOUNT_F_AUTOMOUNTS != 0,
            false,
            &security,
        )?;
        move_attached_mount(&source, &target, flags)?;
    }
    Ok(0)
}

pub fn sys_mount<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    source: *const c_char,
    target: *const c_char,
    fs_type: *const c_char,
    flags: i32,
    data: *const c_void,
) -> AxResult<isize> {
    let source = if source.is_null() {
        FsPathBuf::new()
    } else {
        load_user_path(memory, source)?
    };
    let target = load_user_path(memory, target)?;
    let fs_type = if fs_type.is_null() {
        String::new()
    } else {
        load_user_string(memory, fs_type)?
    };
    debug!("sys_mount <= source: {source:?}, target: {target:?}, fs_type: {fs_type:?}");

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let credentials = security.credentials();
    let umask = current_fs_context().lock().umask() as u16;
    if !may_mount(&security) {
        return Err(AxError::from(LinuxError::EPERM));
    }
    let flags_u32 = validate_mount_flags(flags).map_err(map_mount_uapi)?;
    let _mount_operation = mounts::namespace_operation();
    let target = current_fs_context()
        .lock()
        .resolve_security(target.as_ref(), &security)?;
    let normalized_fs = match fs_type.as_str() {
        name if name.starts_with("vfat") => "vfat",
        "fat" | "msdos" => "vfat",
        name => name,
    };
    let data_is_ignored = flags_u32 & (MS_BIND | MS_MOVE | MS_PROPAGATION_FLAGS) != 0;
    // Legacy overlay options are byte grammar, just like fsconfig's STRING
    // and PATH forms.  Do not force a layer name through UTF-8 before the
    // common resolved-location constructor sees it.
    let overlay_data = if !data_is_ignored && !data.is_null() && normalized_fs == "overlay" {
        Some(
            vm_load_until_nul(memory, (data as *const c_char).cast::<u8>())
                .map_err(map_usercopy_error)?,
        )
    } else {
        None
    };
    let data = if !data_is_ignored && !data.is_null() && normalized_fs != "overlay" {
        load_user_string(memory, data as *const c_char)?
    } else {
        String::new()
    };
    let mut data = if normalized_fs == "overlay" {
        overlay_record_data(overlay_data.as_deref().unwrap_or_default())?
    } else {
        data
    };

    if flags_u32 & MS_REMOUNT != 0 {
        if !target.is_root_of_mount() {
            return Err(AxError::InvalidInput);
        }
        if flags_u32 & (MS_MOVE | MS_PROPAGATION_FLAGS) != 0 {
            return Err(AxError::OperationNotSupported);
        }
        let current_flags = mounts::flags_for_location(&target)?;
        if flags_u32 & MS_BIND != 0 {
            if flags_u32 & MS_REC != 0 {
                return Err(AxError::OperationNotSupported);
            }
            let allowed = MS_REMOUNT | MS_BIND | MS_SILENT | MS_BIND_REMOUNT_FLAGS;
            if flags_u32 & !allowed != 0 || !data.is_empty() {
                return Err(AxError::OperationNotSupported);
            }
            let bind_flags =
                normalize_mount_atime(flags_u32, Some(current_flags)) & MS_BIND_REMOUNT_FLAGS;
            mounts::remount_with_data(
                &target,
                source,
                try_string(normalized_fs)?,
                bind_flags,
                data,
            )?;
            return Ok(0);
        }
        let mut remount_flags = normalize_mount_atime(flags_u32, Some(current_flags));
        remount_flags &= !(MS_REMOUNT | MS_SILENT | MS_REC);
        let current_metadata = mounts::clone_metadata_for_bind(&target)?;
        if current_metadata.fs_type == "xfs" {
            let current_options = parse_xfs_mount_options(&current_metadata.data)?;
            let requested_options = if data.is_empty() {
                // No mount(2) data means no XFS option transition.  Retain
                // the original ledger so a `norecovery` mount cannot appear
                // to lose its recovery constraint at remount time.
                data = current_metadata.data.clone();
                parse_xfs_mount_options(&data)?
            } else {
                parse_xfs_mount_options(&data)?
            };
            if !same_xfs_mount_options(&current_options, &requested_options) {
                // Changing member roles or recovery policy requires a real
                // provider transition; this remount path has none, so never
                // publish a ledger-only success.
                return Err(AxError::OperationNotSupported);
            }
            if requested_options.norecovery && remount_flags & MS_RDONLY == 0 {
                return Err(AxError::OperationNotSupported);
            }
        }
        mounts::remount_with_data(
            &target,
            source,
            try_string(normalized_fs)?,
            remount_flags,
            data,
        )?;
        return Ok(0);
    }

    if flags_u32 & MS_BIND != 0 {
        do_bind_mount(&source, &target, flags_u32, &security)?;
        reconcile_current_mount_topology()?;
        return Ok(0);
    }

    if flags_u32 & MS_PROPAGATION_FLAGS != 0 {
        let allowed = MS_PROPAGATION_FLAGS | MS_REC | MS_SILENT;
        if flags_u32 & !allowed != 0 || (flags_u32 & MS_PROPAGATION_FLAGS).count_ones() != 1 {
            return Err(AxError::InvalidInput);
        }
        let propagation = (flags_u32 & MS_PROPAGATION_FLAGS) as u64;
        let prepared = current_mount_namespace().topology().prepare_setattr(
            target.mountpoint().mount_id(),
            flags_u32 & MS_REC != 0,
            mounts::MountSetattrRequest {
                attr_set: 0,
                attr_clr: 0,
                propagation,
                idmap: None,
                idmap_replace: false,
            },
        )?;
        prepared.commit()?;
        return Ok(0);
    }

    if flags_u32 & MS_MOVE != 0 {
        do_move_mount_old(&source, &target, &security)?;
        reconcile_current_mount_topology()?;
        return Ok(0);
    }

    if source.is_empty() || fs_type.is_empty() {
        return Err(AxError::InvalidInput);
    }
    if filesystem_type(normalized_fs).is_none() {
        return Err(AxError::NoSuchDevice);
    }
    let mount_flags = normalize_mount_atime(flags_u32, None) & !(MS_REC | MS_SILENT);

    let (fs, linux_device, block_members) = if normalized_fs == "overlay" {
        // Legacy mount and the fsopen family deliberately enter the same
        // resolved-location constructor.  In particular, textual paths are
        // resolved once before topology/idmap admission, never reparsed by a
        // provider after another namespace mutation.
        let options =
            legacy_overlay_options(overlay_data.as_deref().ok_or(AxError::InvalidInput)?)?;
        (
            resolve_overlay_filesystem(&options, &[], &security)?,
            None,
            None,
        )
    } else if let Some(fs) = if matches!(
        normalized_fs,
        "tmpfs"
            | "hugetlbfs"
            | "bpf"
            | "cgroup"
            | "cgroup2"
            | "proc"
            | "sysfs"
            | "devpts"
            | "mqueue"
            | "tracefs"
            | "debugfs"
            | "rpc_pipefs"
    ) {
        pseudo_fs_for_mount(
            core::str::from_utf8(source.as_bytes()).map_err(|_| AxError::IllegalBytes)?,
            normalized_fs,
            &data,
        )
    } else {
        Ok(None)
    }? {
        (fs, None, None)
    } else {
        if normalized_fs == "xfs" {
            let (fs, linux_device) = new_xfs_filesystem(
                source.as_ref(),
                &data,
                &security,
                mount_flags & MS_RDONLY != 0,
            )?;
            (fs, Some(linux_device), None)
        } else if normalized_fs == "btrfs" {
            let (fs, linux_device, members) = new_btrfs_filesystem(
                source.as_ref(),
                &data,
                &security,
                mount_flags & MS_RDONLY != 0,
            )?;
            (fs, Some(linux_device), Some(members))
        } else {
            let source_loc = current_fs_context()
                .lock()
                .resolve_security(&source, &security)?;
            let metadata = source_loc.metadata()?;
            if metadata.node_type != NodeType::BlockDevice {
                return Err(AxError::from(LinuxError::ENOTBLK));
            }

            if mounts::is_nodev(&source_loc)? {
                return Err(AxError::PermissionDenied);
            }

            let linux_device = metadata.rdev;
            let dev_name =
                block_device_name_for_rdev(linux_device)?.ok_or(AxError::NoSuchDevice)?;
            match open_block_device(&dev_name) {
                Ok(dev) => {
                    debug!("sys_mount: opening block device {dev_name}");
                    let read_only =
                        block_device_is_read_only(&dev_name).ok_or(AxError::NoSuchDevice)?;
                    if read_only && mount_flags & MS_RDONLY == 0 {
                        return Err(AxError::PermissionDenied);
                    }
                    let fs = if normalized_fs == "vfat" {
                        new_block_filesystem_with_fat_options(
                            normalized_fs,
                            dev,
                            parse_fat_mount_options(&data, credentials, umask)?,
                        )?
                    } else {
                        if !data.is_empty() {
                            return Err(AxError::OperationNotSupported);
                        }
                        new_block_filesystem(normalized_fs, dev)?
                    };
                    (fs, Some(linux_device), None)
                }
                Err(OpenBlockDeviceError::NotFound) => {
                    debug!("sys_mount: no such block device {dev_name}");
                    return Err(AxError::NoSuchDevice);
                }
                Err(OpenBlockDeviceError::Busy) => {
                    debug!("sys_mount: block device {dev_name} is already mounted");
                    return Err(AxError::ResourceBusy);
                }
            }
        }
    };

    let record_data = if normalized_fs == "cgroup" && data.is_empty() {
        match source.as_bytes() {
            b"none" | b"cgroup" => String::new(),
            _ => core::str::from_utf8(source.as_bytes())
                .map_err(|_| AxError::IllegalBytes)?
                .into(),
        }
    } else {
        data
    };
    let metadata = mounts::MountMetadata::new(
        source,
        try_string(normalized_fs)?,
        FsPathBuf::from_vec(b"/".to_vec()),
        record_data,
    )
    .with_block_members(block_members.unwrap_or_default());
    let mountpoint = mounts::new_detached_with_flags(&fs, mount_flags, metadata)?;
    if let Some(linux_device) = linux_device {
        mounts::register_linux_device(mountpoint.filesystem_identity(), linux_device)?;
    }
    mounts::attach_tree_and_record(&mountpoint, &target)?;
    reconcile_current_mount_topology()?;

    Ok(0)
}

pub fn sys_pivot_root<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    new_root: *const c_char,
    put_old: *const c_char,
) -> AxResult<isize> {
    // Linux checks namespace authority before either pathname copy.  Besides
    // matching the observable EPERM/EFAULT order, this avoids user-memory
    // access for callers that cannot mount in the first place.
    if !current_may_mount() {
        return Err(LinuxError::EPERM.into());
    }
    let new_root = load_user_path(memory, new_root)?;
    if new_root.as_bytes().is_empty() {
        return Err(AxError::NotFound);
    }

    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    let _mount_operation = mounts::namespace_operation();
    // Freeze fs_struct cloning/replacement before resolving the transaction
    // and retain it until every live old-root reference has moved.
    let _fs_context_publication = fs_context_publication();
    // Resolve in Linux order while the namespace topology is frozen.  The
    // security-aware walk applies DAC and registered inode security hooks to
    // both paths before the topology commit.
    let fs_context = current_fs_context();
    let fs = fs_context.lock();
    let old_root = fs.root_dir().clone();
    let new_root_loc = fs.resolve_security(new_root.as_ref(), &security)?;
    // LOOKUP_DIRECTORY belongs to the new-root walk. Linux returns ENOTDIR
    // here before it reads or resolves put_old.
    new_root_loc.check_is_dir()?;
    drop(fs);

    let put_old = load_user_path(memory, put_old)?;
    if put_old.as_bytes().is_empty() {
        return Err(AxError::NotFound);
    }
    debug!("sys_pivot_root <= new_root: {new_root:?}, put_old: {put_old:?}");
    let put_old_loc = fs_context
        .lock()
        .resolve_security(put_old.as_ref(), &security)?;
    put_old_loc.check_is_dir()?;
    // Keep every live task pinned before the irreversible topology commit.
    // The subsequent per-context updates are allocation-free, matching
    // chroot_fs_refs(): only root/cwd references exactly at the old root move.
    let tasks = try_tasks()?;
    mounts::pivot_root_and_records(&old_root, &new_root_loc, &put_old_loc)?;
    for task in tasks {
        if let Some(thread) = task.try_as_thread() {
            // A task may have completed exit after its weak registry entry was
            // snapshotted. The publication gate prevents retirement after a
            // successful acquisition; an already-retired exiting task has no
            // user-visible fs references left to update.
            if let Some(fs_context) = thread.try_fs_context() {
                fs_context.lock().pivot_root_refs(&old_root, &new_root_loc);
            }
        }
    }
    reconcile_current_mount_topology()?;
    Ok(0)
}

pub fn sys_umount2<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    target: *const c_char,
    flags: i32,
) -> AxResult<isize> {
    validate_umount_flags(flags).map_err(map_mount_uapi)?;
    let target = load_user_path(memory, target)?;
    debug!("sys_umount2 <= target: {target:?}, flags: {flags:#x}");
    let curr = current();
    let security = VfsSecurityContext::new(curr.as_thread().current_cred());
    if !may_mount(&security) {
        return Err(AxError::from(LinuxError::EPERM));
    }
    if flags & MNT_FORCE != 0 {
        return Err(AxError::OperationNotSupported);
    }

    let _mount_operation = mounts::namespace_operation();
    let target = if flags & UMOUNT_NOFOLLOW != 0 {
        current_fs_context()
            .lock()
            .resolve_no_follow_security_unobserved(target.as_ref(), &security)?
    } else {
        current_fs_context()
            .lock()
            .resolve_security_unobserved(target.as_ref(), &security)?
    };
    if !target.is_root_of_mount() {
        return Err(AxError::InvalidInput);
    }
    if target.is_root() {
        return Err(AxError::from(LinuxError::EBUSY));
    }
    mounts::unmount_and_remove_records(target, flags & MNT_DETACH != 0, flags & MNT_EXPIRE != 0)?;
    reconcile_current_mount_topology()?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use axfs_ng_vfs::{ExportHandleMode, Mountpoint, NodePermission, NodeType};

    use super::*;
    use crate::pseudofs::MemoryFs;

    #[test]
    fn bind_filesystem_forwards_export_handles_with_its_own_mount_identity() {
        let source_fs = MemoryFs::new().unwrap();
        let source_mount = Mountpoint::new_root(&source_fs);
        let source_root = source_mount.root_location();
        let scoped = source_root
            .create(
                "scoped",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o700),
            )
            .unwrap();
        let child = scoped
            .create(
                "child",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let outside = source_root
            .create(
                "outside",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();

        let bind_fs = bind_filesystem_for(&scoped, "tmpfs").unwrap();
        let bind_mount = Mountpoint::new_root(&bind_fs);
        let bind_root = bind_mount.root_location();
        let bind_child = bind_root.lookup_no_follow("child").unwrap();
        assert_ne!(bind_mount.mount_id(), source_mount.mount_id());

        let source_handle = source_mount
            .encode_export_handle(&child, ExportHandleMode::Openable)
            .unwrap();
        let bind_handle = bind_mount
            .encode_export_handle(&bind_child, ExportHandleMode::Openable)
            .unwrap();
        assert_eq!(bind_handle, source_handle);

        let decoded = bind_mount
            .decode_export_handle(
                bind_handle.handle_type,
                &bind_handle.bytes,
                ExportHandleDecodeMode::Any,
            )
            .unwrap();
        assert_eq!(decoded.mountpoint().mount_id(), bind_mount.mount_id());
        assert_eq!(decoded.inode(), child.inode());
        assert!(
            bind_mount
                .export_handle_is_descendant(&bind_root, &decoded)
                .unwrap()
        );

        let outside_handle = source_mount
            .encode_export_handle(&outside, ExportHandleMode::Openable)
            .unwrap();
        let decoded_outside = bind_mount
            .decode_export_handle(
                outside_handle.handle_type,
                &outside_handle.bytes,
                ExportHandleDecodeMode::Any,
            )
            .unwrap();
        assert!(
            !bind_mount
                .export_handle_is_descendant(&bind_root, &decoded_outside)
                .unwrap()
        );
    }

    #[test]
    fn fat_mount_defaults_follow_mounting_process_identity_and_umask() {
        let options = parse_fat_mount_options_with_defaults("", 1000, 100, 0o022).unwrap();
        assert_eq!(options.uid, 1000);
        assert_eq!(options.gid, 100);
        assert_eq!(options.file_mode.bits(), 0o755);
        assert_eq!(options.dir_mode.bits(), 0o755);
    }

    #[test]
    fn fat_mount_masks_are_parsed_independently() {
        let options =
            parse_fat_mount_options_with_defaults("uid=42,gid=7,fmask=0133,dmask=0027", 0, 0, 0)
                .unwrap();
        assert_eq!(options.uid, 42);
        assert_eq!(options.gid, 7);
        assert_eq!(options.file_mode.bits(), 0o644);
        assert_eq!(options.dir_mode.bits(), 0o750);
    }

    #[test]
    fn fat_mount_rejects_unimplemented_or_invalid_options() {
        assert_eq!(
            parse_fat_mount_options_with_defaults("iocharset=utf8", 0, 0, 0).unwrap_err(),
            AxError::OperationNotSupported
        );
        assert_eq!(
            parse_fat_mount_options_with_defaults("umask=0899", 0, 0, 0).unwrap_err(),
            AxError::InvalidInput
        );
    }

    #[test]
    fn mount_flag_validation_separates_invalid_and_unsupported_bits() {
        assert_eq!(
            validate_mount_flags(MS_KERNMOUNT as i32).unwrap_err(),
            UapiError::Invalid
        );
        assert_eq!(
            validate_mount_flags(MS_SYNCHRONOUS as i32).unwrap_err(),
            UapiError::Unsupported
        );
        assert_eq!(
            validate_mount_flags((MS_MGC_VAL | MS_RDONLY) as i32).unwrap(),
            MS_RDONLY
        );
    }

    #[test]
    fn mount_atime_normalization_defaults_preserves_and_prioritizes_strict() {
        assert_eq!(
            normalize_mount_atime(MS_NODEV, None) & MS_RELATIME,
            MS_RELATIME
        );
        assert_eq!(
            normalize_mount_atime(MS_REMOUNT | MS_NODEV, Some(MS_NOATIME)) & MS_ATIME_FLAGS,
            MS_NOATIME
        );
        let strict = normalize_mount_atime(MS_NOATIME | MS_RELATIME | MS_STRICTATIME, None);
        assert_ne!(strict & MS_STRICTATIME, 0);
        assert_eq!(strict & (MS_NOATIME | MS_RELATIME), 0);
    }
}
