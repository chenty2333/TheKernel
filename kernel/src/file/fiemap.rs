//! Linux `FS_IOC_FIEMAP` adapter for regular files.
//!
//! Raw UAPI parsing and userspace access stay in this module.  Filesystems
//! receive only a typed extent query and return owned metadata, so neither an
//! ext4 lock nor a cache-range owner is retained across a userspace fault.

use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use bytemuck::{Pod, Zeroable};
use linux_raw_sys::ioctl::{FIEMAP_FLAG_SYNC, FS_IOC_FIEMAP};

use super::IoctlContext;
use crate::mm::map_usercopy_error;

const FIEMAP_SUPPORTED_FLAGS: u32 = FIEMAP_FLAG_SYNC;
const FIEMAP_MAX_EXTENTS: u32 = u32::MAX / size_of::<LinuxFiemapExtent>() as u32;
const FIEMAP_MAX_BYTES: u64 = i64::MAX as u64;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
struct LinuxFiemap {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
struct LinuxFiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

const _: () = assert!(size_of::<LinuxFiemap>() == 32);
const _: () = assert!(size_of::<LinuxFiemapExtent>() == 56);

#[inline]
pub(super) const fn is_fiemap_command(command: u32) -> bool {
    command == FS_IOC_FIEMAP
}

fn extent_user_address(base: usize, index: usize) -> AxResult<usize> {
    index
        .checked_mul(size_of::<LinuxFiemapExtent>())
        .and_then(|offset| size_of::<LinuxFiemap>().checked_add(offset))
        .and_then(|offset| base.checked_add(offset))
        .ok_or(AxError::BadAddress)
}

fn write_header(context: &IoctlContext, address: usize, header: LinuxFiemap) -> AxResult<()> {
    context
        .user_memory()
        .write_value(address as *mut LinuxFiemap, header)
        .map_err(map_usercopy_error)
}

fn write_extent(
    context: &IoctlContext,
    base: usize,
    index: usize,
    extent: axfs_ng_vfs::FileExtent,
) -> AxResult<()> {
    let address = extent_user_address(base, index)?;
    let raw = LinuxFiemapExtent {
        fe_logical: extent.logical,
        fe_physical: extent.physical,
        fe_length: extent.length,
        fe_flags: extent.flags,
        ..LinuxFiemapExtent::default()
    };
    context
        .user_memory()
        .write_value(address as *mut LinuxFiemapExtent, raw)
        .map_err(map_usercopy_error)
}

fn validate_fiemap_request(header: &LinuxFiemap) -> AxResult<u64> {
    // Check the filesystem-wide offset limit before zero-length, capacity, or
    // flag handling.  An offset at maxbytes is never an empty successful
    // query; Linux reports EFBIG for it.
    if header.fm_start >= FIEMAP_MAX_BYTES {
        return Err(AxError::from(LinuxError::EFBIG));
    }
    if header.fm_extent_count as usize > axfs_ng_vfs::FILE_EXTENT_MAX {
        return Err(AxError::InvalidInput);
    }
    if header.fm_extent_count > FIEMAP_MAX_EXTENTS {
        return Err(AxError::InvalidInput);
    }
    Ok(header.fm_length.min(FIEMAP_MAX_BYTES - header.fm_start))
}

/// Executes `FS_IOC_FIEMAP` for one regular open file description.
///
/// Linux copies the header back even when the filesystem reports an error;
/// in particular unsupported flag bits are returned in `fm_flags` with
/// `EBADR`.  Extents are copied before the final header, matching the kernel's
/// observable partial-copy ordering on `EFAULT`.
pub(super) fn ioctl(file: &axfs::File, context: &IoctlContext, address: usize) -> AxResult<usize> {
    let mut header = context
        .user_memory()
        .read_value(address as *const LinuxFiemap)
        .map_err(map_usercopy_error)?;

    let query_length = validate_fiemap_request(&header)?;

    if header.fm_length == 0 {
        header.fm_mapped_extents = 0;
        write_header(context, address, header)?;
        return Err(AxError::InvalidInput);
    }

    let bad_flags = header.fm_flags & !FIEMAP_SUPPORTED_FLAGS;
    if bad_flags != 0 {
        header.fm_flags = bad_flags;
        header.fm_mapped_extents = 0;
        write_header(context, address, header)?;
        return Err(AxError::from(LinuxError::EBADR));
    }

    let max_extents = if header.fm_extent_count == 0 {
        0
    } else {
        usize::try_from(header.fm_extent_count).unwrap_or(usize::MAX)
    };
    let query = file.map_extents(
        header.fm_start,
        query_length,
        max_extents,
        header.fm_flags & FIEMAP_FLAG_SYNC != 0,
    );

    let operation = match query {
        Ok(mapped) => {
            if header.fm_extent_count == 0 {
                header.fm_mapped_extents = mapped.mapped_extents;
                Ok(())
            } else {
                let copy_count = mapped.extents.len().min(header.fm_extent_count as usize);
                let mut copied = 0usize;
                let mut copy_result = Ok(());
                for (index, extent) in mapped.extents.into_iter().take(copy_count).enumerate() {
                    if let Err(error) = write_extent(context, address, index, extent) {
                        copy_result = Err(error);
                        break;
                    }
                    copied += 1;
                }
                header.fm_mapped_extents = copied as u32;
                copy_result
            }
        }
        Err(error) => {
            header.fm_mapped_extents = 0;
            Err(error)
        }
    };

    // Linux's ioctl adapter always copies the updated fixed header after the
    // filesystem callback, and a header fault takes precedence.
    write_header(context, address, header)?;
    operation?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_fiemap_layout_matches_x86_64_uapi() {
        assert_eq!(size_of::<LinuxFiemap>(), 32);
        assert_eq!(size_of::<LinuxFiemapExtent>(), 56);
        assert_eq!(core::mem::offset_of!(LinuxFiemap, fm_extent_count), 24);
        assert_eq!(core::mem::offset_of!(LinuxFiemapExtent, fe_flags), 40);
    }

    #[test]
    fn extent_address_is_checked_and_starts_after_header() {
        assert_eq!(extent_user_address(0x1000, 0), Ok(0x1020));
        assert_eq!(extent_user_address(0x1000, 3), Ok(0x10c8));
        assert_eq!(
            extent_user_address(usize::MAX - 8, 0),
            Err(AxError::BadAddress)
        );
    }

    #[test]
    fn supported_flags_and_capacity_match_linux_contract() {
        assert_eq!(FIEMAP_SUPPORTED_FLAGS, FIEMAP_FLAG_SYNC);
        assert_eq!(axfs_ng_vfs::FILE_EXTENT_MAX, 4096);
        assert_eq!(FIEMAP_MAX_EXTENTS, u32::MAX / 56);
    }

    #[test]
    fn maxbytes_is_checked_before_empty_or_capacity_queries() {
        let header = LinuxFiemap {
            fm_start: FIEMAP_MAX_BYTES,
            fm_length: 0,
            fm_extent_count: (axfs_ng_vfs::FILE_EXTENT_MAX + 1) as u32,
            ..LinuxFiemap::default()
        };
        assert_eq!(
            validate_fiemap_request(&header),
            Err(AxError::from(LinuxError::EFBIG))
        );
    }

    #[test]
    fn capacity_above_internal_bound_is_an_explicit_error() {
        let header = LinuxFiemap {
            fm_extent_count: (axfs_ng_vfs::FILE_EXTENT_MAX + 1) as u32,
            ..LinuxFiemap::default()
        };
        assert_eq!(validate_fiemap_request(&header), Err(AxError::InvalidInput));
    }

    #[test]
    fn overlong_range_is_clamped_after_a_valid_start() {
        let full_range = LinuxFiemap {
            fm_length: u64::MAX,
            ..LinuxFiemap::default()
        };
        assert_eq!(validate_fiemap_request(&full_range), Ok(FIEMAP_MAX_BYTES));
        assert_eq!(full_range.fm_length, u64::MAX);

        let header = LinuxFiemap {
            fm_start: FIEMAP_MAX_BYTES - 1,
            fm_length: u64::MAX,
            ..LinuxFiemap::default()
        };
        assert_eq!(validate_fiemap_request(&header), Ok(1));
        assert_eq!(header.fm_length, u64::MAX);
    }
}
