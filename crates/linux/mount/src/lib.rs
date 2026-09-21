//! Bounded mount namespace topology and transactional mutation plans.
//!
//! Only opaque identities and immutable snapshots are represented here; no
//! filesystem object, storage implementation, or lock is owned by this crate.
#![no_std]
#![forbid(unsafe_code)]
#![allow(missing_docs)]

use core::num::NonZeroU64;

/// Linux mount UAPI validation failures.  The syscall owner maps these to
/// Linux errno after user-memory copying and object lookup have completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UapiError {
    Invalid,
    Unsupported,
    TooBig,
    NotFound,
    /// A capability the syscall's own order requires before it looks at any
    /// user pointer or descriptor: `-EPERM`.
    Permission,
}

pub const FSOPEN_CLOEXEC: u32 = 0x0000_0001;
pub const FSCONFIG_SET_FLAG: u32 = 0;
pub const FSCONFIG_SET_STRING: u32 = 1;
pub const FSCONFIG_SET_BINARY: u32 = 2;
pub const FSCONFIG_SET_PATH: u32 = 3;
pub const FSCONFIG_SET_PATH_EMPTY: u32 = 4;
pub const FSCONFIG_SET_FD: u32 = 5;
pub const FSCONFIG_CMD_CREATE: u32 = 6;
pub const FSCONFIG_CMD_RECONFIGURE: u32 = 7;
pub const FSCONFIG_CMD_CREATE_EXCL: u32 = 8;
pub const FSMOUNT_CLOEXEC: u32 = 0x0000_0001;
/// `FSMOUNT_NAMESPACE` clones the new mount into a fresh mount namespace.
pub const FSMOUNT_NAMESPACE: u32 = 0x0000_0002;
pub const AT_EMPTY_PATH: u32 = 0x1000;
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
pub const AT_NO_AUTOMOUNT: u32 = 0x800;
pub const AT_RECURSIVE: u32 = 0x8000;
pub const OPEN_TREE_CLONE: u32 = 0x0000_0001;
/// `OPEN_TREE_NAMESPACE` clones the target tree into a new mount namespace.
pub const OPEN_TREE_NAMESPACE: u32 = 0x0000_0002;
pub const OPEN_TREE_CLOEXEC: u32 = 0x0008_0000;
pub const OPEN_TREE_MASK: u32 = OPEN_TREE_CLONE
    | OPEN_TREE_NAMESPACE
    | OPEN_TREE_CLOEXEC
    | AT_EMPTY_PATH
    | AT_NO_AUTOMOUNT
    | AT_RECURSIVE
    | AT_SYMLINK_NOFOLLOW;
pub const MOVE_MOUNT_F_SYMLINKS: u32 = 0x0000_0001;
pub const MOVE_MOUNT_F_AUTOMOUNTS: u32 = 0x0000_0002;
pub const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x0000_0004;
pub const MOVE_MOUNT_T_SYMLINKS: u32 = 0x0000_0010;
pub const MOVE_MOUNT_T_AUTOMOUNTS: u32 = 0x0000_0020;
pub const MOVE_MOUNT_T_EMPTY_PATH: u32 = 0x0000_0040;
pub const MOVE_MOUNT_SET_GROUP: u32 = 0x0000_0100;
pub const MOVE_MOUNT_BENEATH: u32 = 0x0000_0200;
pub const MOVE_MOUNT_MASK: u32 = 0x0000_0377;
pub const MOUNT_SETATTR_FLAGS: u32 =
    AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_RECURSIVE | AT_SYMLINK_NOFOLLOW;
pub const PAGE_SIZE: usize = 4096;

pub const MOUNT_ATTR_RDONLY: u32 = 0x0000_0001;
pub const MOUNT_ATTR_NOSUID: u32 = 0x0000_0002;
pub const MOUNT_ATTR_NODEV: u32 = 0x0000_0004;
pub const MOUNT_ATTR_NOEXEC: u32 = 0x0000_0008;
pub const MOUNT_ATTR_NOATIME: u32 = 0x0000_0010;
pub const MOUNT_ATTR_STRICTATIME: u32 = 0x0000_0020;
pub const MOUNT_ATTR_NODIRATIME: u32 = 0x0000_0080;
pub const MOUNT_ATTR_IDMAP: u32 = 0x0010_0000;
pub const MOUNT_ATTR_NOSYMFOLLOW: u32 = 0x0020_0000;
pub const MOUNT_ATTR_ATIME: u32 = 0x0000_0070;
pub const MOUNT_ATTR_SUPPORTED: u32 = MOUNT_ATTR_RDONLY
    | MOUNT_ATTR_NOSUID
    | MOUNT_ATTR_NODEV
    | MOUNT_ATTR_NOEXEC
    | MOUNT_ATTR_ATIME
    | MOUNT_ATTR_NODIRATIME
    | MOUNT_ATTR_IDMAP
    | MOUNT_ATTR_NOSYMFOLLOW;

pub const MS_RDONLY: u32 = 0x1;
pub const MS_NOSUID: u32 = 0x2;
pub const MS_NODEV: u32 = 0x4;
pub const MS_NOEXEC: u32 = 0x8;
pub const MS_SYNCHRONOUS: u32 = 0x10;
pub const MS_REMOUNT: u32 = 0x20;
pub const MS_MANDLOCK: u32 = 0x40;
pub const MS_DIRSYNC: u32 = 0x80;
pub const MS_NOSYMFOLLOW: u32 = 0x100;
pub const MS_NOATIME: u32 = 0x400;
pub const MS_NODIRATIME: u32 = 0x800;
pub const MS_BIND: u32 = 0x1000;
pub const MS_MOVE: u32 = 0x2000;
pub const MS_REC: u32 = 0x4000;
pub const MS_SILENT: u32 = 0x8000;
pub const MS_UNBINDABLE: u32 = 1 << 17;
pub const MS_PRIVATE: u32 = 1 << 18;
pub const MS_SLAVE: u32 = 1 << 19;
pub const MS_SHARED: u32 = 1 << 20;
pub const MS_RELATIME: u32 = 0x20_0000;
pub const MS_KERNMOUNT: u32 = 1 << 22;
pub const MS_I_VERSION: u32 = 1 << 23;
pub const MS_STRICTATIME: u32 = 0x100_0000;
pub const MS_LAZYTIME: u32 = 1 << 25;
pub const MS_INTERNAL_FLAGS: u32 = 0xfc00_0000;
pub const MS_MGC_VAL: u32 = 0xc0ed_0000;
pub const MS_MGC_MSK: u32 = 0xffff_0000;
pub const MS_PROPAGATION_FLAGS: u32 = MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED;
pub const MS_ATIME_FLAGS: u32 = MS_NOATIME | MS_NODIRATIME | MS_RELATIME | MS_STRICTATIME;
pub const MS_SUPPORTED_FLAGS: u32 = MS_RDONLY
    | MS_NOSUID
    | MS_NODEV
    | MS_NOEXEC
    | MS_REMOUNT
    | MS_MANDLOCK
    | MS_NOSYMFOLLOW
    | MS_NOATIME
    | MS_NODIRATIME
    | MS_BIND
    | MS_MOVE
    | MS_REC
    | MS_SILENT
    | MS_PROPAGATION_FLAGS
    | MS_RELATIME
    | MS_STRICTATIME
    | MS_SUPERBLOCK_FLAGS;
