//! ArceOS filesystem module.
//!
//! Provides high-level filesystem operations built on top of the VFS layer,
//! including file I/O with page caching, directory traversal, and
//! `std::fs`-like APIs.

#![cfg_attr(all(not(test), not(doc)), no_std)]
#![feature(allocator_api)]
#![feature(doc_cfg)]
#![allow(clippy::new_ret_no_self)]

extern crate alloc;

#[macro_use]
extern crate log;

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write as _,
    sync::atomic::{AtomicBool, Ordering},
};

use axdriver::{AxBlockDevice, AxDeviceContainer, SharedBlockDevice, prelude::*};
use axfs_ng_vfs::{Filesystem, WeakFilesystemIdentity};
use axsync::Mutex;
use spin::Once;

/// Attributes successful transfers to the task that issued backing I/O.
///
/// The filesystem crate cannot depend on the kernel task layer without
/// creating a cycle, so the native kernel supplies this narrow callback when
/// it enables `task-io-accounting`.
#[cfg(feature = "task-io-accounting")]
#[crate_interface::def_interface]
pub trait TaskIoAccounting {
    /// Accounts bytes read from a backing device or filesystem node.
    fn account_read(bytes: usize);
    /// Accounts bytes written to a backing device or filesystem node.
    fn account_write(bytes: usize);
}

#[cfg(feature = "task-io-accounting")]
#[inline]
pub(crate) fn account_backing_read(bytes: usize) {
    if bytes != 0 {
        crate_interface::call_interface!(TaskIoAccounting::account_read(bytes));
    }
}

#[cfg(feature = "task-io-accounting")]
#[inline]
pub(crate) fn account_backing_write(bytes: usize) {
    if bytes != 0 {
        crate_interface::call_interface!(TaskIoAccounting::account_write(bytes));
    }
}

#[cfg(not(feature = "task-io-accounting"))]
#[inline]
pub(crate) fn account_backing_read(_bytes: usize) {}

#[cfg(not(feature = "task-io-accounting"))]
#[inline]
pub(crate) fn account_backing_write(_bytes: usize) {}

mod fs;
#[cfg(feature = "btrfs")]
pub use fs::BtrfsFilesystem;
#[cfg(feature = "nfs41")]
pub use fs::nfs::*;
#[cfg(feature = "overlay")]
pub use fs::overlay::{
    IdentityOverlayIdMapper, OVERLAY_MAX_LAYERS, OverlayCopyUpBackend, OverlayFeatures,
    OverlayFilesystem, OverlayIdMapper, OverlayMountOptions, OverlayTopology, OverlayWriteBackend,
    VfsOverlayWriteBackend, copy_up,
};
pub use fs::{
    FatMountOptions, drain_deferred_filesystem_finalizers, has_deferred_filesystem_finalizer_work,
    set_deferred_filesystem_finalizer_waker,
};
#[cfg(feature = "xfs")]
pub use fs::{
    XfsAgBtreeKind, XfsAgBtreeNode, XfsAgBtreeRecords, XfsAgFreeRecord, XfsAgFreelist,
    XfsAgInodeRecord, XfsAgOwnershipSnapshot, XfsAgf, XfsAgi, XfsAil, XfsAilEntry,
    XfsAllocationGroup, XfsBmapLocalMutation, XfsBmbtNode, XfsBmbtRoot, XfsBufferReplayItem,
    XfsDirectoryDataBlock, XfsDirectoryDataEntry, XfsDirectoryEntry, XfsDirectoryLeafBlock,
    XfsDirectoryLeafEntry, XfsDirtyMetadataBuffer, XfsDoneReplayItem, XfsDquotReplayItem, XfsError,
    XfsExportHandle, XfsExtent, XfsExtentAllocation, XfsFeatures, XfsFileAttr, XfsFilesystem,
    XfsForkFormat, XfsHomeWriteDescriptor, XfsInode, XfsInodeAllocation, XfsInodeReplayItem,
    XfsIntentKey, XfsIntentKind, XfsIntentRecovery, XfsIntentReplayItem, XfsJournalRecord,
    XfsJournalRecoveryState, XfsLogByteOrder, XfsLogFragment, XfsLogOperation, XfsLogRecordHeader,
    XfsLogReplayExtent, XfsLogReservation, XfsLogRing, XfsMetadataTransaction, XfsMount,
    XfsMountMembers, XfsNode, XfsPhysicalLogScan, XfsPreparedLogCommit, XfsQuotaRoots,
    XfsRecoveryCommit, XfsRecoveryPlan, XfsRecoveryTransaction, XfsRegularWrite, XfsReplayItem,
    XfsResult, XfsShortformXattr, XfsSuperblock, XfsTransactionHeader, XfsUuid, XfsVolume,
};

