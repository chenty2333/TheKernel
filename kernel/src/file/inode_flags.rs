//! One Linux file-attribute adapter shared by `file_{get,set}attr` and the
//! legacy inode-flag ioctls. Filesystems retain native state; this layer owns
//! VFS permission, capability, and security-hook rules.

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{FileAttr, Location, NodeType};
use linux_raw_sys::general::{
    CAP_FOWNER, CAP_LINUX_IMMUTABLE, STATX_ATTR_APPEND, STATX_ATTR_DAX, STATX_ATTR_IMMUTABLE,
    STATX_ATTR_MOUNT_ROOT, STATX_ATTR_NODUMP,
};
use linux_vfs::validate_file_setattr_xflags;
use thekernel_linux_cred::InodeFileAttrIntent;

use super::{
    IoctlContext,
    permission::{VfsSecurityContext, check_writable_mount},
};
use crate::{
    mm::map_usercopy_error,
    mounts,
    task::{
        ns_capable,
        security::{InodeSetattrCommittedSecurityRef, InodeSetattrProposal},
    },
};

pub(crate) const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
pub(crate) const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
pub(crate) const FS_IOC_FSGETXATTR: u32 = 0x801C_581F;
pub(crate) const FS_IOC_FSSETXATTR: u32 = 0x401C_5820;
const FS_IOC_ENABLE_VERITY: u32 = 0x4080_6685;

const FS_SYNC_FL: u64 = 0x0000_0008;
const FS_IMMUTABLE_FL: u64 = 0x0000_0010;
const FS_APPEND_FL: u64 = 0x0000_0020;
const FS_NODUMP_FL: u64 = 0x0000_0040;
const FS_NOATIME_FL: u64 = 0x0000_0080;
const FS_PROJINHERIT_FL: u64 = 0x2000_0000;
const FS_DAX_FL: u64 = 0x0200_0000;
const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;
const FS_XFLAG_APPEND: u64 = 0x0000_0010;
const FS_XFLAG_SYNC: u64 = 0x0000_0020;
const FS_XFLAG_NOATIME: u64 = 0x0000_0040;
const FS_XFLAG_NODUMP: u64 = 0x0000_0080;
const FS_XFLAG_PROJINHERIT: u64 = 0x0000_0200;
const FS_XFLAG_EXTSIZE: u64 = 0x0000_0800;
const FS_XFLAG_EXTSZINHERIT: u64 = 0x0000_1000;
const FS_XFLAG_DAX: u64 = 0x0000_8000;
const FS_XFLAG_COWEXTSIZE: u64 = 0x0001_0000;
const FS_XFLAG_RDONLY_MASK: u64 = 0x8000_0002;
pub(crate) const FS_XFLAGS_MASK: u32 = 0x8001_fffb;