/// `MS_NOUSER` — the only legacy flag `path_mount()` rejects outright.
pub const MS_NOUSER: u32 = 1 << 31;
/// Superblock-scoped legacy flags.  `path_mount()` folds these into
/// `sb->s_flags` through
/// `sb_flags = flags & (SB_RDONLY | SB_SYNCHRONOUS | SB_MANDLOCK | SB_DIRSYNC |
///                      SB_SILENT | SB_POSIXACL | SB_LAZYTIME | SB_I_VERSION)`;
/// they are accepted, not refused.  (`1 << 16` is `MS_POSIXACL`.)
pub const MS_SUPERBLOCK_FLAGS: u32 =
    MS_SYNCHRONOUS | MS_DIRSYNC | (1 << 16) | MS_I_VERSION | MS_LAZYTIME;
pub const MS_INHERITED_BIND_FLAGS: u32 = MS_RDONLY
    | MS_NOSUID
    | MS_NODEV
    | MS_NOEXEC
    | MS_NOSYMFOLLOW
    | MS_NOATIME
    | MS_NODIRATIME
    | MS_RELATIME
    | MS_STRICTATIME;
pub const MS_BIND_REMOUNT_FLAGS: u32 = MS_INHERITED_BIND_FLAGS;
pub const MNT_FORCE: i32 = 0x1;
pub const MNT_DETACH: i32 = 0x2;
pub const MNT_EXPIRE: i32 = 0x4;
pub const UMOUNT_NOFOLLOW: i32 = 0x8;
pub const UMOUNT_FLAGS_VALID: i32 = MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW;

pub const MNT_ID_REQ_SIZE_VER0: usize = 24;
pub const MNT_ID_REQ_SIZE_VER1: usize = 32;
pub const LISTMOUNT_REVERSE: u32 = 1;
pub const LSMT_ROOT: u64 = u64::MAX;

pub const FSPICK_CLOEXEC: u32 = 0x0000_0001;
pub const FSPICK_SYMLINK_NOFOLLOW: u32 = 0x0000_0002;
pub const FSPICK_NO_AUTOMOUNT: u32 = 0x0000_0004;
pub const FSPICK_EMPTY_PATH: u32 = 0x0000_0008;
pub const FSPICK_MASK: u32 =
    FSPICK_CLOEXEC | FSPICK_SYMLINK_NOFOLLOW | FSPICK_NO_AUTOMOUNT | FSPICK_EMPTY_PATH;
pub const STATMOUNT_SB_BASIC: u64 = 0x001;
pub const STATMOUNT_MNT_BASIC: u64 = 0x002;
pub const STATMOUNT_PROPAGATE_FROM: u64 = 0x004;
pub const STATMOUNT_MNT_ROOT: u64 = 0x008;
pub const STATMOUNT_MNT_POINT: u64 = 0x010;
pub const STATMOUNT_FS_TYPE: u64 = 0x020;
pub const STATMOUNT_MNT_NS_ID: u64 = 0x040;
pub const STATMOUNT_MNT_OPTS: u64 = 0x080;
pub const STATMOUNT_FS_SUBTYPE: u64 = 0x100;
pub const STATMOUNT_SB_SOURCE: u64 = 0x200;
pub const STATMOUNT_OPT_ARRAY: u64 = 0x400;
pub const STATMOUNT_OPT_SEC_ARRAY: u64 = 0x800;
pub const STATMOUNT_SUPPORTED_MASK: u64 = 0x1000;
pub const STATMOUNT_MNT_UIDMAP: u64 = 0x2000;
pub const STATMOUNT_MNT_GIDMAP: u64 = 0x4000;
pub const STATMOUNT_SUPPORTED: u64 = STATMOUNT_SB_BASIC
    | STATMOUNT_MNT_BASIC
    | STATMOUNT_PROPAGATE_FROM
    | STATMOUNT_MNT_ROOT
    | STATMOUNT_MNT_POINT
    | STATMOUNT_FS_TYPE
    | STATMOUNT_MNT_NS_ID
    | STATMOUNT_MNT_OPTS
    | STATMOUNT_FS_SUBTYPE
    | STATMOUNT_SB_SOURCE
    | STATMOUNT_OPT_ARRAY
    | STATMOUNT_OPT_SEC_ARRAY
    | STATMOUNT_SUPPORTED_MASK
    | STATMOUNT_MNT_UIDMAP
    | STATMOUNT_MNT_GIDMAP;

/// `statmount(2)` accepts only this flag bit; `mnt_id_req.mnt_fd` then names the
/// mount to describe instead of `mnt_id`.
pub const STATMOUNT_BY_FD: u32 = 0x0000_0001;

