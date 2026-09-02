//! Linux cachestat(2) ABI adapter.

use core::ffi::c_int;

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::W_OK;
use linux_vfs::{
    CacheStat as Cachestat, CachestatAdmissionError, CachestatRange, cachestat_write_open,
    validate_cachestat_admission,
};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use crate::{
    file::{
        FileLike, get_file_like,
        permission::{
            VfsSecurityContext, check_inode_permissions_with_security_and_idmap,
            inode_owner_and_fowner_with_idmap,
        },
    },
    mm::map_usercopy_error,
    task::AsThread,
};

/// Splits `inode_owner_or_capable(mnt_idmap, inode)` into the two facts that
/// the cachestat ABI keeps separately.  Keeping the decision branches visible
/// matters even though either one admits the syscall: it preserves the Linux
/// contract graph and prevents an idmapped `CAP_FOWNER` check from being
/// accidentally evaluated in the filesystem-owner namespace.
fn cachestat_owner_and_fowner(
    metadata: &axfs_ng_vfs::Metadata,
    security: &VfsSecurityContext,
    idmap: Option<&crate::mounts::MountIdmap>,
) -> (bool, bool) {
    inode_owner_and_fowner_with_idmap(metadata, security, idmap)
}

fn can_do_cachestat(
    file: &dyn FileLike,
    status_flags: u32,
    security: &VfsSecurityContext,
    idmap: Option<&crate::mounts::MountIdmap>,
) -> AxResult<(bool, bool, bool, bool)> {
    let write_open = cachestat_write_open(status_flags);
    let Some(location) = file.cachestat_location() else {
        return Ok((write_open, false, false, false));
    };
    let metadata = location.metadata()?;
    let (owns_inode, fowner_capable) = cachestat_owner_and_fowner(&metadata, security, idmap);
    let may_write = !(write_open || owns_inode || fowner_capable)
        && check_inode_permissions_with_security_and_idmap(
            location, &metadata, W_OK, security, idmap,
        )
        .is_ok();
    Ok((write_open, owns_inode, fowner_capable, may_write))
}

fn map_admission_error(error: CachestatAdmissionError) -> AxError {
    match error {
        CachestatAdmissionError::HugeTlb => AxError::OperationNotSupported,
        CachestatAdmissionError::PermissionDenied => AxError::OperationNotPermitted,
        CachestatAdmissionError::InvalidFlags => AxError::InvalidInput,
    }
}

/// Linux represents `len == 0` with `last_index = ULONG_MAX`; it does not
/// consult the inode length.  This matters for nonresident workingset shadows
/// beyond a concurrently truncated EOF and also preserves Linux's error
/// ordering by avoiding an extra filesystem operation.
fn cachestat_pages(range: CachestatRange) -> (u64, u64) {
    let pages = range.page_range();
    (pages.first, pages.last)
}

/// Implements the Linux v6.18 cachestat(2) contract.
///
/// Keep validation in the upstream order: descriptor lookup, range copyin,
/// hugetlb classification, admission, flags, then output copyout.
pub(crate) fn sys_cachestat<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: c_int,
    range: *const CachestatRange,
    output: *mut Cachestat,
    flags: u32,
) -> AxResult<isize> {
    // EBADF must win over an invalid range pointer.
    let file = get_file_like(fd)?;
    let range = VmPtr::vm_read(range, memory).map_err(map_usercopy_error)?;

    // Linux classifies hugetlbfs immediately after range copyin. Do not run
    // DAC/LSM MAY_WRITE hooks first: even a discarded denial would create an
    // observable audit side effect for a syscall whose result is EOPNOTSUPP.
    if file.cachestat_is_hugetlbfs() {
        return Err(AxError::OperationNotSupported);
    }

    let current = current();
    let security = VfsSecurityContext::new(current.as_thread().current_cred());
    let idmap = file.vfs_mount_idmap();
    let (write_open, owns_inode, fowner_capable, may_write) =
        can_do_cachestat(&*file, file.status_flags(), &security, idmap.as_deref())?;
    validate_cachestat_admission(
        false,
        write_open,
        owns_inode,
        fowner_capable,
        may_write,
        flags,
    )
    .map_err(map_admission_error)?;

    let (first_page, last_page) = cachestat_pages(range);
    let snapshot = file.cachestat(first_page, last_page)?;
    let stats = Cachestat {
        nr_cache: snapshot.nr_cache,
        nr_dirty: snapshot.nr_dirty,
        nr_writeback: snapshot.nr_writeback,
        nr_evicted: snapshot.nr_evicted,
        nr_recently_evicted: snapshot.nr_recently_evicted,
    };
    VmMutPtr::vm_write(output, memory, stats).map_err(map_usercopy_error)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use linux_raw_sys::general::O_PATH;

    use super::*;

    #[test]
    fn root_maps_linux_admission_errors() {
        assert_eq!(
            map_admission_error(CachestatAdmissionError::HugeTlb),
            AxError::OperationNotSupported
        );
        assert_eq!(
            map_admission_error(CachestatAdmissionError::PermissionDenied),
            AxError::OperationNotPermitted
        );
        assert_eq!(
            map_admission_error(CachestatAdmissionError::InvalidFlags),
            AxError::InvalidInput
        );
    }

    #[test]
    fn cachestat_model_preserves_range_type_and_admission_boundaries() {
        // `len == 0` intentionally has no EOF dependence, while a wrapping
        // nonzero interval remains empty after inclusive page conversion.
        assert_eq!(
            cachestat_pages(CachestatRange {
                off: 0x2_000,
                len: 0,
            }),
            (2, u64::MAX)
        );
        assert_eq!(
            cachestat_pages(CachestatRange {
                off: u64::MAX,
                len: 2,
            }),
            (u64::MAX >> 12, 0)
        );

        // O_PATH is read-only for the access-mode check.  It can only reach
        // cachestat admission through inode ownership/capability/permission,
        // never by masquerading as a writable description.
        assert!(!cachestat_write_open(O_PATH));
        assert!(!cachestat_write_open(O_PATH | 0x800));
        assert!(!cachestat_write_open(O_PATH | 1));

        // The adapter delegates the Linux order after descriptor/range
        // validation: hugetlb rejection and access denial take precedence
        // over a nonzero flags word; flags win only after admission succeeds.
        assert_eq!(
            validate_cachestat_admission(true, true, false, false, false, 1),
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
    }
}
