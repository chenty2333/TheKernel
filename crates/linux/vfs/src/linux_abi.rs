//! Pure Linux file-cache, seal, quota and xattr ABI policy.
#![allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxVfsError {
    InvalidFlags,
    InvalidRange,
    StructTooSmall,
    StructTooLarge,
    XattrTooLarge,
    InvalidXattrName,
    SealDenied,
    QuotaOverflow,
}

/// `AT_SYMLINK_NOFOLLOW`, accepted by the v6.18 xattr-at and file-attribute
/// syscalls.
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
/// `AT_EMPTY_PATH`, accepted by the v6.18 xattr-at and file-attribute
/// syscalls.
pub const AT_EMPTY_PATH: u32 = 0x1000;

/// Flags accepted by the pathname lookup part of syscall numbers 463 through
/// 466 and 468 through 469.
pub const FILE_AT_FLAGS: u32 = AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH;

/// The largest versioned syscall structure Linux accepts for these ABI entry
/// points.  This is deliberately a page, rather than a VFS allocation limit.
pub const VERSIONED_FILE_ABI_MAX_SIZE: usize = 4096;

/// The direction in which a versioned structure crosses the user boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StructCopyDirection {
    /// `copy_struct_from_user()`: short inputs are zero-extended and a
    /// nonzero unknown trailing extension is rejected.
    FromUser,
    /// `copy_struct_to_user()`: known output is copied and any user-visible
    /// extension is cleared.
    ToUser,
}

/// A copy-size policy derived from one Linux versioned-structure syscall.
///
/// The consumer performs the actual usercopy.  Keeping this plan independent
/// of an address space makes the ABI crate usable by every VFS backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StructCopyPlan {
    /// Size supplied in the syscall's `usize` argument.
    pub user_size: usize,
    /// First published ABI size required by the syscall.
    pub minimum_size: usize,
    /// Size understood by this kernel version.
    pub kernel_size: usize,
    /// Usercopy direction and extension treatment.
    pub direction: StructCopyDirection,
}

impl StructCopyPlan {
    const fn versioned(
        user_size: usize,
        minimum_size: usize,
        kernel_size: usize,
        direction: StructCopyDirection,
    ) -> Result<Self, LinuxVfsError> {
        // The four versioned-structure entry points test the page cap before
        // attempting a user copy. Their individual callers preserve their
        // source-level EINVAL/E2BIG ordering; no size can trigger both.
        if user_size > VERSIONED_FILE_ABI_MAX_SIZE {
            return Err(LinuxVfsError::StructTooLarge);
        }
        if user_size < minimum_size {
            return Err(LinuxVfsError::StructTooSmall);
        }
        Ok(Self {
            user_size,
            minimum_size,
            kernel_size,
            direction,
        })
    }
}

/// Native Linux v6.18 `struct xattr_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct XattrArgs {
    /// User virtual address of the attribute value buffer.
    pub value: u64,
    /// Input or output buffer size in bytes.
    pub size: u32,
    /// `XATTR_*` flags; only setxattrat permits nonzero bits.
    pub flags: u32,
}

/// First published size of [`XattrArgs`].
pub const XATTR_ARGS_SIZE_VER0: usize = 16;
/// Size understood by Linux v6.18.
pub const XATTR_ARGS_SIZE_LATEST: usize = XATTR_ARGS_SIZE_VER0;

/// `XATTR_CREATE`.
pub const XATTR_CREATE: u32 = 0x1;
/// `XATTR_REPLACE`.
pub const XATTR_REPLACE: u32 = 0x2;
/// Bits accepted in `XattrArgs::flags` by `setxattrat`.
pub const XATTR_SET_FLAGS: u32 = XATTR_CREATE | XATTR_REPLACE;

/// Native Linux v6.18 `struct file_attr`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FileAttr {
    /// `FS_XFLAG_*` values.
    pub fa_xflags: u64,
    /// Extent-size allocation hint.
    pub fa_extsize: u32,
    /// Extent count, output-only.
    pub fa_nextents: u32,
    /// Project identifier.
    pub fa_projid: u32,
    /// Copy-on-write extent-size allocation hint.
    pub fa_cowextsize: u32,
}