mod highlevel;
pub use highlevel::*;
// `fs::nfs::*` above also exports an NFS-wire `ReadDirEntry`; pin the crate
// root name to the generic directory-stream entry.
pub use highlevel::ReadDirEntry;

/// Enables or disables filesystem I/O counters that are normally off.
pub fn set_io_stats_counters_enabled(enabled: bool) {
    set_cached_file_io_counters_enabled(enabled);
    #[cfg(feature = "ext4")]
    lwext4_rust::set_io_counters_enabled(enabled);
}

pub fn set_lwext4_async_mapped_read_enabled(enabled: bool) {
    #[cfg(feature = "ext4")]
    lwext4_rust::set_async_mapped_read_enabled(enabled);

    #[cfg(not(feature = "ext4"))]
    {
        let _ = enabled;
    }
}

/// Resets filesystem I/O counters.
pub fn reset_io_stats_counters() {
    reset_cached_file_io_counters();
    #[cfg(feature = "ext4")]
    lwext4_rust::reset_io_counters();
}

/// Renders filesystem I/O counters in a stable line-oriented format.
pub fn render_io_stats_counters() -> String {
    let cached = cached_file_io_counters_snapshot();
    let mut out = String::new();

    let _ = writeln!(
        out,
        "cached.read_bypass_eligible {}",
        cached.read_bypass_eligible
    );
    let _ = writeln!(out, "cached.read_bypass_hits {}", cached.read_bypass_hits);
    let _ = writeln!(out, "cached.read_bypass_bytes {}", cached.read_bypass_bytes);
    let _ = writeln!(
        out,
        "cached.read_bypass_slice_hits {}",
        cached.read_bypass_slice_hits
    );
    let _ = writeln!(
        out,
        "cached.read_bypass_slice_bytes {}",
        cached.read_bypass_slice_bytes
    );
    let _ = writeln!(
        out,
        "cached.read_bypass_reject_in_memory {}",
        cached.read_bypass_reject_in_memory
    );
    let _ = writeln!(
        out,
        "cached.read_bypass_reject_unaligned {}",
        cached.read_bypass_reject_unaligned
    );
    let _ = writeln!(
        out,
        "cached.read_bypass_reject_cached {}",
        cached.read_bypass_reject_cached
    );
    let _ = writeln!(
        out,
        "cached.read_bypass_eof_races {}",
        cached.read_bypass_eof_races
    );
    let _ = writeln!(
        out,
        "cached.write_bypass_eligible {}",
        cached.write_bypass_eligible
    );
    let _ = writeln!(out, "cached.write_bypass_hits {}", cached.write_bypass_hits);
    let _ = writeln!(
        out,
        "cached.write_bypass_bytes {}",
        cached.write_bypass_bytes
    );
    let _ = writeln!(
        out,
        "cached.write_bypass_slice_hits {}",
        cached.write_bypass_slice_hits
    );
    let _ = writeln!(
        out,
        "cached.write_bypass_slice_bytes {}",
        cached.write_bypass_slice_bytes
    );
    let _ = writeln!(
        out,
        "cached.write_bypass_reject_in_memory {}",
        cached.write_bypass_reject_in_memory
    );
    let _ = writeln!(
        out,
        "cached.write_bypass_reject_unaligned {}",
        cached.write_bypass_reject_unaligned
    );
    let _ = writeln!(
        out,
        "cached.write_no_read_insert_pages {}",
        cached.write_no_read_insert_pages
    );
    let _ = writeln!(
        out,
        "cached.write_no_read_insert_bytes {}",
        cached.write_no_read_insert_bytes
    );
    let _ = writeln!(out, "cached.flush_dirty_pages {}", cached.flush_dirty_pages);
    let _ = writeln!(out, "cached.flush_bytes {}", cached.flush_bytes);
    let _ = writeln!(
        out,
        "cached.range_flush_dirty_pages {}",
        cached.range_flush_dirty_pages
    );
    let _ = writeln!(out, "cached.range_flush_bytes {}", cached.range_flush_bytes);
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_hits {}",
        cached.async_dirty_flush_hits
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_pages {}",
        cached.async_dirty_flush_pages
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_bytes {}",
        cached.async_dirty_flush_bytes
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_errors {}",
        cached.async_dirty_flush_errors
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_sg_enabled {}",
        cached.async_dirty_flush_sg_enabled
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_sg_hits {}",
        cached.async_dirty_flush_sg_hits
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_sg_segments {}",
        cached.async_dirty_flush_sg_segments
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_sg_async_submit_hits {}",
        cached.async_dirty_flush_sg_async_submit_hits
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_sg_async_submit_segments {}",
        cached.async_dirty_flush_sg_async_submit_segments
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_bounce_fallbacks {}",
        cached.async_dirty_flush_bounce_fallbacks
    );
    let _ = writeln!(
        out,
        "cached.async_dirty_flush_writeback_restarts {}",
        cached.async_dirty_flush_writeback_restarts
    );
    let _ = writeln!(out, "cached.readahead_enabled {}", cached.readahead_enabled);
    let _ = writeln!(
        out,
        "cached.readahead_window_pages {}",
        cached.readahead_window_pages
    );
    let _ = writeln!(out, "cached.readahead_misses {}", cached.readahead_misses);
    let _ = writeln!(out, "cached.readahead_windows {}", cached.readahead_windows);
    let _ = writeln!(out, "cached.readahead_pages {}", cached.readahead_pages);
    let _ = writeln!(out, "cached.readahead_hits {}", cached.readahead_hits);
    let _ = writeln!(
        out,
        "cached.readahead_pressure_skips {}",
        cached.readahead_pressure_skips
    );
    let _ = writeln!(
        out,
        "cached.readahead_retired_unused_pages {}",
        cached.readahead_retired_unused_pages
    );
    let _ = writeln!(
        out,
        "cached.sync_data_only_requests {}",
        cached.sync_data_only_requests
    );
    let _ = writeln!(
        out,
        "cached.sync_metadata_requests {}",
        cached.sync_metadata_requests
    );
    let _ = writeln!(
        out,
        "cached.sync_data_only_metadata_fallbacks {}",
        cached.sync_data_only_metadata_fallbacks
    );
    let _ = writeln!(
        out,
        "cached.range_invalidate_pages {}",
        cached.range_invalidate_pages
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_retain_attempts {}",
        cached.closed_cache_retain_attempts
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_retain_hits {}",
        cached.closed_cache_retain_hits
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_retain_pages {}",
        cached.closed_cache_retain_pages
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_retain_reject_pages {}",
        cached.closed_cache_retain_reject_pages
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_reopen_hits {}",
        cached.closed_cache_reopen_hits
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_retain_releases {}",
        cached.closed_cache_retain_releases
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_trim_releases {}",
        cached.closed_cache_trim_releases
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_trim_pages {}",
        cached.closed_cache_trim_pages
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_trim_flush_errors {}",
        cached.closed_cache_trim_flush_errors
    );
    let _ = writeln!(
        out,
        "cached.closed_cache_retained_pages_current {}",
        cached.closed_cache_retained_pages_current
    );

    #[cfg(feature = "ext4")]
    {
        let ext4 = lwext4_rust::io_counters_snapshot();
        let _ = writeln!(out, "ext4.hot_inode_hits {}", ext4.hot_inode_hits);
        let _ = writeln!(out, "ext4.hot_inode_misses {}", ext4.hot_inode_misses);
        let _ = writeln!(out, "ext4.hot_inode_evictions {}", ext4.hot_inode_evictions);
        let _ = writeln!(out, "ext4.hot_inode_drains {}", ext4.hot_inode_drains);
        let _ = writeln!(out, "ext4.inode_ref_gets {}", ext4.inode_ref_gets);
        let _ = writeln!(
            out,
            "ext4.extent_get_blocks_calls {}",
            ext4.extent_get_blocks_calls
        );
        let _ = writeln!(
            out,
            "ext4.extent_get_blocks_requested {}",
            ext4.extent_get_blocks_requested
        );
        let _ = writeln!(
            out,
            "ext4.extent_get_blocks_returned {}",
            ext4.extent_get_blocks_returned
        );
        let _ = writeln!(
            out,
            "ext4.extent_get_blocks_create_calls {}",
            ext4.extent_get_blocks_create_calls
        );
        let _ = writeln!(out, "ext4.legacy_dblk_lookups {}", ext4.legacy_dblk_lookups);
        let _ = writeln!(out, "ext4.extent_status_hits {}", ext4.extent_status_hits);
        let _ = writeln!(
            out,
            "ext4.extent_status_misses {}",
            ext4.extent_status_misses
        );
        let _ = writeln!(
            out,
            "ext4.extent_status_inserts {}",
            ext4.extent_status_inserts
        );
        let _ = writeln!(
            out,
            "ext4.extent_status_invalidations {}",
            ext4.extent_status_invalidations
        );
        let _ = writeln!(
            out,
            "ext4.extent_status_reclaims {}",
            ext4.extent_status_reclaims
        );
        let _ = writeln!(out, "ext4.mapped_read_runs {}", ext4.mapped_read_runs);
        let _ = writeln!(out, "ext4.mapped_read_bytes {}", ext4.mapped_read_bytes);
        let _ = writeln!(
            out,
            "ext4.mapped_overwrite_hits {}",
            ext4.mapped_overwrite_hits
        );
        let _ = writeln!(
            out,
            "ext4.mapped_overwrite_misses {}",
            ext4.mapped_overwrite_misses
        );
        let _ = writeln!(
            out,
            "ext4.mapped_overwrite_bytes {}",
            ext4.mapped_overwrite_bytes
        );
        let _ = writeln!(
            out,
            "ext4.mapped_read_vectored_runs {}",
            ext4.mapped_read_vectored_runs
        );
        let _ = writeln!(
            out,
            "ext4.mapped_read_vectored_bytes {}",
            ext4.mapped_read_vectored_bytes
        );
        let _ = writeln!(
            out,
            "ext4.mapped_overwrite_vectored_hits {}",
            ext4.mapped_overwrite_vectored_hits
        );
        let _ = writeln!(
            out,
            "ext4.mapped_overwrite_vectored_bytes {}",
            ext4.mapped_overwrite_vectored_bytes
        );
        let _ = writeln!(
            out,
            "ext4.async_mapped_read_enabled {}",
            ext4.async_mapped_read_enabled
        );
        let _ = writeln!(
            out,
            "ext4.async_mapped_read_hits {}",
            ext4.async_mapped_read_hits
        );
        let _ = writeln!(
            out,
            "ext4.async_mapped_read_runs {}",
            ext4.async_mapped_read_runs
        );
        let _ = writeln!(
            out,
            "ext4.async_mapped_read_bytes {}",
            ext4.async_mapped_read_bytes
        );
        let _ = writeln!(
            out,
            "ext4.async_mapped_read_submit_batches {}",
            ext4.async_mapped_read_submit_batches
        );
        let _ = writeln!(
            out,
            "ext4.async_mapped_read_fallbacks {}",
            ext4.async_mapped_read_fallbacks
        );
        let _ = writeln!(
            out,
            "ext4.async_mapped_read_cookie_rejects {}",
            ext4.async_mapped_read_cookie_rejects
        );
        let _ = writeln!(
            out,
            "ext4.readahead_async_pages {}",
            ext4.readahead_async_pages
        );
        let _ = writeln!(
            out,
            "ext4.readahead_async_hits {}",
            ext4.readahead_async_hits
        );
    }

    out
}

