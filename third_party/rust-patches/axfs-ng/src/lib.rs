//! ArceOS filesystem module.
//!
//! Provides high-level filesystem operations built on top of the VFS layer,
//! including file I/O with page caching, directory traversal, and
//! `std::fs`-like APIs.

#![cfg_attr(all(not(test), not(doc)), no_std)]
#![feature(doc_cfg)]
#![allow(clippy::new_ret_no_self)]

extern crate alloc;

#[macro_use]
extern crate log;

use alloc::{format, string::String, vec::Vec};
use core::{
    fmt::Write as _,
    sync::atomic::{AtomicBool, Ordering},
};

use axdriver::{AxBlockDevice, AxDeviceContainer, prelude::*};
use axsync::Mutex;
use spin::Once;

mod fs;

mod highlevel;
pub use highlevel::*;

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
    device: Option<AxBlockDevice>,
    info: BlockDeviceInfo,
    read_only: AtomicBool,
}

pub enum OpenBlockDeviceError {
    NotFound,
    Busy,
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
    let devices = EXTRA_BLOCK_DEVICES.get()?;
    devices
        .lock()
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.info)
}

pub fn block_device_is_read_only(name: &str) -> Option<bool> {
    let devices = EXTRA_BLOCK_DEVICES.get()?;
    devices
        .lock()
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.read_only.load(Ordering::Relaxed))
}

pub fn set_block_device_read_only(name: &str, read_only: bool) -> Result<(), OpenBlockDeviceError> {
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(OpenBlockDeviceError::NotFound)?;
    let devices = devices.lock();
    let entry = devices
        .iter()
        .find(|entry| entry.name == name)
        .ok_or(OpenBlockDeviceError::NotFound)?;
    entry.read_only.store(read_only, Ordering::Relaxed);
    Ok(())
}

pub fn open_block_device(name: &str) -> Result<AxBlockDevice, OpenBlockDeviceError> {
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(OpenBlockDeviceError::NotFound)?;
    let mut devices = devices.lock();
    let entry = devices
        .iter_mut()
        .find(|entry| entry.name == name)
        .ok_or(OpenBlockDeviceError::NotFound)?;
    entry.device.take().ok_or(OpenBlockDeviceError::Busy)
}