/// First published size of [`FileAttr`].
pub const FILE_ATTR_SIZE_VER0: usize = 24;
/// Size understood by Linux v6.18.
pub const FILE_ATTR_SIZE_LATEST: usize = FILE_ATTR_SIZE_VER0;

/// The v6.18 `FS_XFLAG_*` mask accepted by `file_setattr` before filesystem
/// specific checks.  `PREALLOC` and `HASATTR` are syntactically accepted but
/// subsequently stripped as read-only by the generic file-attribute layer.
pub const FILE_ATTR_XFLAGS_MASK: u64 = 0x8001_fffb;

/// Validates the common `*at` flag word used by xattr-at and file-attribute
/// syscalls.
pub const fn validate_file_at_flags(flags: u32) -> Result<(), LinuxVfsError> {
    if flags & !FILE_AT_FLAGS != 0 {
        Err(LinuxVfsError::InvalidFlags)
    } else {
        Ok(())
    }
}

/// Describes the `setxattrat` usercopy after its `usize` validation.
pub const fn setxattrat_copy_plan(user_size: usize) -> Result<StructCopyPlan, LinuxVfsError> {
    StructCopyPlan::versioned(
        user_size,
        XATTR_ARGS_SIZE_VER0,
        XATTR_ARGS_SIZE_LATEST,
        StructCopyDirection::FromUser,
    )
}

/// Describes the `getxattrat` usercopy after its `usize` validation.
pub const fn getxattrat_copy_plan(user_size: usize) -> Result<StructCopyPlan, LinuxVfsError> {
    StructCopyPlan::versioned(
        user_size,
        XATTR_ARGS_SIZE_VER0,
        XATTR_ARGS_SIZE_LATEST,
        StructCopyDirection::FromUser,
    )
}

/// Validates `setxattrat`'s xattr operation flags.  Linux deliberately admits
/// the `CREATE | REPLACE` combination; the selected xattr provider determines
/// the resulting existence error.
pub const fn validate_setxattr_flags(flags: u32) -> Result<(), LinuxVfsError> {
    if flags & !XATTR_SET_FLAGS != 0 {
        Err(LinuxVfsError::InvalidFlags)
    } else {
        Ok(())
    }
}

/// Validates `getxattrat`'s reserved `xattr_args.flags` field.
pub const fn validate_getxattr_flags(flags: u32) -> Result<(), LinuxVfsError> {
    if flags != 0 {
        Err(LinuxVfsError::InvalidFlags)
    } else {
        Ok(())
    }
}

/// Describes the `file_getattr` output copy after its `usize` validation.
pub const fn file_getattr_copy_plan(user_size: usize) -> Result<StructCopyPlan, LinuxVfsError> {
    StructCopyPlan::versioned(
        user_size,
        FILE_ATTR_SIZE_VER0,
        FILE_ATTR_SIZE_LATEST,
        StructCopyDirection::ToUser,
    )
}

/// Describes the `file_setattr` input copy after its `usize` validation.
pub const fn file_setattr_copy_plan(user_size: usize) -> Result<StructCopyPlan, LinuxVfsError> {
    StructCopyPlan::versioned(
        user_size,
        FILE_ATTR_SIZE_VER0,
        FILE_ATTR_SIZE_LATEST,
        StructCopyDirection::FromUser,
    )
}