struct RegisteredBlockDevice {
    name: String,
    device: SharedBlockDevice,
    info: BlockDeviceInfo,
    read_only: AtomicBool,
    mounted: Arc<AtomicBool>,
}

#[derive(Debug)]
pub enum OpenBlockDeviceError {
    NotFound,
    Busy,
}

/// A block-device mount claim paired with a shared driver handle.
///
/// Raw device users may clone the underlying handle concurrently, while only
/// one filesystem mount may hold this claim. Dropping the filesystem releases
/// the claim, including failure paths during filesystem construction.
pub struct MountedBlockDevice {
    device: SharedBlockDevice,
    mounted: Arc<AtomicBool>,
    // The registry refuses a read-only transition while a claim is live, so
    // this mount-time snapshot remains authoritative for the claim lifetime.
    read_only: bool,
}

impl MountedBlockDevice {
    pub(crate) fn device(&self) -> &SharedBlockDevice {
        &self.device
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only
    }
}

impl Drop for MountedBlockDevice {
    fn drop(&mut self) {
        self.mounted.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockDeviceInfo {
    pub num_blocks: u64,
    pub block_size: usize,
}

impl BlockDeviceInfo {
    pub fn byte_len(self) -> u64 {
        self.num_blocks.saturating_mul(self.block_size as u64)
    }
}

static EXTRA_BLOCK_DEVICES: Once<Mutex<Vec<RegisteredBlockDevice>>> = Once::new();
static ROOT_BLOCK_DEVICE: Once<RegisteredBlockDevice> = Once::new();

/// Exact filesystem-to-queue bindings used by physical I/O admission. The
/// table is intentionally tiny and fixed-capacity: lookup is only on the
/// submitter admission path, while completion routing uses the lower shared
/// device's opaque identity directly. A weak filesystem identity lets normal
/// unmount teardown reclaim a slot without coupling queue lifetime to a VFS
/// path or device-name string.
const MAX_FILESYSTEM_BLOCK_BINDINGS: usize = 32;

struct FilesystemBlockBinding {
    filesystem: WeakFilesystemIdentity,
    device: SharedBlockDevice,
}

static FILESYSTEM_BLOCK_BINDINGS: Once<Mutex<Vec<FilesystemBlockBinding>>> = Once::new();

fn register_filesystem_block_binding(
    filesystem: &Filesystem,
    device: &SharedBlockDevice,
) -> axfs_ng_vfs::VfsResult<()> {
    let bindings = FILESYSTEM_BLOCK_BINDINGS.call_once(|| Mutex::new(Vec::new()));
    let mut bindings = bindings.lock();
    bindings.retain(|binding| binding.filesystem.upgrade().is_some());
    if bindings.iter().any(|binding| {
        binding
            .filesystem
            .upgrade()
            .is_some_and(|identity| identity.device() == filesystem.device())
    }) {
        return Ok(());
    }
    if bindings.len() >= MAX_FILESYSTEM_BLOCK_BINDINGS {
        return Err(axfs_ng_vfs::VfsError::NoMemory);
    }
    bindings
        .try_reserve(1)
        .map_err(|_| axfs_ng_vfs::VfsError::NoMemory)?;
    bindings.push(FilesystemBlockBinding {
        filesystem: filesystem.identity_weak(),
        device: device.clone(),
    });
    Ok(())
}

/// Resolves the exact block queue backing one mounted filesystem identity.
/// This is intentionally not a pathname or `/dev/vd*` authorization check.
pub fn block_device_for_filesystem(vfs_device: u64) -> Option<SharedBlockDevice> {
    let bindings = FILESYSTEM_BLOCK_BINDINGS.get()?;
    let mut bindings = bindings.lock();
    bindings.retain(|binding| binding.filesystem.upgrade().is_some());
    bindings.iter().find_map(|binding| {
        binding
            .filesystem
            .upgrade()
            .filter(|identity| identity.device() == vfs_device)
            .map(|_| binding.device.clone())
    })
}

pub const ROOT_BLOCK_DEVICE_NAME: &str = "vda";

fn extra_device_name(index: usize) -> String {
    let letter = (b'a' + index as u8) as char;
    format!("vd{letter}")
}

pub fn block_device_names() -> Vec<String> {
    EXTRA_BLOCK_DEVICES
        .get()
        .map(|devices| {
            devices
                .lock()
                .iter()
                .map(|entry| entry.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub fn block_device_info(name: &str) -> Option<BlockDeviceInfo> {
    if name == ROOT_BLOCK_DEVICE_NAME {
        return ROOT_BLOCK_DEVICE.get().map(|entry| entry.info);
    }
    let devices = EXTRA_BLOCK_DEVICES.get()?;
    devices
        .lock()
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.info)
}

pub fn root_block_device_info() -> Option<BlockDeviceInfo> {
    ROOT_BLOCK_DEVICE.get().map(|entry| entry.info)
}

pub fn block_device_is_read_only(name: &str) -> Option<bool> {
    if name == ROOT_BLOCK_DEVICE_NAME {
        return ROOT_BLOCK_DEVICE
            .get()
            .map(|entry| entry.read_only.load(Ordering::Acquire));
    }
    let devices = EXTRA_BLOCK_DEVICES.get()?;
    devices
        .lock()
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.read_only.load(Ordering::Acquire))
}

pub fn set_block_device_read_only(name: &str, read_only: bool) -> Result<(), OpenBlockDeviceError> {
    if name == ROOT_BLOCK_DEVICE_NAME {
        let entry = ROOT_BLOCK_DEVICE
            .get()
            .ok_or(OpenBlockDeviceError::NotFound)?;
        if entry.read_only.load(Ordering::Acquire) != read_only
            && entry.mounted.load(Ordering::Acquire)
        {
            return Err(OpenBlockDeviceError::Busy);
        }
        entry.read_only.store(read_only, Ordering::Release);
        return Ok(());
    }
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(OpenBlockDeviceError::NotFound)?;
    let devices = devices.lock();
    let entry = devices
        .iter()
        .find(|entry| entry.name == name)
        .ok_or(OpenBlockDeviceError::NotFound)?;
    if entry.read_only.load(Ordering::Acquire) != read_only && entry.mounted.load(Ordering::Acquire)
    {
        return Err(OpenBlockDeviceError::Busy);
    }
    entry.read_only.store(read_only, Ordering::Release);
    Ok(())
}

fn claim_block_device(
    entry: &RegisteredBlockDevice,
) -> Result<MountedBlockDevice, OpenBlockDeviceError> {
    entry
        .mounted
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| OpenBlockDeviceError::Busy)?;
    Ok(MountedBlockDevice {
        device: entry.device.clone(),
        mounted: entry.mounted.clone(),
        read_only: entry.read_only.load(Ordering::Acquire),
    })
}

/// Opens a block device for a filesystem mount.
pub fn open_block_device(name: &str) -> Result<MountedBlockDevice, OpenBlockDeviceError> {
    if name == ROOT_BLOCK_DEVICE_NAME {
        let entry = ROOT_BLOCK_DEVICE
            .get()
            .ok_or(OpenBlockDeviceError::NotFound)?;
        return claim_block_device(entry);
    }
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(OpenBlockDeviceError::NotFound)?;
    let devices = devices.lock();
    let entry = devices
        .iter()
        .find(|entry| entry.name == name)
        .ok_or(OpenBlockDeviceError::NotFound)?;
    claim_block_device(entry)
}

/// Clones a raw-access handle without taking a filesystem mount claim.
pub fn raw_block_device(name: &str) -> Result<SharedBlockDevice, OpenBlockDeviceError> {
    if name == ROOT_BLOCK_DEVICE_NAME {
        return ROOT_BLOCK_DEVICE
            .get()
            .map(|entry| entry.device.clone())
            .ok_or(OpenBlockDeviceError::NotFound);
    }
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(OpenBlockDeviceError::NotFound)?;
    devices
        .lock()
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.device.clone())
        .ok_or(OpenBlockDeviceError::NotFound)
}

pub fn with_block_device_mut<R>(
    name: &str,
    f: impl FnOnce(&mut SharedBlockDevice) -> R,
) -> Result<R, OpenBlockDeviceError> {
    if name == ROOT_BLOCK_DEVICE_NAME {
        let entry = ROOT_BLOCK_DEVICE
            .get()
            .ok_or(OpenBlockDeviceError::NotFound)?;
        if entry.mounted.load(Ordering::Acquire) {
            return Err(OpenBlockDeviceError::Busy);
        }
        let mut device = entry.device.clone();
        return Ok(f(&mut device));
    }
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(OpenBlockDeviceError::NotFound)?;
    let devices = devices.lock();
    let entry = devices
        .iter()
        .find(|entry| entry.name == name)
        .ok_or(OpenBlockDeviceError::NotFound)?;
    if entry.mounted.load(Ordering::Acquire) {
        return Err(OpenBlockDeviceError::Busy);
    }
    let mut device = entry.device.clone();
    Ok(f(&mut device))
}

pub fn new_block_filesystem(
    fs_type: &str,
    dev: MountedBlockDevice,
) -> axfs_ng_vfs::VfsResult<axfs_ng_vfs::Filesystem> {
    let device = dev.device.clone();
    let filesystem = fs::new_named(fs_type, dev, None)?;
    register_filesystem_block_binding(&filesystem, &device)?;
    Ok(filesystem)
}

pub fn new_block_filesystem_with_fat_options(
    fs_type: &str,
    dev: MountedBlockDevice,
    options: FatMountOptions,
) -> axfs_ng_vfs::VfsResult<axfs_ng_vfs::Filesystem> {
    let device = dev.device.clone();
    let filesystem = fs::new_named(fs_type, dev, Some(options))?;
    register_filesystem_block_binding(&filesystem, &device)?;
    Ok(filesystem)
}

/// Opens a Btrfs filesystem from an already claimed, ordered member set.
///
/// The Btrfs adapter retains every claim for the lifetime of the resulting
/// superblock.  Bind physical-I/O admission to the first (mount source)
/// member, matching the Linux device identity recorded by the mount layer;
/// member discovery never has to reparse mount-option strings during unmount.
pub fn new_btrfs_filesystem_with_members(
    members: Vec<MountedBlockDevice>,
) -> axfs_ng_vfs::VfsResult<axfs_ng_vfs::Filesystem> {
    #[cfg(feature = "btrfs")]
    {
        let source = members
            .first()
            .ok_or(axfs_ng_vfs::VfsError::NoSuchDevice)?
            .device
            .clone();
        let filesystem = fs::btrfs::BtrfsFilesystem::new_multi(members)?;
        register_filesystem_block_binding(&filesystem, &source)?;
        return Ok(filesystem);
    }
    #[cfg(not(feature = "btrfs"))]
    {
        let _ = members;
        Err(axfs_ng_vfs::VfsError::NoSuchDevice)
    }
}

/// Initializes the filesystem subsystem using the first available block device.
pub fn init_filesystems(mut block_devs: AxDeviceContainer<AxBlockDevice>) {
    info!("Initialize filesystem subsystem...");
    debug!(
        "axfs init_filesystems: detected {} block device(s)",
        block_devs.len()
    );
    for (index, dev) in block_devs.iter().enumerate() {
        debug!(
            "axfs init_filesystems: block[{index}] device_name={}, blocks={}",
            dev.device_name(),
            dev.num_blocks()
        );
    }

    assert!(!block_devs.is_empty(), "No block device found!");
    // Device discovery order is the boot contract: a staged Multiboot module,
    // or else the runner's rootfs image (vda), precedes any data image (vdb).
    // Do not infer root identity from capacity; a perfectly valid data disk
    // may be larger than the rootfs image.
    let root_index = 0;
    let dev = block_devs.remove(root_index);
    if axdriver::block_device_is_read_only(&dev) {
        init_filesystems_with_root_read_only(dev, block_devs);
    } else {
        init_filesystems_with_root(dev, block_devs);
    }
}

/// Initializes filesystems with an explicitly supplied root block device.
///
/// This keeps the ordinary discovered-device order unchanged while allowing a
/// platform-owned, bootloader-supplied block image to become the root without
/// adding a second mount path.
pub fn init_filesystems_with_root(
    dev: AxBlockDevice,
    block_devs: AxDeviceContainer<AxBlockDevice>,
) {
    init_filesystems_with_root_mode(dev, block_devs, false);
}

/// Like [`init_filesystems_with_root`], but publishes immutable boot media as
/// read-only in the established block-device registry.
pub fn init_filesystems_with_root_read_only(
    dev: AxBlockDevice,
    block_devs: AxDeviceContainer<AxBlockDevice>,
) {
    init_filesystems_with_root_mode(dev, block_devs, true);
}

fn init_filesystems_with_root_mode(
    dev: AxBlockDevice,
    mut block_devs: AxDeviceContainer<AxBlockDevice>,
    root_read_only: bool,
) {
    let root_index = 0;
    info!(
        "  use block device {root_index}: {:?} (blocks={})",
        dev.device_name(),
        dev.num_blocks()
    );

    let root_device = SharedBlockDevice::new(dev);
    ROOT_BLOCK_DEVICE.call_once(|| RegisteredBlockDevice {
        name: ROOT_BLOCK_DEVICE_NAME.into(),
        info: BlockDeviceInfo {
            num_blocks: root_device.num_blocks(),
            block_size: root_device.block_size(),
        },
        read_only: AtomicBool::new(root_read_only),
        mounted: Arc::new(AtomicBool::new(false)),
        device: root_device,
    });

    let mut extras = Vec::new();
    let mut index = 1;
    while !block_devs.is_empty() {
        let dev = block_devs.remove(0);
        debug!(
            "axfs init_filesystems: registering extra block device {} as {}",
            dev.device_name(),
            extra_device_name(index)
        );
        let device = SharedBlockDevice::new(dev);
        extras.push(RegisteredBlockDevice {
            name: extra_device_name(index),
            info: BlockDeviceInfo {
                num_blocks: device.num_blocks(),
                block_size: device.block_size(),
            },
            read_only: AtomicBool::new(false),
            mounted: Arc::new(AtomicBool::new(false)),
            device,
        });
        index += 1;
    }
    EXTRA_BLOCK_DEVICES.call_once(|| Mutex::new(extras));

    let root_device = open_block_device(ROOT_BLOCK_DEVICE_NAME)
        .expect("failed to claim root block device for filesystem mount");
    let fs = fs::new_default(root_device).expect("Failed to initialize filesystem");
    let root_device = raw_block_device(ROOT_BLOCK_DEVICE_NAME)
        .expect("root block device disappeared during filesystem initialization");
    register_filesystem_block_binding(&fs, &root_device)
        .expect("failed to bind root filesystem to its block device");
    info!("  filesystem type: {:?}", fs.name());

    // Linux `init_mount_tree()` (`fs/namespace.c`:6180-6230) builds the initial
    // mount namespace from two mounts: the immutable `nullfs` (mount ID 1)
    // becomes `init_mnt_ns.root`, and the mutable `rootfs` (mount ID 2) is
    // attached on top of it.  `current->fs->root` is the rootfs, so "/" still
    // resolves to the mutable tree while the absolute root underneath stays
    // permanently in place.
    let nullfs = axfs_ng_vfs::nullfs::filesystem().expect("Failed to create nullfs");
    let namespace_root = axfs_ng_vfs::Mountpoint::new_root(&nullfs);
    // The extension slot must stay free: the kernel installs this mount's
    // `LinuxMountState` (mount ID 2) when it publishes the initial ledger.
    let rootfs = axfs_ng_vfs::Mountpoint::new_detached_uninitialized(&fs)
        .expect("Failed to allocate the mutable rootfs mount");
    rootfs
        .attach_to(&namespace_root.root_location())
        .expect("VFS: Failed to mount rootfs on nullfs");

    let root_context = FsContext::new(rootfs.root_location());
    ROOT_FS_CONTEXT.call_once(|| root_context.clone());
    let shared = Arc::try_new(Mutex::new(root_context))
        .expect("Failed to allocate root filesystem scope context");
    ROOT_FS_SCOPE_CONTEXT.call_once(|| shared);
}