pub fn with_block_device_mut<R>(
    name: &str,
    f: impl FnOnce(&mut AxBlockDevice) -> R,
) -> Result<R, OpenBlockDeviceError> {
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(OpenBlockDeviceError::NotFound)?;
    let mut devices = devices.lock();
    let entry = devices
        .iter_mut()
        .find(|entry| entry.name == name)
        .ok_or(OpenBlockDeviceError::NotFound)?;
    let dev = entry.device.as_mut().ok_or(OpenBlockDeviceError::Busy)?;
    Ok(f(dev))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsyncBlockQueueSelftestError {
    NoBlockDevice,
    Unsupported,
    Io,
}

pub fn async_block_queue_read_write_selftest() -> Result<(), AsyncBlockQueueSelftestError> {
    let names = block_device_names();
    for name in names {
        let result = with_block_device_mut(&name, |dev| {
            let caps = dev
                .async_queue_caps()
                .ok_or(AsyncBlockQueueSelftestError::Unsupported)?;
            if caps.max_requests < 2 || caps.max_descriptors < 6 {
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }

            let block_size = dev.block_size();
            let request_bytes = block_size
                .checked_mul(2)
                .ok_or(AsyncBlockQueueSelftestError::Io)?;
            let blocks_per_request = request_bytes / block_size;
            let first_block = 16u64;
            let second_block = 32u64;
            if dev.num_blocks() <= second_block + blocks_per_request as u64 {
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }

            let write_a = Vec::from_iter(
                (0..request_bytes)
                    .map(|idx| (idx.wrapping_mul(37).wrapping_add(0x51) & 0xff) as u8),
            );
            let write_b = Vec::from_iter(
                (0..request_bytes)
                    .map(|idx| (idx.wrapping_mul(53).wrapping_add(0xa7) & 0xff) as u8),
            );

            let write_seg_a = [BlockSegment::from_write_buf(&write_a)];
            let write_seg_b = [BlockSegment::from_write_buf(&write_b)];
            let mut write_requests = [
                BlockQueueRequest {
                    op: BlockAsyncOp::Write,
                    block_id: first_block,
                    segments: &write_seg_a,
                    handle: None,
                },
                BlockQueueRequest {
                    op: BlockAsyncOp::Write,
                    block_id: second_block,
                    segments: &write_seg_b,
                    handle: None,
                },
            ];

            let write_report = dev
                .submit_async_batch(&mut write_requests)
                .map_err(|_| AsyncBlockQueueSelftestError::Unsupported)?;
            if write_report.submitted < 2 {
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }
            let write_handles = [
                write_requests[0]
                    .handle
                    .ok_or(AsyncBlockQueueSelftestError::Io)?,
                write_requests[1]
                    .handle
                    .ok_or(AsyncBlockQueueSelftestError::Io)?,
            ];
            dev.wait_async_all(&write_handles)
                .map_err(|_| AsyncBlockQueueSelftestError::Io)?;

            let mut read_a = Vec::from_iter(core::iter::repeat(0).take(request_bytes));
            let mut read_b = Vec::from_iter(core::iter::repeat(0).take(request_bytes));
            let read_seg_a = [BlockSegment::from_read_buf(&mut read_a)];
            let read_seg_b = [BlockSegment::from_read_buf(&mut read_b)];
            let mut read_requests = [
                BlockQueueRequest {
                    op: BlockAsyncOp::Read,
                    block_id: first_block,
                    segments: &read_seg_a,
                    handle: None,
                },
                BlockQueueRequest {
                    op: BlockAsyncOp::Read,
                    block_id: second_block,
                    segments: &read_seg_b,
                    handle: None,
                },
            ];

            let read_report = dev
                .submit_async_batch(&mut read_requests)
                .map_err(|_| AsyncBlockQueueSelftestError::Unsupported)?;
            if read_report.submitted < 2 {
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }
            let read_handles = [
                read_requests[0]
                    .handle
                    .ok_or(AsyncBlockQueueSelftestError::Io)?,
                read_requests[1]
                    .handle
                    .ok_or(AsyncBlockQueueSelftestError::Io)?,
            ];
            dev.wait_async_all(&read_handles)
                .map_err(|_| AsyncBlockQueueSelftestError::Io)?;

            if read_a != write_a || read_b != write_b {
                return Err(AsyncBlockQueueSelftestError::Io);
            }
            Ok(())
        });

        match result {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(AsyncBlockQueueSelftestError::Unsupported))
            | Err(OpenBlockDeviceError::Busy) => {
                continue;
            }
            Ok(Err(err)) => return Err(err),
            Err(OpenBlockDeviceError::NotFound) => continue,
        }
    }
    Err(AsyncBlockQueueSelftestError::NoBlockDevice)
}