/// Validates the generic VFS portion of a `file_setattr` request.  The VFS
/// later clears read-only xflags and leaves semantic filesystem validation to
/// the selected attribute provider.
pub const fn validate_file_setattr_xflags(xflags: u64) -> Result<(), LinuxVfsError> {
    if xflags & !FILE_ATTR_XFLAGS_MASK != 0 {
        Err(LinuxVfsError::InvalidFlags)
    } else {
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fadvise {
    Normal,
    Sequential,
    Random,
    NoReuse,
    WillNeed,
    DontNeed,
}
impl Fadvise {
    pub const fn from_raw(raw: i32) -> Result<Self, LinuxVfsError> {
        match raw {
            0 => Ok(Self::Normal),
            1 => Ok(Self::Random),
            2 => Ok(Self::Sequential),
            3 => Ok(Self::WillNeed),
            4 => Ok(Self::DontNeed),
            5 => Ok(Self::NoReuse),
            _ => Err(LinuxVfsError::InvalidFlags),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileRange {
    pub offset: u64,
    pub length: u64,
}
impl FileRange {
    pub const fn new(offset: i64, length: i64) -> Result<Self, LinuxVfsError> {
        if offset < 0 || length < 0 {
            return Err(LinuxVfsError::InvalidRange);
        }
        let offset = offset as u64;
        let length = length as u64;
        if length != 0 && offset.checked_add(length).is_none() {
            return Err(LinuxVfsError::InvalidRange);
        }
        Ok(Self { offset, length })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadvisePlan {
    pub range: FileRange,
    pub advice: Fadvise,
}
impl FadvisePlan {
    pub fn new(offset: i64, length: i64, advice: i32) -> Result<Self, LinuxVfsError> {
        Ok(Self {
            range: FileRange::new(offset, length)?,
            advice: Fadvise::from_raw(advice)?,
        })
    }
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CacheStat {
    pub nr_cache: u64,
    pub nr_dirty: u64,
    pub nr_writeback: u64,
    pub nr_evicted: u64,
    pub nr_recently_evicted: u64,
}

/// Native Linux `struct cachestat_range`, independent of its user-memory
/// transport.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CachestatRange {
    /// Byte offset at which the query starts.
    pub off: u64,
    /// Length of the queried interval.
    pub len: u64,
}

/// Inclusive page interval consumed by the generic cache mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CachestatPageRange {
    /// First queried page.
    pub first: u64,
    /// Last queried page.
    pub last: u64,
}

impl CachestatRange {
    /// Applies Linux's `cachestat(2)` inclusive page-index arithmetic.
    ///
    /// Linux represents `len == 0` with `last_index = ULONG_MAX`; it does not
    /// inspect the inode size.  Keeping that sentinel in the pure ABI plan
    /// also preserves syscall error ordering and includes nonresident
    /// workingset shadows beyond a concurrently truncated EOF.
    pub const fn page_range(self) -> CachestatPageRange {
        let first = self.off >> 12;
        let last = if self.len == 0 {
            u64::MAX
        } else {
            self.off.wrapping_add(self.len).wrapping_sub(1) >> 12
        };
        CachestatPageRange { first, last }
    }
}

/// Linux-visible cachestat admission failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CachestatAdmissionError {
    /// The mapping belongs to hugetlbfs.
    HugeTlb,
    /// Neither an open-file-description nor inode access route admitted it.
    PermissionDenied,
    /// The syscall flags word is nonzero.
    InvalidFlags,
}

/// Whether an open file description has Linux write access.
pub const fn cachestat_write_open(status_flags: u32) -> bool {
    // `cachestat` consults FMODE_WRITE, not merely the low O_ACCMODE bits.
    // O_PATH clears that mode even if a reconstructed status word retains
    // otherwise-write-looking access bits.
    status_flags & 0x0020_0000 == 0 && matches!(status_flags & 3, 1 | 2)
}

/// Applies Linux's cachestat authorization and error ordering after the
/// caller has copied in the range and observed VFS/credential facts.
pub const fn validate_cachestat_admission(
    is_hugetlbfs: bool,
    write_open: bool,
    owns_inode: bool,
    fowner_capable: bool,
    may_write: bool,
    flags: u32,
) -> Result<(), CachestatAdmissionError> {
    if is_hugetlbfs {
        return Err(CachestatAdmissionError::HugeTlb);
    }
    if !(write_open || owns_inode || fowner_capable || may_write) {
        return Err(CachestatAdmissionError::PermissionDenied);
    }
    if flags != 0 {
        return Err(CachestatAdmissionError::InvalidFlags);
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemfdSeals(u32);
impl MemfdSeals {
    pub const SEAL: u32 = 1;
    pub const SHRINK: u32 = 2;
    pub const GROW: u32 = 4;
    pub const WRITE: u32 = 8;
    pub const FUTURE_WRITE: u32 = 16;
    pub const ALL: u32 = 31;
    pub const fn new(bits: u32) -> Result<Self, LinuxVfsError> {
        if bits & !Self::ALL != 0 {
            Err(LinuxVfsError::InvalidFlags)
        } else {
            Ok(Self(bits))
        }
    }
    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn add(
        current: Self,
        requested: Self,
        sealing_allowed: bool,
    ) -> Result<Self, LinuxVfsError> {
        if !sealing_allowed || current.0 & Self::SEAL != 0 {
            Err(LinuxVfsError::SealDenied)
        } else {
            Ok(Self(current.0 | requested.0))
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XattrValuePlan {
    length: usize,
}
impl XattrValuePlan {
    pub const MAX: usize = 65536;
    pub const NAME_MAX: usize = 255;
    pub const fn new(name_length: usize, value_length: usize) -> Result<Self, LinuxVfsError> {
        if name_length == 0 || name_length > Self::NAME_MAX {
            return Err(LinuxVfsError::InvalidXattrName);
        }
        if value_length > Self::MAX {
            return Err(LinuxVfsError::XattrTooLarge);
        }
        Ok(Self {
            length: value_length,
        })
    }
    pub const fn length(self) -> usize {
        self.length
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QuotaUsage {
    pub bytes: u64,
    pub inodes: u64,
}
impl QuotaUsage {
    pub const fn checked_charge(self, bytes: u64, inodes: u64) -> Result<Self, LinuxVfsError> {
        match (
            self.bytes.checked_add(bytes),
            self.inodes.checked_add(inodes),
        ) {
            (Some(bytes), Some(inodes)) => Ok(Self { bytes, inodes }),
            _ => Err(LinuxVfsError::QuotaOverflow),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds() {
        assert_eq!(FileRange::new(-1, 0), Err(LinuxVfsError::InvalidRange));
        assert_eq!(
            XattrValuePlan::new(1, 65537),
            Err(LinuxVfsError::XattrTooLarge)
        );
        assert_eq!(
            QuotaUsage {
                bytes: u64::MAX,
                inodes: 0
            }
            .checked_charge(1, 0),
            Err(LinuxVfsError::QuotaOverflow)
        );
    }

    #[test]
    fn cachestat_preserves_wrapping_page_range_and_admission_order() {
        assert_eq!(
            CachestatRange {
                off: 0x2000,
                len: 0,
            }
            .page_range(),
            CachestatPageRange {
                first: 2,
                last: u64::MAX,
            }
        );
        assert_eq!(
            CachestatRange {
                off: u64::MAX,
                len: 2,
            }
            .page_range(),
            CachestatPageRange {
                first: u64::MAX >> 12,
                last: 0,
            }
        );
        assert!(cachestat_write_open(1));
        assert!(cachestat_write_open(2));
        assert!(!cachestat_write_open(0));
        // O_PATH carries no access-mode bits, and an O_RDONLY descriptor
        // with unrelated status flags must likewise not accidentally gain
        // write-open admission.
        assert!(!cachestat_write_open(0x0020_0000));
        assert!(!cachestat_write_open(0x0020_0000 | 0x800));
        assert!(!cachestat_write_open(0x0020_0000 | 1));
        assert!(!cachestat_write_open(0x0020_0000 | 2));
        assert_eq!(
            validate_cachestat_admission(true, false, false, false, false, 1),
            Err(CachestatAdmissionError::HugeTlb)
        );
        assert_eq!(
            validate_cachestat_admission(false, false, false, false, false, 1),
            Err(CachestatAdmissionError::PermissionDenied)
        );
        assert_eq!(
            validate_cachestat_admission(false, false, true, false, false, 1),
            Err(CachestatAdmissionError::InvalidFlags)
        );
        // A permitted descriptor still rejects unknown flags, whereas each
        // legitimate authorization route accepts the zero-flags request.
        assert_eq!(
            validate_cachestat_admission(false, true, false, false, false, 0),
            Ok(())
        );
        assert_eq!(
            validate_cachestat_admission(false, false, true, false, false, 0),
            Ok(())
        );
        assert_eq!(
            validate_cachestat_admission(false, false, false, true, false, 0),
            Ok(())
        );
        assert_eq!(
            validate_cachestat_admission(false, false, false, false, true, 0),
            Ok(())
        );
    }
}