/// Returns the active VFS attributes when the mounted filesystem publishes
/// them.  Attribute-less pseudo files retain their provider-defined behavior;
/// they cannot accidentally acquire an in-memory immutable bit.
fn active_attributes(location: &Location) -> AxResult<Option<FileAttr>> {
    match location.get_file_attr() {
        Ok(attr) => Ok(Some(attr)),
        Err(error)
            if matches!(
                LinuxError::from(error),
                LinuxError::ENOTTY | LinuxError::EOPNOTSUPP
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Gate every content-changing operation before it can reach a backend.
/// This is intentionally independent of the caller's credential: immutable
/// and append are inode state, not an advisory open-file flag.
pub(crate) fn check_content_mutable(location: &Location) -> AxResult<()> {
    if active_attributes(location)?.is_some_and(|attr| attr.xflags & FS_XFLAG_IMMUTABLE != 0) {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

/// Gate a metadata/extent mutation which is never an O_APPEND data write.
pub(crate) fn check_nonappend_content_mutable(location: &Location) -> AxResult<()> {
    if active_attributes(location)?
        .is_some_and(|attr| attr.xflags & (FS_XFLAG_IMMUTABLE | FS_XFLAG_APPEND) != 0)
    {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

/// Whether native FS_SYNC_FL requires an operation to wait for durable data
/// before reporting success.  Unsupported attribute providers have no such
/// inode flag and therefore return false.
pub(crate) fn sync_on_content_write(location: &Location) -> AxResult<bool> {
    Ok(active_attributes(location)?.is_some_and(|attr| attr.xflags & FS_XFLAG_SYNC != 0))
}

/// Per-inode FS_NOATIME_FL is stronger than the mount's relatime policy.
pub(crate) fn suppresses_atime(location: &Location) -> bool {
    active_attributes(location)
        .ok()
        .flatten()
        .is_some_and(|attr| attr.xflags & FS_XFLAG_NOATIME != 0)
}

/// Gate a size change.  Linux's append-only inode rule permits neither grow
/// nor shrink through truncate/fallocate; only an append write at EOF may
/// change its contents.
pub(crate) fn check_size_change(location: &Location, old_size: u64, new_size: u64) -> AxResult<()> {
    let Some(attr) = active_attributes(location)? else {
        return Ok(());
    };
    if attr.xflags & FS_XFLAG_IMMUTABLE != 0
        || (attr.xflags & FS_XFLAG_APPEND != 0 && old_size != new_size)
    {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

/// Gate a data write at a concrete byte offset.  Callers using O_APPEND pass
/// the offset selected under the inode append lock, so a stale pre-lock EOF
/// can never authorize an overwrite.
pub(crate) fn check_data_write(location: &Location, offset: u64, append: bool) -> AxResult<()> {
    let Some(attr) = active_attributes(location)? else {
        return Ok(());
    };
    if attr.xflags & FS_XFLAG_IMMUTABLE != 0 {
        return Err(LinuxError::EPERM.into());
    }
    if attr.xflags & FS_XFLAG_APPEND != 0 && (!append || offset != location.len()?) {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

/// Admission for opening an existing inode with write access.  This is kept
/// distinct from [`check_data_write`]: opening does not yet select an offset,
/// but Linux still rejects an immutable inode and requires O_APPEND for an
/// append-only inode before an OFD can acquire write access.
pub(crate) fn check_write_open(location: &Location, append: bool) -> AxResult<()> {
    let Some(attr) = active_attributes(location)? else {
        return Ok(());
    };
    if attr.xflags & FS_XFLAG_IMMUTABLE != 0 || (attr.xflags & FS_XFLAG_APPEND != 0 && !append) {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}

/// Applies the parent directory's native project-id inheritance immediately
/// after a successful create.  A directory inherits PROJINHERIT itself; other
/// children inherit only the project id.  The provider stores this as native
/// fileattr state (never a private xattr), which keeps quota and statx views
/// coherent.
pub(crate) fn inherit_project_id(parent: &Location, child: &Location) -> AxResult<()> {
    let Some(parent_attr) = active_attributes(parent)? else {
        return Ok(());
    };
    if parent_attr.xflags & FS_XFLAG_PROJINHERIT == 0 {
        return Ok(());
    }
    let Some(mut child_attr) = active_attributes(child)? else {
        return Err(LinuxError::EOPNOTSUPP.into());
    };
    child_attr.project_id = parent_attr.project_id;
    if child.is_dir() {
        child_attr.xflags |= FS_XFLAG_PROJINHERIT;
    }
    child.set_file_attr(child_attr).map_err(unsupported)
}

/// Snapshots project inheritance during namespace admission.  The returned
/// values are carried into the provider create transaction; callers must not
/// re-read or apply them after a dentry has become visible.
pub(crate) fn prepare_inherited_project_id(
    parent: &Location,
    child_is_dir: bool,
) -> AxResult<(Option<u32>, bool)> {
    let Some(parent_attr) = active_attributes(parent)? else {
        return Ok((None, false));
    };
    if parent_attr.xflags & FS_XFLAG_PROJINHERIT == 0 {
        return Ok((None, false));
    }
    Ok((Some(parent_attr.project_id), child_is_dir))
}

fn unsupported(error: AxError) -> AxError {
    match LinuxError::from(error) {
        LinuxError::ENOTTY | LinuxError::EOPNOTSUPP => LinuxError::EOPNOTSUPP.into(),
        _ => error,
    }
}

fn flags_to_xflags(flags: u64) -> u64 {
    let mut xflags = 0;
    for (flag, xflag) in [
        (FS_SYNC_FL, FS_XFLAG_SYNC),
        (FS_IMMUTABLE_FL, FS_XFLAG_IMMUTABLE),
        (FS_APPEND_FL, FS_XFLAG_APPEND),
        (FS_NODUMP_FL, FS_XFLAG_NODUMP),
        (FS_NOATIME_FL, FS_XFLAG_NOATIME),
        (FS_DAX_FL, FS_XFLAG_DAX),
        (FS_PROJINHERIT_FL, FS_XFLAG_PROJINHERIT),
    ] {
        if flags & flag != 0 {
            xflags |= xflag;
        }
    }
    xflags
}

pub(crate) fn idmapped_owner_or_capable(
    location: &Location,
    metadata: &axfs_ng_vfs::Metadata,
    security: &VfsSecurityContext,
    topology: &mounts::MountTopology,
) -> AxResult<bool> {
    let idmap = topology.idmap_for_mount(location.mountpoint().mount_id())?;
    Ok(owner_or_capable_with_idmap(
        metadata,
        security,
        idmap.as_deref(),
    ))
}

/// Implements `inode_owner_or_capable(mnt_idmap, inode)` against an exact
/// mount-idmap snapshot.  Open-file syscalls use this entry point so an fd
/// retains the mount view from open time even after the caller changes mount
/// namespaces.
pub(crate) fn owner_or_capable_with_idmap(
    metadata: &axfs_ng_vfs::Metadata,
    security: &VfsSecurityContext,
    idmap: Option<&mounts::MountIdmap>,
) -> bool {
    let Some(idmap) = idmap else {
        return security.credentials().uid().into_raw() == metadata.uid
            || ns_capable(
                security.actor(),
                security.filesystem_owner_user_ns(),
                CAP_FOWNER,
            );
    };
    // Metadata stores filesystem (outside) IDs.  A mounted idmap projects
    // those through the mount's immutable outside->inside rows before owner
    // comparison, and capabilities are meaningful only in that idmap's user
    // namespace.  An unmapped owner is deliberately not silently equal.
    let mapped_uid = idmap.uid.iter().find_map(|row| {
        let end = row.outside.checked_add(row.length)?;
        (metadata.uid >= row.outside && metadata.uid < end)
            .then_some(row.inside.checked_add(metadata.uid - row.outside))
            .flatten()
    });
    mapped_uid == Some(security.credentials().uid().into_raw())
        || ns_capable(security.actor(), idmap.user_namespace(), CAP_FOWNER)
}

pub(crate) fn get_file_attr(
    location: &Location,
    security: &VfsSecurityContext,
) -> AxResult<FileAttr> {
    let metadata = location.metadata()?;
    security.inode_file_getattr(location, &metadata)?;
    location.get_file_attr().map_err(unsupported)
}

fn get_legacy_flags(location: &Location, security: &VfsSecurityContext) -> AxResult<u32> {
    let metadata = location.metadata()?;
    security.inode_file_getattr(location, &metadata)?;
    location.get_legacy_file_flags().map_err(unsupported)
}

fn validate_semantics(
    old: FileAttr,
    mut requested: FileAttr,
    current_user_ns_is_initial: bool,
    metadata: &axfs_ng_vfs::Metadata,
    security: &VfsSecurityContext,
) -> AxResult<FileAttr> {
    validate_file_setattr_xflags(requested.xflags).map_err(|_| AxError::InvalidInput)?;
    requested.xflags &= !FS_XFLAG_RDONLY_MASK;
    if (old.xflags ^ requested.xflags) & (FS_XFLAG_APPEND | FS_XFLAG_IMMUTABLE) != 0
        && !ns_capable(
            security.actor(),
            security.filesystem_owner_user_ns(),
            CAP_LINUX_IMMUTABLE,
        )
    {
        return Err(LinuxError::EPERM.into());
    }
    if !current_user_ns_is_initial
        && (old.project_id != requested.project_id
            || (old.xflags ^ requested.xflags) & FS_XFLAG_PROJINHERIT != 0)
    {
        return Err(AxError::InvalidInput);
    }
    if requested.xflags & FS_XFLAG_EXTSIZE != 0 && metadata.node_type != NodeType::RegularFile {
        return Err(AxError::InvalidInput);
    }
    if requested.xflags & FS_XFLAG_EXTSZINHERIT != 0 && metadata.node_type != NodeType::Directory {
        return Err(AxError::InvalidInput);
    }
    if requested.xflags & FS_XFLAG_COWEXTSIZE != 0
        && !matches!(
            metadata.node_type,
            NodeType::RegularFile | NodeType::Directory
        )
    {
        return Err(AxError::InvalidInput);
    }
    if requested.xflags & FS_XFLAG_DAX != 0
        && !matches!(
            metadata.node_type,
            NodeType::RegularFile | NodeType::Directory
        )
    {
        return Err(AxError::InvalidInput);
    }
    if requested.extsize == 0 {
        requested.xflags &= !(FS_XFLAG_EXTSIZE | FS_XFLAG_EXTSZINHERIT);
    }
    if requested.cowextsize == 0 {
        requested.xflags &= !FS_XFLAG_COWEXTSIZE;
    }
    requested.nextents = old.nextents;
    Ok(requested)
}

/// Performs the common `vfs_fileattr_set()` sequence. Backend publication is
/// bracketed by the exact linear setattr admission and its mandatory post hook.
pub(crate) fn set_file_attr(
    location: &Location,
    requested: FileAttr,
    security: &VfsSecurityContext,
    current_user_ns_is_initial: bool,
    topology: &mounts::MountTopology,
) -> AxResult<()> {
    // Generic VFS has no inode-local writer API yet.  Use its established
    // metadata writer fallback over the complete snapshot/admission/publication
    // window so a concurrent chown or mount-idmap change cannot retarget it.
    let _metadata_writer = mounts::namespace_operation();
    check_writable_mount(location)?;
    let metadata = location.metadata()?;
    if !idmapped_owner_or_capable(location, &metadata, security, topology)? {
        return Err(LinuxError::EPERM.into());
    }
    security.inode_file_getattr(location, &metadata)?;
    let old = location.get_file_attr().map_err(unsupported)?;
    let requested = validate_semantics(
        old,
        requested,
        current_user_ns_is_initial,
        &metadata,
        security,
    )?;
    let admission = security.begin_inode_setattr(
        location,
        &metadata,
        InodeSetattrProposal::file_attr(InodeFileAttrIntent::new(
            requested.xflags,
            requested.extsize,
            requested.project_id,
            requested.cowextsize,
        )),
    )?;
    location.set_file_attr(requested).map_err(unsupported)?;
    admission.committed(InodeSetattrCommittedSecurityRef::new(location, &metadata));
    Ok(())
}

fn set_legacy_flags(
    location: &Location,
    flags: u32,
    security: &VfsSecurityContext,
    current_user_ns_is_initial: bool,
    topology: &mounts::MountTopology,
) -> AxResult<()> {
    let _metadata_writer = mounts::namespace_operation();
    check_writable_mount(location)?;
    let metadata = location.metadata()?;
    if !idmapped_owner_or_capable(location, &metadata, security, topology)? {
        return Err(LinuxError::EPERM.into());
    }
    security.inode_file_getattr(location, &metadata)?;
    let old_attr = location.get_file_attr().map_err(unsupported)?;
    let mut proposed_attr = old_attr;
    proposed_attr.xflags = (old_attr.xflags
        & !(FS_XFLAG_SYNC
            | FS_XFLAG_IMMUTABLE
            | FS_XFLAG_APPEND
            | FS_XFLAG_NODUMP
            | FS_XFLAG_NOATIME
            | FS_XFLAG_DAX
            | FS_XFLAG_PROJINHERIT))
        | flags_to_xflags(flags as u64);
    let proposed_attr = validate_semantics(
        old_attr,
        proposed_attr,
        current_user_ns_is_initial,
        &metadata,
        security,
    )?;
    let admission = security.begin_inode_setattr(
        location,
        &metadata,
        InodeSetattrProposal::file_attr(InodeFileAttrIntent::new(
            proposed_attr.xflags,
            proposed_attr.extsize,
            proposed_attr.project_id,
            proposed_attr.cowextsize,
        )),
    )?;
    location.set_legacy_file_flags(flags).map_err(unsupported)?;
    admission.committed(InodeSetattrCommittedSecurityRef::new(location, &metadata));
    Ok(())
}

pub fn same_inode(left: &Location, right: &Location) -> bool {
    left.mountpoint().device() == right.mountpoint().device() && left.inode() == right.inode()
}

pub fn statx_attributes(loc: &Location) -> AxResult<(u64, u64)> {
    let mut attributes = if loc.is_root_of_mount() {
        STATX_ATTR_MOUNT_ROOT as u64
    } else {
        0
    };
    let mut mask = STATX_ATTR_MOUNT_ROOT as u64;
    if let Some(attr) = active_attributes(loc)? {
        for (xflag, statx) in [
            (FS_XFLAG_IMMUTABLE, STATX_ATTR_IMMUTABLE as u64),
            (FS_XFLAG_APPEND, STATX_ATTR_APPEND as u64),
            (FS_XFLAG_NODUMP, STATX_ATTR_NODUMP as u64),
            (FS_XFLAG_DAX, STATX_ATTR_DAX as u64),
        ] {
            mask |= statx;
            if attr.xflags & xflag != 0 {
                attributes |= statx;
            }
        }
    }
    Ok((attributes, mask))
}

fn current_security(context: &IoctlContext) -> VfsSecurityContext {
    VfsSecurityContext::new(context.caller_cred().clone())
}
fn read_u32(context: &IoctlContext, arg: usize) -> AxResult<u32> {
    context
        .user_memory()
        .read_value(arg as *const u32)
        .map_err(map_usercopy_error)
}
fn write_u32(context: &IoctlContext, arg: usize, value: u32) -> AxResult<()> {
    context
        .user_memory()
        .write_bytes(arg, &value.to_ne_bytes())
        .map_err(map_usercopy_error)
}

fn read_fsxattr(context: &IoctlContext, arg: usize) -> AxResult<[u32; 5]> {
    let mut bytes = [core::mem::MaybeUninit::uninit(); 20];
    context
        .user_memory()
        .read_bytes(arg, &mut bytes)
        .map_err(map_usercopy_error)?;
    let bytes: [u8; 20] = unsafe { core::mem::transmute(bytes) };
    Ok(core::array::from_fn(|index| {
        u32::from_ne_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
    }))
}

fn write_fsxattr(context: &IoctlContext, arg: usize, attr: FileAttr) -> AxResult<()> {
    let words = [
        attr.xflags as u32,
        attr.extsize,
        attr.nextents,
        attr.project_id,
        attr.cowextsize,
    ];
    let mut bytes = [0u8; 20];
    for (index, word) in words.into_iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_ne_bytes());
    }
    context
        .user_memory()
        .write_bytes(arg, &bytes)
        .map_err(map_usercopy_error)
}

/// Every legacy file-attribute ioctl uses the same provider/security core as
/// v6.18 `file_getattr` / `file_setattr`.
pub fn ioctl(
    location: &Location,
    context: &IoctlContext,
    cmd: u32,
    arg: usize,
) -> Option<AxResult<usize>> {
    let security = current_security(context);
    let initial_ns = context.caller_cred().user_ns().is_initial();
    let topology = context.caller_process().mount_ns().topology();
    Some(match cmd {
        FS_IOC_GETFLAGS => get_legacy_flags(location, &security)
            .and_then(|flags| write_u32(context, arg, flags))
            .map(|()| 0),
        FS_IOC_SETFLAGS => (|| {
            let flags = read_u32(context, arg)?;
            set_legacy_flags(location, flags, &security, initial_ns, &topology)?;
            Ok(0)
        })(),
        FS_IOC_FSGETXATTR => get_file_attr(location, &security)
            .and_then(|attr| write_fsxattr(context, arg, attr))
            .map(|()| 0),
        FS_IOC_FSSETXATTR => (|| {
            let input = read_fsxattr(context, arg)?;
            if input[0] & !FS_XFLAGS_MASK != 0 {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            let requested = FileAttr {
                xflags: input[0] as u64,
                extsize: input[1],
                nextents: 0,
                project_id: input[3],
                cowextsize: input[4],
            };
            set_file_attr(location, requested, &security, initial_ns, &topology)?;
            Ok(0)
        })(),
        FS_IOC_ENABLE_VERITY => Err(LinuxError::EOPNOTSUPP.into()),
        _ => return None,
    })
}