pub fn async_block_queue_interrupt_selftest() -> Result<(), AsyncBlockQueueSelftestError> {
    let names = block_device_names();
    for name in names {
        let result = with_block_device_mut(&name, |dev| {
            let caps = dev
                .async_queue_caps()
                .ok_or(AsyncBlockQueueSelftestError::Unsupported)?;
            if caps.max_requests < 1 || caps.max_descriptors < 3 {
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }

            let block_size = dev.block_size();
            if block_size == 0 {
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }
            let first_block = 48u64;
            if dev.num_blocks() <= first_block + 1 {
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }

            let write_data = Vec::from_iter(
                (0..block_size).map(|idx| (idx.wrapping_mul(29).wrapping_add(0x3d) & 0xff) as u8),
            );
            let write_seg = [BlockSegment::from_write_buf(&write_data)];
            let mut write_requests = [BlockQueueRequest {
                op: BlockAsyncOp::Write,
                block_id: first_block,
                segments: &write_seg,
                handle: None,
            }];

            dev.enable_irq()
                .map_err(|_| AsyncBlockQueueSelftestError::Unsupported)?;
            let write_report = dev
                .submit_async_batch(&mut write_requests)
                .map_err(|_| AsyncBlockQueueSelftestError::Unsupported)?;
            if write_report.submitted < 1 {
                let _ = dev.disable_irq();
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }
            let write_handle = write_requests[0]
                .handle
                .ok_or(AsyncBlockQueueSelftestError::Io)?;
            let mut drained = 0usize;
            for _ in 0..4096 {
                drained = drained.saturating_add(
                    dev.handle_irq()
                        .map_err(|_| AsyncBlockQueueSelftestError::Io)?,
                );
                if drained >= 1 {
                    break;
                }
                core::hint::spin_loop();
            }
            if drained == 0 {
                let _ = dev.disable_irq();
                return Err(AsyncBlockQueueSelftestError::Io);
            }
            dev.wait_async_all(&[write_handle])
                .map_err(|_| AsyncBlockQueueSelftestError::Io)?;

            let mut read_data = Vec::from_iter(core::iter::repeat(0).take(block_size));
            let read_seg = [BlockSegment::from_read_buf(&mut read_data)];
            let mut read_requests = [BlockQueueRequest {
                op: BlockAsyncOp::Read,
                block_id: first_block,
                segments: &read_seg,
                handle: None,
            }];
            let read_report = dev
                .submit_async_batch(&mut read_requests)
                .map_err(|_| AsyncBlockQueueSelftestError::Unsupported)?;
            if read_report.submitted < 1 {
                let _ = dev.disable_irq();
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }
            let read_handle = read_requests[0]
                .handle
                .ok_or(AsyncBlockQueueSelftestError::Io)?;
            let mut drained = 0usize;
            for _ in 0..4096 {
                drained = drained.saturating_add(
                    dev.handle_irq()
                        .map_err(|_| AsyncBlockQueueSelftestError::Io)?,
                );
                if drained >= 1 {
                    break;
                }
                core::hint::spin_loop();
            }
            if drained == 0 {
                let _ = dev.disable_irq();
                return Err(AsyncBlockQueueSelftestError::Io);
            }
            dev.wait_async_all(&[read_handle])
                .map_err(|_| AsyncBlockQueueSelftestError::Io)?;
            dev.disable_irq()
                .map_err(|_| AsyncBlockQueueSelftestError::Io)?;

            if read_data != write_data {
                return Err(AsyncBlockQueueSelftestError::Io);
            }
            Ok(())
        });

        match result {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(AsyncBlockQueueSelftestError::Unsupported))
            | Err(OpenBlockDeviceError::Busy) => {
                continue;
            }
            Ok(Err(err)) => return Err(err),
            Err(OpenBlockDeviceError::NotFound) => continue,
        }
    }

    Err(AsyncBlockQueueSelftestError::NoBlockDevice)
}

