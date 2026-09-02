//! Linux `FS_IOC_FIEMAP` adapter for regular files.
//!
//! Raw UAPI parsing and userspace access stay in this module.  Filesystems
//! receive only a typed extent query and return owned metadata, so neither an
//! ext4 lock nor a cache-range owner is retained across a userspace fault.

use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use linux_raw_sys::ioctl::FS_IOC_FIEMAP;
use linux_vfs::{
    FIEMAP_STREAM_BATCH_EXTENTS, Fiemap, FiemapExtent, FiemapExtentState, FiemapRequestError,
};

use super::IoctlContext;
use crate::mm::map_usercopy_error;

#[inline]
pub(super) const fn is_fiemap_command(command: u32) -> bool {
    command == FS_IOC_FIEMAP
}

fn extent_user_address(base: usize, index: usize) -> AxResult<usize> {
    index
        .checked_mul(size_of::<FiemapExtent>())
        .and_then(|offset| size_of::<Fiemap>().checked_add(offset))
        .and_then(|offset| base.checked_add(offset))
        .ok_or(AxError::BadAddress)
}

fn write_header(context: &IoctlContext, address: usize, header: Fiemap) -> AxResult<()> {
    context
        .user_memory()
        .write_value(address as *mut Fiemap, header)
        .map_err(map_usercopy_error)
}

fn write_extent(
    context: &IoctlContext,
    base: usize,
    index: usize,
    extent: FiemapExtent,
) -> AxResult<()> {
    let address = extent_user_address(base, index)?;
    context
        .user_memory()
        .write_value(address as *mut FiemapExtent, extent)
        .map_err(map_usercopy_error)
}

fn validate_extent_count(header: &Fiemap) -> AxResult<()> {
    header.validate_extent_count().map_err(|error| match error {
        FiemapRequestError::ExtentCapacityTooLarge => AxError::InvalidInput,
        _ => unreachable!("extent count validation has one failure mode"),
    })
}

fn prepare_fiemap_request(header: &mut Fiemap, max_bytes: u64) -> AxResult<u64> {
    header.prepare(max_bytes).map_err(|error| match error {
        FiemapRequestError::ZeroLength => AxError::InvalidInput,
        FiemapRequestError::StartPastMaximum => AxError::from(LinuxError::EFBIG),
        FiemapRequestError::UnsupportedFlags => AxError::from(LinuxError::EBADR),
        FiemapRequestError::ExtentCapacityTooLarge => unreachable!("validated before preparation"),
    })
}