pub const fn validate_statmount_flags(flags: u32) -> Result<bool, UapiError> {
    if flags & !STATMOUNT_BY_FD == 0 {
        Ok(flags & STATMOUNT_BY_FD != 0)
    } else {
        Err(UapiError::Invalid)
    }
}
pub const fn validate_listmount_flags(flags: u32) -> Result<bool, UapiError> {
    if flags & !LISTMOUNT_REVERSE == 0 {
        Ok(flags & LISTMOUNT_REVERSE != 0)
    } else {
        Err(UapiError::Invalid)
    }
}
pub const fn validate_mnt_id_request(request: MntIdReq) -> Result<(), UapiError> {
    if request.mnt_ns_fd != 0 && request.ns_id != 0 {
        Err(UapiError::Invalid)
    } else if request.mnt_id <= (1u64 << 31) {
        Err(UapiError::Invalid)
    } else {
        Ok(())
    }
}
/// `copy_mnt_id_req()` in `fs/namespace.c` validates the request object
/// differently depending on `STATMOUNT_BY_FD`: the descriptor form forbids the
/// mount-id fields entirely, while the id form forbids mixing `mnt_ns_fd` with
/// `mnt_ns_id` and requires a unique mount id above `MNT_UNIQUE_ID_OFFSET`.
pub const fn validate_statmount_request(request: MntIdReq, by_fd: bool) -> Result<(), UapiError> {
    if by_fd {
        if request.mnt_id != 0 || request.ns_id != 0 {
            Err(UapiError::Invalid)
        } else {
            Ok(())
        }
    } else {
        validate_mnt_id_request(request)
    }
}
pub const fn validate_unique_mount_id(mount_id: u64) -> Result<(), UapiError> {
    if mount_id <= (1u64 << 31) {
        Err(UapiError::Invalid)
    } else {
        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct StatmountPrefix {
    pub size: u32,
    pub mnt_opts: u32,
    pub mask: u64,
    pub sb_dev_major: u32,
    pub sb_dev_minor: u32,
    pub sb_magic: u64,
    pub sb_flags: u32,
    pub fs_type: u32,
    pub mnt_id: u64,
    pub mnt_parent_id: u64,
    pub mnt_id_old: u32,
    pub mnt_parent_id_old: u32,
    pub mnt_attr: u64,
    pub mnt_propagation: u64,
    pub mnt_peer_group: u64,
    pub mnt_master: u64,
    pub propagate_from: u64,
    pub mnt_root: u32,
    pub mnt_point: u32,
    pub mnt_ns_id: u64,
    pub fs_subtype: u32,
    pub sb_source: u32,
    pub opt_num: u32,
    pub opt_array: u32,
    pub opt_sec_num: u32,
    pub opt_sec_array: u32,
    pub supported_mask: u64,
    pub mnt_uidmap_num: u32,
    pub mnt_uidmap: u32,
    pub mnt_gidmap_num: u32,
    pub mnt_gidmap: u32,
    pub spare: [u64; 43],
}
pub const STATMOUNT_PREFIX_SIZE: usize = core::mem::size_of::<StatmountPrefix>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MntIdReq {
    pub mnt_ns_fd: u32,
    pub mnt_id: u64,
    pub param: u64,
    pub ns_id: u64,
}
impl MntIdReq {
    /// The 32-bit word after `size` is a union in Linux: `mnt_ns_fd` for the
    /// identifier form of `statmount`/`listmount` and `mnt_fd` for the
    /// `STATMOUNT_BY_FD` form of `statmount`.
    pub const fn descriptor_word(self) -> u32 {
        self.mnt_ns_fd
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, UapiError> {
        if bytes.len() < MNT_ID_REQ_SIZE_VER0 {
            return Err(UapiError::Invalid);
        }
        let word = |start| u64::from_ne_bytes(bytes[start..start + 8].try_into().unwrap());
        Ok(Self {
            mnt_ns_fd: u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            mnt_id: word(8),
            param: word(16),
            ns_id: if bytes.len() >= MNT_ID_REQ_SIZE_VER1 {
                word(24)
            } else {
                0
            },
        })
    }
}

/// `fspick(2)`'s two pre-lookup rejections. Linux tests `may_mount()` first
/// and only then the flag word, so a caller that cannot mount observes `EPERM`
/// even for a malformed flag set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FspickReject {
    /// `!may_mount()`: `-EPERM`.
    NotCapable,
    /// A bit outside `FSPICK_VALID_FLAGS`: `-EINVAL`.
    InvalidFlags,
}
pub const fn validate_fspick(flags: u32, may_mount: bool) -> Result<bool, FspickReject> {
    if !may_mount {
        Err(FspickReject::NotCapable)
    } else if flags & !FSPICK_MASK != 0 {
        Err(FspickReject::InvalidFlags)
    } else {
        Ok(flags & FSPICK_CLOEXEC != 0)
    }
}
pub const fn validate_fsopen_flags(flags: u32) -> Result<bool, UapiError> {
    if flags & !FSOPEN_CLOEXEC != 0 {
        Err(UapiError::Invalid)
    } else {
        Ok(flags & FSOPEN_CLOEXEC != 0)
    }
}
pub const fn validate_fsconfig_shape(
    command: u32,
    key_present: bool,
    value_present: bool,
    aux: i32,
    at_fdcwd: i32,
) -> Result<(), UapiError> {
    let valid = match command {
        FSCONFIG_SET_FLAG => key_present && !value_present && aux == 0,
        FSCONFIG_SET_STRING => key_present && value_present && aux == 0,
        FSCONFIG_SET_BINARY => key_present && value_present && aux > 0 && aux <= 1024 * 1024,
        FSCONFIG_SET_PATH | FSCONFIG_SET_PATH_EMPTY => {
            key_present && value_present && (aux == at_fdcwd || aux >= 0)
        }
        FSCONFIG_SET_FD => key_present && !value_present && aux >= 0,
        FSCONFIG_CMD_CREATE | FSCONFIG_CMD_CREATE_EXCL | FSCONFIG_CMD_RECONFIGURE => {
            !key_present && !value_present && aux == 0
        }
        _ => return Err(UapiError::Unsupported),
    };
    if valid {
        Ok(())
    } else {
        Err(UapiError::Invalid)
    }
}
/// The flag-word half of `SYSCALL_DEFINE3(fsmount, ...)`, which is the first
/// statement of the syscall (`fs/namespace.c` v7.2.3:4447-4448):
///
/// ```text
///     if (flags & ~(FSMOUNT_CLOEXEC | FSMOUNT_NAMESPACE))
///         return -EINVAL;
/// ```
pub const fn validate_fsmount_flags(flags: u32) -> Result<bool, UapiError> {
    if flags & !(FSMOUNT_CLOEXEC | FSMOUNT_NAMESPACE) != 0 {
        Err(UapiError::Invalid)
    } else {
        Ok(flags & FSMOUNT_CLOEXEC != 0)
    }
}
/// The attribute-word half of `fsmount(2)`, which Linux applies *after* the
/// namespace capability decision (`fs/namespace.c`:4458-4471):
///
/// Excerpt: Linux v7.2.3 `fs/namespace.c:4456-4469` — GPL-2.0-only, (C) The Linux Kernel Authors
/// ```text
///     if (attr_flags & ~FSMOUNT_VALID_FLAGS)
///         return -EINVAL;
///
///     switch (attr_flags & MOUNT_ATTR__ATIME) {
///     case MOUNT_ATTR_RELATIME:
///     case MOUNT_ATTR_NOATIME:
///     case MOUNT_ATTR_STRICTATIME:
///         break;
///     default:
///         return -EINVAL;
///     }
/// ```
///
/// `FSMOUNT_VALID_FLAGS` (`:4401-4404`) is `MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID
/// | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC | MOUNT_ATTR__ATIME |
/// MOUNT_ATTR_NODIRATIME | MOUNT_ATTR_NOSYMFOLLOW`, so `MOUNT_ATTR_IDMAP` is
/// rejected here even though `mount_setattr(2)` accepts it through the wider
/// `MOUNT_SETATTR_VALID_FLAGS` (`:4406`).
pub const fn validate_fsmount_attrs(attrs: u32) -> Result<(), UapiError> {
    if attrs & !(MOUNT_ATTR_SUPPORTED & !MOUNT_ATTR_IDMAP) != 0 || !valid_atime_set(attrs) {
        Err(UapiError::Invalid)
    } else {
        Ok(())
    }
}
/// The ordered scalar admission of `SYSCALL_DEFINE3(fsmount, ...)`, which runs
/// entirely before the syscall looks at the fs_context descriptor
/// (`fs/namespace.c` v7.2.3:4435-4471):
///
/// Excerpt: Linux v7.2.3 `fs/namespace.c:4446-4461` — GPL-2.0-only, (C) The Linux Kernel Authors
/// ```text
///     if (flags & ~(FSMOUNT_CLOEXEC | FSMOUNT_NAMESPACE))
///         return -EINVAL;
///     if ((flags & FSMOUNT_NAMESPACE) &&
///         !ns_capable(current_user_ns(), CAP_SYS_ADMIN))
///         return -EPERM;
///     if (!(flags & FSMOUNT_NAMESPACE) && !may_mount())
///         return -EPERM;
///     if (attr_flags & ~FSMOUNT_VALID_FLAGS)
///         return -EINVAL;
///     switch (attr_flags & MOUNT_ATTR__ATIME) { ... default: return -EINVAL; }
/// ```
///
/// The two capability tests are different domains and cannot be collapsed:
/// `FSMOUNT_NAMESPACE` needs `CAP_SYS_ADMIN` in the caller's *own* user
/// namespace (`current_user_namespace_admin`), because the new namespace is
/// owned by it, while the plain form needs `may_mount()`, which is
/// `ns_capable(current->nsproxy->mnt_ns->user_ns, CAP_SYS_ADMIN)`
/// (`fs/namespace.c`:2007-2010) — `mount_ns_owner_admin` here.  The capability
/// test therefore precedes attribute validation, so an unprivileged caller
/// with a malformed attribute word gets `EPERM`, not `EINVAL`.
pub const fn admit_fsmount(
    flags: u32,
    attrs: u32,
    current_user_namespace_admin: bool,
    mount_ns_owner_admin: bool,
) -> Result<bool, UapiError> {
    let cloexec = match validate_fsmount_flags(flags) {
        Ok(cloexec) => cloexec,
        Err(error) => return Err(error),
    };
    if flags & FSMOUNT_NAMESPACE != 0 {
        if !current_user_namespace_admin {
            return Err(UapiError::Permission);
        }
    } else if !mount_ns_owner_admin {
        return Err(UapiError::Permission);
    }
    match validate_fsmount_attrs(attrs) {
        Ok(()) => Ok(cloexec),
        Err(error) => Err(error),
    }
}
/// `vfs_open_tree()` in `fs/namespace.c` rejects, in this order, any bit
/// outside `AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_RECURSIVE |
/// AT_SYMLINK_NOFOLLOW | OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC |
/// OPEN_TREE_NAMESPACE`, `AT_RECURSIVE` without either clone flag, and both
/// clone flags together.
pub const fn validate_open_tree(flags: u32) -> Result<bool, UapiError> {
    if flags & !OPEN_TREE_MASK != 0
        || flags & (AT_RECURSIVE | OPEN_TREE_CLONE | OPEN_TREE_NAMESPACE) == AT_RECURSIVE
        || (flags & (OPEN_TREE_CLONE | OPEN_TREE_NAMESPACE)).count_ones() > 1
    {
        Err(UapiError::Invalid)
    } else {
        Ok(flags & OPEN_TREE_CLOEXEC != 0)
    }
}
/// The capability half of `vfs_open_tree()`, which Linux applies after the
/// flag-word rules and before the pathname copy (`fs/namespace.c`
/// v7.2.3:3220-3233):
///
/// Excerpt: Linux v7.2.3 `fs/namespace.c:3220-3229` — GPL-2.0-only, (C) The Linux Kernel Authors
/// ```text
///     /*
///      * If we create a new mount namespace with the cloned mount tree we
///      * just care about being privileged over our current user namespace.
///      * The new mount namespace will be owned by it.
///      */
///     if ((flags & OPEN_TREE_NAMESPACE) &&
///         !ns_capable(current_user_ns(), CAP_SYS_ADMIN))
///         return ERR_PTR(-EPERM);
///
///     if ((flags & OPEN_TREE_CLONE) && !may_mount())
///         return ERR_PTR(-EPERM);
/// ```
///
/// `OPEN_TREE_NAMESPACE` deliberately does *not* consult `may_mount()`: a
/// caller that holds `CAP_SYS_ADMIN` in its own user namespace may create a
/// namespace owned by that user namespace even while `may_mount()` is false
/// for the mount namespace it currently lives in.
pub const fn admit_open_tree(
    flags: u32,
    current_user_namespace_admin: bool,
    mount_ns_owner_admin: bool,
) -> Result<bool, UapiError> {
    let cloexec = match validate_open_tree(flags) {
        Ok(cloexec) => cloexec,
        Err(error) => return Err(error),
    };
    if flags & OPEN_TREE_NAMESPACE != 0 && !current_user_namespace_admin {
        return Err(UapiError::Permission);
    }
    if flags & OPEN_TREE_CLONE != 0 && !mount_ns_owner_admin {
        return Err(UapiError::Permission);
    }
    Ok(cloexec)
}
pub const fn validate_move_mount(
    flags: u32,
    from_empty: bool,
    to_empty: bool,
) -> Result<(), UapiError> {
    if flags & !MOVE_MOUNT_MASK != 0 {
        Err(UapiError::Invalid)
    } else if flags & (MOVE_MOUNT_SET_GROUP | MOVE_MOUNT_BENEATH)
        == (MOVE_MOUNT_SET_GROUP | MOVE_MOUNT_BENEATH)
    {
        Err(UapiError::Invalid)
    } else if flags & MOVE_MOUNT_F_EMPTY_PATH == 0 && from_empty
        || to_empty && flags & MOVE_MOUNT_T_EMPTY_PATH == 0
    {
        Err(UapiError::NotFound)
    } else {
        Ok(())
    }
}
pub const fn validate_mount_setattr_flags(flags: u32, size: usize) -> Result<(), UapiError> {
    if flags & !MOUNT_SETATTR_FLAGS != 0 || size < 32 {
        Err(UapiError::Invalid)
    } else if size > PAGE_SIZE {
        Err(UapiError::TooBig)
    } else {
        Ok(())
    }
}
/// `ksys_umount()` (fs/namespace.c) performs the whole flag-validity check
/// before it even loads the pathname:
///
/// Excerpt: Linux v7.2.3 `fs/namespace.c:2061-2063` — GPL-2.0-only, (C) The Linux Kernel Authors
/// ```text
///     // basic validity checks done first
///     if (flags & ~(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW))
///         return -EINVAL;
/// ```
///
/// The `MNT_EXPIRE` combination rule lives in `do_umount()`, which only runs
/// after the lookup and permission stages, so it is *not* part of this test.
pub const fn validate_umount_flags(flags: i32) -> Result<(), UapiError> {
    if flags & !UMOUNT_FLAGS_VALID != 0 {
        Err(UapiError::Invalid)
    } else {
        Ok(())
    }
}
/// `path_mount()` (fs/namespace.c) validates the legacy flag word with exactly
/// one test:
///
/// Excerpt: Linux v7.2.3 `fs/namespace.c:4096-4101` — GPL-2.0-only, (C) The Linux Kernel Authors
/// ```text
///     /* Basic sanity checks */
///     if (data_page)
///         ((char *)data_page)[PAGE_SIZE - 1] = 0;
///
///     if (flags & MS_NOUSER)
///         return -EINVAL;
/// ```
///
/// Nothing else is refused: `MS_SYNCHRONOUS`, `MS_DIRSYNC`, `MS_POSIXACL`,
/// `MS_I_VERSION` and `MS_LAZYTIME` are all accepted and become superblock
/// flags, so the legacy entry point must not invent an `EOPNOTSUPP` for them.
pub const fn validate_mount_flags(raw: i32) -> Result<u32, UapiError> {
    let mut flags = raw as u32;
    if flags & MS_MGC_MSK == MS_MGC_VAL {
        flags &= !MS_MGC_MSK;
    }
    if flags & MS_NOUSER != 0 {
        Err(UapiError::Invalid)
    } else {
        Ok(flags)
    }
}
pub const fn valid_atime_set(set: u32) -> bool {
    matches!(
        set & MOUNT_ATTR_ATIME,
        0 | MOUNT_ATTR_NOATIME | MOUNT_ATTR_STRICTATIME
    )
}
pub const fn mount_attr_to_mount_flags(attrs: u32) -> u32 {
    let mut flags = 0;
    if attrs & MOUNT_ATTR_RDONLY != 0 {
        flags |= MS_RDONLY;
    }
    if attrs & MOUNT_ATTR_NOSUID != 0 {
        flags |= MS_NOSUID;
    }
    if attrs & MOUNT_ATTR_NODEV != 0 {
        flags |= MS_NODEV;
    }
    if attrs & MOUNT_ATTR_NOEXEC != 0 {
        flags |= MS_NOEXEC;
    }
    if attrs & MOUNT_ATTR_NOATIME != 0 {
        flags |= MS_NOATIME;
    }
    if attrs & MOUNT_ATTR_STRICTATIME != 0 {
        flags |= MS_STRICTATIME;
    }
    if attrs & MOUNT_ATTR_NODIRATIME != 0 {
        flags |= MS_NODIRATIME;
    }
    if attrs & MOUNT_ATTR_NOSYMFOLLOW != 0 {
        flags |= MS_NOSYMFOLLOW;
    }
    flags
}
pub const fn apply_mount_attr_flags(
    current: u32,
    set: u64,
    clear: u64,
    propagation: u64,
    userns_fd: u64,
) -> Result<u32, UapiError> {
    if (set | clear) & !(MOUNT_ATTR_SUPPORTED as u64) != 0
        || (set | clear) & MOUNT_ATTR_IDMAP as u64 != 0
        || propagation & !(MS_PROPAGATION_FLAGS as u64) != 0
        || propagation.count_ones() > 1
        || userns_fd != 0
    {
        return Err(UapiError::Invalid);
    }
    if propagation != 0 {
        return Err(UapiError::Unsupported);
    }
    let set = set as u32;
    let clear = clear as u32;
    if !valid_atime_set(set)
        || clear & MOUNT_ATTR_ATIME != 0 && clear & MOUNT_ATTR_ATIME != MOUNT_ATTR_ATIME
        || clear & MOUNT_ATTR_ATIME == 0 && set & MOUNT_ATTR_ATIME != 0
    {
        return Err(UapiError::Invalid);
    }
    let mut next = current;
    if clear & MOUNT_ATTR_ATIME != 0 {
        next &= !(MS_NOATIME | MS_RELATIME | MS_STRICTATIME);
    }
    next &= !mount_attr_to_mount_flags(clear & !MOUNT_ATTR_ATIME);
    next |= mount_attr_to_mount_flags(set);
    if clear & MOUNT_ATTR_ATIME != 0 && set & MOUNT_ATTR_ATIME == 0 {
        next |= MS_RELATIME;
    }
    Ok(next)
}
pub const fn normalize_mount_atime(mut requested: u32, current: Option<u32>) -> u32 {
    if requested & MS_REMOUNT != 0 && requested & MS_ATIME_FLAGS == 0 {
        if let Some(current) = current {
            requested |= current & MS_ATIME_FLAGS;
            return requested;
        }
    }
    if requested & MS_NOATIME != 0 {
        requested &= !MS_RELATIME;
    } else {
        requested |= MS_RELATIME;
    }
    if requested & MS_STRICTATIME != 0 {
        requested &= !(MS_NOATIME | MS_RELATIME);
    }
    requested
}
pub const fn statmount_attr(flags: u32) -> u64 {
    let mut attrs = 0;
    if flags & MS_RDONLY != 0 {
        attrs |= MOUNT_ATTR_RDONLY as u64;
    }
    if flags & MS_NOSUID != 0 {
        attrs |= MOUNT_ATTR_NOSUID as u64;
    }
    if flags & MS_NODEV != 0 {
        attrs |= MOUNT_ATTR_NODEV as u64;
    }
    if flags & MS_NOEXEC != 0 {
        attrs |= MOUNT_ATTR_NOEXEC as u64;
    }
    if flags & MS_NOATIME != 0 {
        attrs |= MOUNT_ATTR_NOATIME as u64;
    }
    if flags & MS_STRICTATIME != 0 {
        attrs |= MOUNT_ATTR_STRICTATIME as u64;
    }
    if flags & MS_NODIRATIME != 0 {
        attrs |= MOUNT_ATTR_NODIRATIME as u64;
    }
    if flags & MS_NOSYMFOLLOW != 0 {
        attrs |= MOUNT_ATTR_NOSYMFOLLOW as u64;
    }
    attrs
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountError {
    ZeroIdentity,
    UnsupportedFlags,
    PermissionDenied,
    InvalidTopology,
    TopologyCycle,
    RootRequired,
    GenerationExhausted,
    Busy,
}

macro_rules! opaque {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);
        impl $name {
            pub const fn new(raw: u64) -> Result<Self, MountError> {
                match NonZeroU64::new(raw) {
                    Some(value) => Ok(Self(value)),
                    None => Err(MountError::ZeroIdentity),
                }
            }
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}
opaque!(NamespaceId);
opaque!(MountId);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NamespaceGeneration(NonZeroU64);
impl NamespaceGeneration {
    pub const fn initial() -> Self {
        Self(NonZeroU64::MIN)
    }
    /// Reconstructs a nonzero generation retained by the namespace owner.
    pub const fn from_raw(raw: u64) -> Result<Self, MountError> {
        match NonZeroU64::new(raw) {
            Some(value) => Ok(Self(value)),
            None => Err(MountError::GenerationExhausted),
        }
    }
    /// Returns the namespace owner's opaque generation value.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
    pub const fn next(self) -> Result<Self, MountError> {
        match self.0.get().checked_add(1) {
            Some(value) => match NonZeroU64::new(value) {
                Some(value) => Ok(Self(value)),
                None => Err(MountError::GenerationExhausted),
            },
            None => Err(MountError::GenerationExhausted),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MountFlags(u64);
impl MountFlags {
    pub const RDONLY: Self = Self(1);
    pub const NOSUID: Self = Self(2);
    pub const NODEV: Self = Self(4);
    pub const NOEXEC: Self = Self(8);
    pub const NOATIME: Self = Self(1024);
    pub const SUPPORTED: Self = Self(1039);
    pub const fn from_bits(bits: u64) -> Result<Self, MountError> {
        if bits & !Self::SUPPORTED.0 == 0 {
            Ok(Self(bits))
        } else {
            Err(MountError::UnsupportedFlags)
        }
    }
    /// Wraps flags that were already validated by the syscall-specific UAPI
    /// parser.  This preserves topology planning for Linux flags outside the
    /// common portable subset.
    #[must_use]
    pub const fn from_validated_kernel_bits(bits: u64) -> Self {
        Self(bits)
    }
    pub const fn bits(self) -> u64 {
        self.0
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MountAuthority {
    pub administer: bool,
    pub pivot_root: bool,
    pub lazy_unmount: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopologyEntry {
    pub mount: MountId,
    pub parent: Option<MountId>,
    pub flags: MountFlags,
    pub detachable: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopologySnapshot<'a> {
    pub namespace: NamespaceId,
    pub generation: NamespaceGeneration,
    pub entries: &'a [TopologyEntry],
}
impl<'a> TopologySnapshot<'a> {
    pub fn find(&self, id: MountId) -> Option<TopologyEntry> {
        self.entries.iter().copied().find(|entry| entry.mount == id)
    }
    pub fn validate(&self) -> Result<(), MountError> {
        let mut roots = 0;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.parent.is_none() {
                roots += 1;
            }
            if let Some(parent) = entry.parent {
                if parent == entry.mount || self.find(parent).is_none() {
                    return Err(MountError::InvalidTopology);
                }
            }
            if self.entries[..index]
                .iter()
                .any(|other| other.mount == entry.mount)
            {
                return Err(MountError::InvalidTopology);
            }
        }
        if roots == 1 {
            for entry in self.entries {
                let mut cursor = Some(*entry);
                let mut hops = 0;
                while let Some(current) = cursor {
                    hops += 1;
                    if hops > self.entries.len() {
                        return Err(MountError::TopologyCycle);
                    }
                    cursor = current.parent.and_then(|parent| self.find(parent));
                }
            }
            Ok(())
        } else {
            Err(MountError::InvalidTopology)
        }
    }
    fn descendant_of(&self, child: MountId, ancestor: MountId) -> bool {
        let mut cursor = self.find(child);
        let mut hops = 0;
        while let Some(entry) = cursor {
            if entry.mount == ancestor {
                return true;
            }
            hops += 1;
            if hops > self.entries.len() {
                return true;
            }
            cursor = entry.parent.and_then(|parent| self.find(parent));
        }
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountOperation {
    Attach { mount: MountId, parent: MountId },
    Bind { source: MountId, parent: MountId },
    Move { mount: MountId, parent: MountId },
    Remount { mount: MountId, flags: MountFlags },
    Setattr { mount: MountId, flags: MountFlags },
    PivotRoot { new_root: MountId, put_old: MountId },
    Unmount { mount: MountId, lazy: bool },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MountPlan {
    pub namespace: NamespaceId,
    pub expected: NamespaceGeneration,
    pub next: NamespaceGeneration,
    pub operation: MountOperation,
}
pub fn plan_mount(
    snapshot: TopologySnapshot<'_>,
    authority: MountAuthority,
    operation: MountOperation,
) -> Result<MountPlan, MountError> {
    snapshot.validate()?;
    if !authority.administer {
        return Err(MountError::PermissionDenied);
    }
    match operation {
        MountOperation::Attach { mount, parent } => {
            if snapshot.find(parent).is_none() {
                return Err(MountError::InvalidTopology);
            }
            if snapshot.find(mount).is_some() && snapshot.descendant_of(parent, mount) {
                return Err(MountError::TopologyCycle);
            }
        }
        MountOperation::Bind { source, parent } => {
            if snapshot.find(source).is_none() || snapshot.find(parent).is_none() {
                return Err(MountError::InvalidTopology);
            }
        }
        MountOperation::Move { mount, parent } => {
            if snapshot.find(mount).is_none() || snapshot.find(parent).is_none() {
                return Err(MountError::InvalidTopology);
            }
            if snapshot.descendant_of(parent, mount) {
                return Err(MountError::TopologyCycle);
            }
        }
        MountOperation::Remount { mount, .. } | MountOperation::Setattr { mount, .. } => {
            if snapshot.find(mount).is_none() {
                return Err(MountError::InvalidTopology);
            }
        }
        MountOperation::PivotRoot { new_root, put_old } => {
            if !authority.pivot_root {
                return Err(MountError::PermissionDenied);
            }
            let root = snapshot
                .entries
                .iter()
                .find(|entry| entry.parent.is_none())
                .map(|entry| entry.mount);
            if root != Some(put_old) || snapshot.find(new_root).is_none() {
                return Err(MountError::RootRequired);
            }
        }
        MountOperation::Unmount { mount, lazy } => {
            let entry = snapshot.find(mount).ok_or(MountError::InvalidTopology)?;
            if entry.parent.is_none() {
                return Err(MountError::RootRequired);
            }
            if !entry.detachable && !(lazy && authority.lazy_unmount) {
                return Err(MountError::Busy);
            }
        }
    }
    Ok(MountPlan {
        namespace: snapshot.namespace,
        expected: snapshot.generation,
        next: snapshot.generation.next()?,
        operation,
    })
}
pub fn plan_is_current(plan: MountPlan, snapshot: TopologySnapshot<'_>) -> bool {
    plan.namespace == snapshot.namespace && plan.expected == snapshot.generation
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(value: u64) -> MountId {
        MountId::new(value).unwrap()
    }
    fn snapshot<'a>(entries: &'a [TopologyEntry]) -> TopologySnapshot<'a> {
        TopologySnapshot {
            namespace: NamespaceId::new(1).unwrap(),
            generation: NamespaceGeneration::initial(),
            entries,
        }
    }
    fn topology() -> [TopologyEntry; 2] {
        [
            TopologyEntry {
                mount: id(1),
                parent: None,
                flags: MountFlags::default(),
                detachable: false,
            },
            TopologyEntry {
                mount: id(2),
                parent: Some(id(1)),
                flags: MountFlags::default(),
                detachable: false,
            },
        ]
    }
    #[test]
    fn cycles_are_rejected() {
        let entries = topology();
        assert_eq!(
            plan_mount(
                snapshot(&entries),
                MountAuthority {
                    administer: true,
                    ..MountAuthority::default()
                },
                MountOperation::Move {
                    mount: id(1),
                    parent: id(2)
                }
            ),
            Err(MountError::TopologyCycle)
        );
    }
    #[test]
    fn plan_lifecycle_is_generation_guarded() {
        let entries = topology();
        let before = snapshot(&entries);
        let plan = plan_mount(
            before,
            MountAuthority {
                administer: true,
                ..MountAuthority::default()
            },
            MountOperation::Remount {
                mount: id(2),
                flags: MountFlags::RDONLY,
            },
        )
        .unwrap();
        assert!(plan_is_current(plan, before));
        assert!(!plan_is_current(
            plan,
            TopologySnapshot {
                generation: before.generation.next().unwrap(),
                ..before
            }
        ));
    }
    #[test]
    fn lazy_unmount_needs_authority() {
        let entries = topology();
        assert_eq!(
            plan_mount(
                snapshot(&entries),
                MountAuthority {
                    administer: true,
                    ..MountAuthority::default()
                },
                MountOperation::Unmount {
                    mount: id(2),
                    lazy: true
                }
            ),
            Err(MountError::Busy)
        );
        assert!(
            plan_mount(
                snapshot(&entries),
                MountAuthority {
                    administer: true,
                    lazy_unmount: true,
                    pivot_root: false
                },
                MountOperation::Unmount {
                    mount: id(2),
                    lazy: true
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn uapi_flag_admission_and_atime_transitions_are_linux_owned() {
        assert_eq!(
            validate_mount_flags(MS_NOUSER as i32),
            Err(UapiError::Invalid)
        );
        // Superblock-scoped legacy flags are accepted, not refused.
        assert_eq!(validate_mount_flags(MS_SYNCHRONOUS as i32), Ok(MS_SYNCHRONOUS));
        assert_eq!(validate_mount_flags(MS_DIRSYNC as i32), Ok(MS_DIRSYNC));
        assert_eq!(validate_mount_flags(MS_I_VERSION as i32), Ok(MS_I_VERSION));
        assert_eq!(validate_mount_flags(MS_LAZYTIME as i32), Ok(MS_LAZYTIME));
        // Only MS_NOUSER is refused; MS_KERNMOUNT survives untouched even
        // though userspace has no business setting it.
        assert_eq!(
            validate_mount_flags((MS_KERNMOUNT | MS_RDONLY) as i32),
            Ok(MS_KERNMOUNT | MS_RDONLY)
        );
        assert_eq!(
            validate_mount_flags((MS_MGC_VAL | MS_RDONLY) as i32),
            Ok(MS_RDONLY)
        );
        assert_eq!(validate_open_tree(OPEN_TREE_CLOEXEC), Ok(true));
        assert_eq!(
            validate_open_tree(OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC),
            Ok(true)
        );
        assert_eq!(
            validate_open_tree(OPEN_TREE_CLOEXEC | AT_RECURSIVE),
            Err(UapiError::Invalid)
        );
        assert_eq!(
            validate_open_tree(OPEN_TREE_CLOEXEC | (1 << 31)),
            Err(UapiError::Invalid)
        );
        assert_eq!(
            validate_fsmount_attrs(MOUNT_ATTR_NODIRATIME),
            Ok(())
        );
        assert_eq!(admit_fsmount(0, MOUNT_ATTR_NODIRATIME, false, true), Ok(false));
        assert_eq!(
            apply_mount_attr_flags(0, MOUNT_ATTR_NODIRATIME as u64, 0, 0, 0),
            Ok(MS_NODIRATIME)
        );
        assert_eq!(
            apply_mount_attr_flags(
                MS_RELATIME,
                MOUNT_ATTR_STRICTATIME as u64,
                MOUNT_ATTR_ATIME as u64,
                0,
                0
            ),
            Ok(MS_STRICTATIME)
        );
    }

    /// `vfs_open_tree()` (fs/namespace.c) — the accepted bit set, the
    /// `AT_RECURSIVE`-without-a-clone rejection and the "only one of
    /// OPEN_TREE_CLONE/OPEN_TREE_NAMESPACE" rejection.
    #[test]
    fn open_tree_admits_linux_clone_flags_and_rejects_invalid_combinations() {
        assert_eq!(validate_open_tree(0), Ok(false));
        assert_eq!(validate_open_tree(OPEN_TREE_CLONE), Ok(false));
        assert_eq!(validate_open_tree(OPEN_TREE_NAMESPACE), Ok(false));
        assert_eq!(validate_open_tree(OPEN_TREE_NAMESPACE | AT_RECURSIVE), Ok(false));
        assert_eq!(
            validate_open_tree(OPEN_TREE_CLONE | AT_RECURSIVE | AT_EMPTY_PATH),
            Ok(false)
        );
        assert_eq!(
            validate_open_tree(OPEN_TREE_NAMESPACE | OPEN_TREE_CLOEXEC),
            Ok(true)
        );
        // Both clone flags at once is `hweight32(...) > 1`.
        assert_eq!(
            validate_open_tree(OPEN_TREE_CLONE | OPEN_TREE_NAMESPACE),
            Err(UapiError::Invalid)
        );
        // AT_RECURSIVE alone (neither clone flag) is rejected.
        assert_eq!(validate_open_tree(AT_RECURSIVE), Err(UapiError::Invalid));
        assert_eq!(
            validate_open_tree(OPEN_TREE_CLOEXEC | AT_NO_AUTOMOUNT | AT_SYMLINK_NOFOLLOW),
            Ok(true)
        );
        assert_eq!(
            validate_open_tree(AT_RECURSIVE | 1 << 31),
            Err(UapiError::Invalid)
        );
    }

    /// `SYSCALL_DEFINE3(fsmount, ...)` — `FSMOUNT_CLOEXEC | FSMOUNT_NAMESPACE`
    /// plus `FSMOUNT_VALID_FLAGS` and the `MOUNT_ATTR__ATIME` enum switch,
    /// which are applied after the capability decision at `fs/namespace.c`:
    /// 4449-4456 and before the descriptor lookup at `:4473`.
    #[test]
    fn fsmount_admits_namespace_flag_and_linux_mount_attributes() {
        assert_eq!(
            admit_fsmount(FSMOUNT_NAMESPACE, 0, true, false),
            Ok(false)
        );
        assert_eq!(
            admit_fsmount(FSMOUNT_CLOEXEC | FSMOUNT_NAMESPACE, 0, true, false),
            Ok(true)
        );
        assert_eq!(
            admit_fsmount(0x4, 0, true, true),
            Err(UapiError::Invalid)
        );
        // FSMOUNT_VALID_FLAGS excludes MOUNT_ATTR_IDMAP.
        assert_eq!(
            admit_fsmount(0, MOUNT_ATTR_IDMAP, true, true),
            Err(UapiError::Invalid)
        );
        assert_eq!(admit_fsmount(0, MOUNT_ATTR_RDONLY, true, true), Ok(false));
        assert_eq!(
            admit_fsmount(0, MOUNT_ATTR_ATIME, true, true),
            Err(UapiError::Invalid)
        );
        assert_eq!(
            admit_fsmount(0, MOUNT_ATTR_NOATIME | MOUNT_ATTR_NODIRATIME, true, true),
            Ok(false)
        );
    }

    /// The capability decision precedes attribute validation, and the two
    /// flag forms use different capability domains: `FSMOUNT_NAMESPACE` needs
    /// `ns_capable(current_user_ns(), CAP_SYS_ADMIN)` (`fs/namespace.c`:4451-4453)
    /// and the plain form needs `may_mount()` (`:4455-4456`, `:2007-2010`).
    #[test]
    fn fsmount_checks_capability_before_attributes() {
        const BAD_ATTRS: u32 = 0x8000_0000;
        // `MOUNT_ATTR_RELATIME` is 0, so the invalid combination is a pair of
        // non-zero atime selectors (`MOUNT_ATTR_NOATIME | MOUNT_ATTR_STRICTATIME`
        // is 0x30, which is neither of the three cases in the switch).
        const BAD_ATIME: u32 = MOUNT_ATTR_NOATIME | MOUNT_ATTR_STRICTATIME;
        for attrs in [BAD_ATTRS, BAD_ATIME] {
            // The namespace form ignores `may_mount()` in both directions.
            assert_eq!(
                admit_fsmount(FSMOUNT_NAMESPACE, attrs, false, true),
                Err(UapiError::Permission)
            );
            assert_eq!(
                admit_fsmount(FSMOUNT_NAMESPACE, attrs, true, false),
                Err(UapiError::Invalid)
            );
            // The plain form ignores the caller's own user namespace.
            assert_eq!(
                admit_fsmount(0, attrs, true, false),
                Err(UapiError::Permission)
            );
            assert_eq!(
                admit_fsmount(0, attrs, false, true),
                Err(UapiError::Invalid)
            );
        }
        // A malformed flag word still outranks the capability test.
        assert_eq!(
            admit_fsmount(0x4000_0000, BAD_ATTRS, false, false),
            Err(UapiError::Invalid)
        );
        assert_eq!(admit_fsmount(0, 0, false, true), Ok(false));
    }

    /// `vfs_open_tree()` applies the same two capability domains to its flag
    /// word (`fs/namespace.c`:3224-3233) after the flag-word rules.
    #[test]
    fn open_tree_capability_domains_match_vfs_open_tree() {
        assert_eq!(admit_open_tree(OPEN_TREE_CLONE, false, true), Ok(false));
        assert_eq!(
            admit_open_tree(OPEN_TREE_CLONE, false, false),
            Err(UapiError::Permission)
        );
        assert_eq!(
            admit_open_tree(OPEN_TREE_NAMESPACE, true, false),
            Ok(false)
        );
        assert_eq!(
            admit_open_tree(OPEN_TREE_NAMESPACE, false, true),
            Err(UapiError::Permission)
        );
        // A plain path descriptor needs no capability at all.
        assert_eq!(admit_open_tree(0, false, false), Ok(false));
        assert_eq!(admit_open_tree(OPEN_TREE_CLOEXEC, false, false), Ok(true));
        // The flag-word rules still run first.
        assert_eq!(
            admit_open_tree(OPEN_TREE_CLONE | OPEN_TREE_NAMESPACE, false, false),
            Err(UapiError::Invalid)
        );
        assert_eq!(admit_open_tree(AT_RECURSIVE, false, false), Err(UapiError::Invalid));
    }

    /// `copy_mnt_id_req()` (fs/namespace.c) — the descriptor form forbids the
    /// mount-id fields; the id form requires a unique id and forbids mixing
    /// `mnt_ns_fd` with `mnt_ns_id`.
    #[test]
    fn statmount_request_validation_follows_linux_by_fd_split() {
        let id_only = MntIdReq {
            mnt_ns_fd: 0,
            mnt_id: (1u64 << 31) + 1,
            param: 0,
            ns_id: 0,
        };
        assert_eq!(validate_statmount_request(id_only, false), Ok(()));
        assert_eq!(validate_statmount_request(id_only, true), Err(UapiError::Invalid));
        assert_eq!(validate_mnt_id_request(id_only), Ok(()));

        let by_fd = MntIdReq {
            mnt_ns_fd: 7,
            mnt_id: 0,
            param: STATMOUNT_SUPPORTED,
            ns_id: 0,
        };
        assert_eq!(validate_statmount_request(by_fd, true), Ok(()));
        // The legacy id form rejects a mount id inside the 31-bit range.
        assert_eq!(
            validate_statmount_request(by_fd, false),
            Err(UapiError::Invalid)
        );

        let mixed = MntIdReq {
            mnt_ns_fd: 7,
            mnt_id: (1u64 << 31) + 1,
            param: 0,
            ns_id: 9,
        };
        assert_eq!(validate_statmount_request(mixed, false), Err(UapiError::Invalid));
        assert_eq!(validate_statmount_request(mixed, true), Err(UapiError::Invalid));
    }

    /// `SYSCALL_DEFINE4(statmount, ...)` accepts only `STATMOUNT_BY_FD`.
    #[test]
    fn statmount_flags_admit_only_by_fd() {
        assert_eq!(validate_statmount_flags(0), Ok(false));
        assert_eq!(validate_statmount_flags(STATMOUNT_BY_FD), Ok(true));
        assert_eq!(validate_statmount_flags(0x2), Err(UapiError::Invalid));
        assert_eq!(validate_statmount_flags(1 << 31), Err(UapiError::Invalid));
    }

    /// `SYSCALL_DEFINE3(fspick, ...)`: `may_mount()` precedes the flag word.
    #[test]
    fn fspick_capability_precedes_flag_validation() {
        assert_eq!(
            validate_fspick(FSPICK_CLOEXEC | FSPICK_EMPTY_PATH, true),
            Ok(true)
        );
        assert_eq!(
            validate_fspick(FSPICK_SYMLINK_NOFOLLOW | FSPICK_NO_AUTOMOUNT, true),
            Ok(false)
        );
        assert_eq!(validate_fspick(0x10, true), Err(FspickReject::InvalidFlags));
        assert_eq!(validate_fspick(0, false), Err(FspickReject::NotCapable));
        // The capability gate wins even when the flags are also invalid.
        assert_eq!(
            validate_fspick(0x8000_0000, false),
            Err(FspickReject::NotCapable)
        );
    }

    #[test]
    fn wire_records_have_fixed_x86_64_layout_and_decode_versioned_requests() {
        assert_eq!(STATMOUNT_PREFIX_SIZE, 512);
        // Unique mount IDs start above the legacy 31-bit ID range.
        let mount_id = (1u64 << 31) + 1;
        let mut bytes = [0u8; MNT_ID_REQ_SIZE_VER1];
        bytes[..4].copy_from_slice(&(MNT_ID_REQ_SIZE_VER1 as u32).to_ne_bytes());
        bytes[8..16].copy_from_slice(&mount_id.to_ne_bytes());
        bytes[16..24].copy_from_slice(&7u64.to_ne_bytes());
        bytes[24..32].copy_from_slice(&1u64.to_ne_bytes());
        assert_eq!(MntIdReq::decode(&bytes).unwrap().mnt_id, mount_id);
        bytes[..4].copy_from_slice(&(MNT_ID_REQ_SIZE_VER0 as u32).to_ne_bytes());
        assert_eq!(
            MntIdReq::decode(&bytes[..MNT_ID_REQ_SIZE_VER0])
                .unwrap()
                .ns_id,
            0
        );
        bytes[..4].copy_from_slice(&(MNT_ID_REQ_SIZE_VER1 as u32).to_ne_bytes());
        let request = MntIdReq::decode(&bytes).unwrap();
        assert_eq!(validate_mnt_id_request(request), Ok(()));
        for invalid_id in [0, 42, (1u64 << 31) - 1, 1u64 << 31] {
            assert_eq!(
                validate_mnt_id_request(MntIdReq {
                    mnt_id: invalid_id,
                    ..request
                }),
                Err(UapiError::Invalid)
            );
        }
        assert_eq!(
            validate_mnt_id_request(MntIdReq {
                mnt_ns_fd: 3,
                ..request
            }),
            Err(UapiError::Invalid)
        );
        assert_eq!(validate_unique_mount_id(mount_id), Ok(()));
        assert_eq!(validate_unique_mount_id(1 << 31), Err(UapiError::Invalid));
    }

    #[test]
    fn fsconfig_shapes_and_statmount_listmount_flags_are_decoded_in_abi() {
        assert_eq!(
            validate_fsconfig_shape(FSCONFIG_SET_STRING, true, true, 0, -100),
            Ok(())
        );
        assert_eq!(
            validate_fsconfig_shape(FSCONFIG_SET_STRING, true, false, 0, -100),
            Err(UapiError::Invalid)
        );
        assert_eq!(
            validate_fsconfig_shape(99, false, false, 0, -100),
            Err(UapiError::Unsupported)
        );
        assert_eq!(validate_statmount_flags(2), Err(UapiError::Invalid));
        assert_eq!(validate_listmount_flags(LISTMOUNT_REVERSE), Ok(true));
    }

    #[test]
    fn topology_validation_rejects_preexisting_parent_cycle() {
        let entries = [
            TopologyEntry {
                mount: id(1),
                parent: None,
                flags: MountFlags::default(),
                detachable: false,
            },
            TopologyEntry {
                mount: id(2),
                parent: Some(id(3)),
                flags: MountFlags::default(),
                detachable: false,
            },
            TopologyEntry {
                mount: id(3),
                parent: Some(id(2)),
                flags: MountFlags::default(),
                detachable: false,
            },
        ];
        assert_eq!(
            snapshot(&entries).validate(),
            Err(MountError::TopologyCycle)
        );
    }
}