pub fn async_block_queue_irq_first_wait_selftest() -> Result<(), AsyncBlockQueueSelftestError> {
    let names = block_device_names();
    for name in names {
        let result = with_block_device_mut(&name, |dev| {
            let caps = dev
                .async_queue_caps()
                .ok_or(AsyncBlockQueueSelftestError::Unsupported)?;
            if caps.max_requests < 1 || caps.max_descriptors < 3 {
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }

            let block_size = dev.block_size();
            let request_blocks = 4096usize;
            let request_bytes = block_size
                .checked_mul(request_blocks)
                .ok_or(AsyncBlockQueueSelftestError::Io)?;
            let first_block = 128u64;
            if block_size == 0 || dev.num_blocks() <= first_block + request_blocks as u64 {
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }

            let write_data = Vec::from_iter(
                (0..request_bytes)
                    .map(|idx| (idx.wrapping_mul(41).wrapping_add(0x9b) & 0xff) as u8),
            );
            let write_seg = [BlockSegment::from_write_buf(&write_data)];
            let mut write_requests = [BlockQueueRequest {
                op: BlockAsyncOp::Write,
                block_id: first_block,
                segments: &write_seg,
                handle: None,
            }];

            dev.enable_irq()
                .map_err(|_| AsyncBlockQueueSelftestError::Unsupported)?;
            let write_report = dev
                .submit_async_batch(&mut write_requests)
                .map_err(|_| AsyncBlockQueueSelftestError::Unsupported)?;
            if write_report.submitted < 1 {
                let _ = dev.disable_irq();
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }
            let write_handle = write_requests[0]
                .handle
                .ok_or(AsyncBlockQueueSelftestError::Io)?;
            dev.wait_async_all(&[write_handle])
                .map_err(|_| AsyncBlockQueueSelftestError::Io)?;

            let mut read_data = Vec::from_iter(core::iter::repeat(0).take(request_bytes));
            let read_seg = [BlockSegment::from_read_buf(&mut read_data)];
            let mut read_requests = [BlockQueueRequest {
                op: BlockAsyncOp::Read,
                block_id: first_block,
                segments: &read_seg,
                handle: None,
            }];
            let read_report = dev
                .submit_async_batch(&mut read_requests)
                .map_err(|_| AsyncBlockQueueSelftestError::Unsupported)?;
            if read_report.submitted < 1 {
                let _ = dev.disable_irq();
                return Err(AsyncBlockQueueSelftestError::Unsupported);
            }
            let read_handle = read_requests[0]
                .handle
                .ok_or(AsyncBlockQueueSelftestError::Io)?;
            dev.wait_async_all(&[read_handle])
                .map_err(|_| AsyncBlockQueueSelftestError::Io)?;
            dev.disable_irq()
                .map_err(|_| AsyncBlockQueueSelftestError::Io)?;

            if read_data != write_data {
                return Err(AsyncBlockQueueSelftestError::Io);
            }
            Ok(())
        });

        match result {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(AsyncBlockQueueSelftestError::Unsupported))
            | Err(OpenBlockDeviceError::Busy) => {
                continue;
            }
            Ok(Err(err)) => return Err(err),
            Err(OpenBlockDeviceError::NotFound) => continue,
        }
    }

    Err(AsyncBlockQueueSelftestError::NoBlockDevice)
}

pub fn async_block_queue_read_selftest() -> Result<(), AsyncBlockQueueSelftestError> {
    async_block_queue_read_write_selftest()
}

pub fn new_block_filesystem(
    fs_type: &str,
    dev: AxBlockDevice,
) -> axfs_ng_vfs::VfsResult<axfs_ng_vfs::Filesystem> {
    fs::new_named(fs_type, dev)
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
    let root_index = block_devs
        .iter()
        .enumerate()
        .max_by_key(|(_, dev)| dev.num_blocks())
        .map(|(index, _)| index)
        .expect("No block device found!");
    let dev = block_devs.remove(root_index);
    info!(
        "  use block device {root_index}: {:?} (blocks={})",
        dev.device_name(),
        dev.num_blocks()
    );

    let fs = fs::new_default(dev).expect("Failed to initialize filesystem");
    info!("  filesystem type: {:?}", fs.name());

    let mp = axfs_ng_vfs::Mountpoint::new_root(&fs);
    ROOT_FS_CONTEXT.call_once(|| FsContext::new(mp.root_location()));

    let mut extras = Vec::new();
    let mut index = 1;
    while !block_devs.is_empty() {
        let dev = block_devs.remove(0);
        debug!(
            "axfs init_filesystems: registering extra block device {} as {}",
            dev.device_name(),
            extra_device_name(index)
        );
        extras.push(RegisteredBlockDevice {
            name: extra_device_name(index),
            info: BlockDeviceInfo {
                num_blocks: dev.num_blocks(),
                block_size: dev.block_size(),
            },
            read_only: AtomicBool::new(false),
            device: Some(dev),
        });
        index += 1;
    }
    EXTRA_BLOCK_DEVICES.call_once(|| Mutex::new(extras));
}