/// Executes `FS_IOC_FIEMAP` for one regular open file description.
///
/// Linux copies the header back even when the filesystem reports an error;
/// in particular unsupported flag bits are returned in `fm_flags` with
/// `EBADR`.  Extents are copied before the final header, matching the kernel's
/// observable partial-copy ordering on `EFAULT`.
pub(super) fn ioctl(file: &axfs::File, context: &IoctlContext, address: usize) -> AxResult<usize> {
    // Match Linux's inode-operation dispatch: unsupported files fail before
    // any access to the ioctl argument, including a bad user pointer.
    if !file.supports_extent_mapping()? {
        return Err(AxError::OperationNotSupported);
    }

    let mut header = context
        .user_memory()
        .read_value(address as *const Fiemap)
        .map_err(map_usercopy_error)?;

    // Linux rejects an unaddressable flexible array before invoking the
    // filesystem and does not copy the fixed header back in that case.
    validate_extent_count(&header)?;

    let operation = match file.max_extent_bytes() {
        Err(error) => Err(error),
        Ok(max_bytes) => match prepare_fiemap_request(&mut header, max_bytes) {
            Err(error) => Err(error),
            Ok(query_length) => stream_extents(file, context, address, &header, query_length),
        },
    };

    let operation = match operation {
        Ok(mapped) => {
            header.fm_mapped_extents = mapped;
            Ok(())
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

/// Streams a FIEMAP request through bounded AX extent batches. No batch keeps
/// more than `FIEMAP_STREAM_BATCH_EXTENTS` entries, regardless of the user
/// supplied flexible-array capacity.
fn stream_extents(
    file: &axfs::File,
    context: &IoctlContext,
    address: usize,
    header: &Fiemap,
    query_length: u64,
) -> AxResult<u32> {
    let query_end = header
        .fm_start
        .checked_add(query_length)
        .ok_or(AxError::InvalidInput)?;
    let mut cursor = header.fm_start;
    let mut mapped = 0u32;
    let mut first_batch = true;

    loop {
        let batch_capacity = stream_batch_capacity(header.fm_extent_count, mapped);
        if header.fm_extent_count != 0 && batch_capacity == 0 {
            return Ok(mapped);
        }
        let batch = file.map_extents(
            cursor,
            query_end - cursor,
            batch_capacity,
            first_batch && header.is_sync(),
        )?;
        first_batch = false;
        if header.fm_extent_count == 0 {
            mapped = mapped
                .checked_add(batch.mapped_extents)
                .ok_or(AxError::InvalidInput)?;
        } else {
            let copy_count = batch.extents.len().min(batch_capacity);
            let last_index = copy_count.saturating_sub(1);
            for (batch_index, extent) in batch.extents.iter().copied().enumerate().take(copy_count)
            {
                let state = match extent.state {
                    axfs_ng_vfs::FileExtentState::Written => FiemapExtentState::Written,
                    axfs_ng_vfs::FileExtentState::Unwritten => FiemapExtentState::Unwritten,
                };
                let raw = FiemapExtent::from_mapping(
                    extent.logical,
                    extent.physical,
                    extent.length,
                    state,
                    batch.complete && batch.reaches_eof && batch_index == last_index,
                );
                write_extent(context, address, mapped as usize, raw)?;
                mapped = mapped.checked_add(1).ok_or(AxError::InvalidInput)?;
            }
        }
        if batch.complete || batch.extents.is_empty() {
            return Ok(mapped);
        }
        let next = batch
            .extents
            .last()
            .and_then(|extent| extent.logical.checked_add(extent.length))
            .ok_or(AxError::InvalidInput)?;
        if next <= cursor || next >= query_end {
            return Ok(mapped);
        }
        cursor = next;
    }
}

const fn stream_batch_capacity(requested: u32, mapped: u32) -> usize {
    if requested == 0 {
        FIEMAP_STREAM_BATCH_EXTENTS
    } else {
        let remaining = requested.saturating_sub(mapped) as usize;
        if remaining < FIEMAP_STREAM_BATCH_EXTENTS {
            remaining
        } else {
            FIEMAP_STREAM_BATCH_EXTENTS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_fiemap_layout_matches_x86_64_uapi() {
        assert_eq!(size_of::<Fiemap>(), 32);
        assert_eq!(size_of::<FiemapExtent>(), 56);
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
    fn capacity_above_internal_bound_skips_preparation() {
        let header = Fiemap {
            fm_extent_count: linux_vfs::FIEMAP_MAX_EXTENTS + 1,
            ..Fiemap::default()
        };
        assert_eq!(validate_extent_count(&header), Err(AxError::InvalidInput));
    }

    #[test]
    fn preparation_matches_linux_error_order_and_copyback_state() {
        let mut zero_length = Fiemap {
            fm_start: 8,
            fm_length: 0,
            fm_flags: 2,
            ..Fiemap::default()
        };
        assert_eq!(
            prepare_fiemap_request(&mut zero_length, 8),
            Err(AxError::InvalidInput)
        );
        assert_eq!(zero_length.fm_flags, 2);

        let mut out_of_range = Fiemap {
            fm_start: 8,
            fm_length: 1,
            fm_flags: 2,
            ..Fiemap::default()
        };
        assert_eq!(
            prepare_fiemap_request(&mut out_of_range, 8),
            Err(AxError::from(LinuxError::EFBIG))
        );
        assert_eq!(out_of_range.fm_flags, 2);

        let mut invalid_flags = Fiemap {
            fm_length: 1,
            fm_flags: 2,
            ..Fiemap::default()
        };
        assert_eq!(
            prepare_fiemap_request(&mut invalid_flags, 8),
            Err(AxError::from(LinuxError::EBADR))
        );
        assert_eq!(invalid_flags.fm_flags, 2);

        let mut clipped = Fiemap {
            fm_start: 7,
            fm_length: u64::MAX,
            ..Fiemap::default()
        };
        assert_eq!(prepare_fiemap_request(&mut clipped, 8), Ok(1));
    }

    #[test]
    fn hostile_extent_capacity_never_expands_a_stream_batch() {
        assert_eq!(
            stream_batch_capacity(u32::MAX, 0),
            FIEMAP_STREAM_BATCH_EXTENTS
        );
        assert_eq!(stream_batch_capacity(3, 2), 1);
        assert_eq!(stream_batch_capacity(3, 3), 0);
        assert_eq!(stream_batch_capacity(0, 0), FIEMAP_STREAM_BATCH_EXTENTS);
    }
}
