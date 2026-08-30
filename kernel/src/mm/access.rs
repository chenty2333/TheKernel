use alloc::{sync::Arc, vec::Vec};
#[cfg(feature = "test-io-control")]
use core::time::Duration;
use core::{
    hint::unlikely,
    mem::MaybeUninit,
    ptr, slice,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs::CachedFilePagePin;
use axhal::{
    mem::phys_to_virt,
    paging::{MappingFlags, PageSize, PagingError},
};
use axio::prelude::*;
use axsync::Mutex;
#[cfg(feature = "test-io-control")]
use axtask::sleep;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr};
use thekernel_linux_mm::{PinAccess, PinDuration, PinRequest, PinToken, PinUse, UserRange};
use thekernel_linux_usercopy::{UserCopyError, VmResult};

use super::{
    AddrSpace, Backend, PhysicalFramePins, PreparedPhysicalFramePins, SharedFutexKey,
    UserIoMappingExpectation, UserMemoryCapability, map_usercopy_error,
    prepare_physical_pin_registry,
};
use crate::{
    config::{USER_SPACE_BASE, USER_SPACE_SIZE},
    syscall::ensure_4k_granularity_across_aliases,
};
static ENABLE_USER_IO_PIN_COUNTERS: AtomicBool = AtomicBool::new(false);
static USER_IO_PIN_TO_USER_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_TO_USER_HITS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_TO_USER_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_FROM_USER_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_FROM_USER_HITS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_FROM_USER_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_EMPTY: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_UNALIGNED: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_ACCESS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_POPULATE: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_PAGETABLE: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_NONCONTIG: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_SEGMENTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_FRAME_PIN: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_PAGE_CACHE_PIN: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_COW_PIN: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_SHARED_PIN: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_FILE_PIN: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_REJECT_LINEAR_PIN: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_SG_BATCHES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_SG_SEGMENTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_SG_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_SG_MULTI_SEGMENT_BATCHES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_DIRECT_READ_HITS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_DIRECT_READ_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_DIRECT_READ_SEGMENTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_DIRECT_READ_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_DIRECT_WRITE_HITS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_DIRECT_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_DIRECT_WRITE_SEGMENTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_DIRECT_WRITE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PREFAULT_TO_USER_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PREFAULT_TO_USER_HITS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PREFAULT_TO_USER_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PREFAULT_TO_USER_REJECTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PREFAULT_FROM_USER_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PREFAULT_FROM_USER_HITS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PREFAULT_FROM_USER_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PREFAULT_FROM_USER_REJECTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_COW_PIN_PAGES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_SHARED_PIN_PAGES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_FILE_PIN_PAGES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_FRAME_PIN_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_FRAME_PIN_HITS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_FRAME_PIN_PAGES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_FRAME_PIN_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_FRAME_PIN_UNPINS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_PAGE_CACHE_PIN_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_PAGE_CACHE_PIN_HITS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_PAGE_CACHE_PIN_PAGES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_PAGE_CACHE_PIN_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_PAGE_CACHE_PIN_UNPINS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_VM_RANGE_PIN_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_VM_RANGE_PIN_HITS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_VM_RANGE_PIN_BYTES: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_VM_RANGE_PIN_REJECTS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_VM_RANGE_PIN_UNPINS: AtomicU64 = AtomicU64::new(0);
static USER_IO_PIN_UNPINS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static USER_IO_PIN_TEST_DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// Bounds one address-space critical section while collecting mapping
/// expectations or acquiring exact lower-level owners for resident user pages.
const USER_IO_PIN_SCAN_CHUNK_PAGES: usize = 64;
#[cfg(feature = "test-io-control")]
pub const USER_IO_PIN_TEST_DELAY_MS_MAX: u64 = 1_000;

fn user_io_pin_scan_chunk_end(cursor: VirtAddr, end: VirtAddr) -> VirtAddr {
    debug_assert!(cursor < end);
    debug_assert!(cursor.is_aligned_4k() && end.is_aligned_4k());
    cursor + (end - cursor).min(USER_IO_PIN_SCAN_CHUNK_PAGES * PAGE_SIZE_4K)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UserIoPinCounters {
    pub to_user_attempts: u64,
    pub to_user_hits: u64,
    pub to_user_bytes: u64,
    pub from_user_attempts: u64,
    pub from_user_hits: u64,
    pub from_user_bytes: u64,
    pub reject_empty: u64,
    pub reject_unaligned: u64,
    pub reject_access: u64,
    pub reject_populate: u64,
    pub reject_pagetable: u64,
    pub reject_noncontig: u64,
    pub reject_segments: u64,
    pub reject_frame_pin: u64,
    pub reject_page_cache_pin: u64,
    pub reject_cow_pin: u64,
    pub reject_shared_pin: u64,
    pub reject_file_pin: u64,
    pub reject_linear_pin: u64,
    pub sg_batches: u64,
    pub sg_segments: u64,
    pub sg_bytes: u64,
    pub sg_multi_segment_batches: u64,
    pub direct_read_hits: u64,
    pub direct_read_bytes: u64,
    pub direct_read_segments: u64,
    pub direct_read_fallbacks: u64,
    pub direct_write_hits: u64,
    pub direct_write_bytes: u64,
    pub direct_write_segments: u64,
    pub direct_write_fallbacks: u64,
    pub prefault_to_user_attempts: u64,
    pub prefault_to_user_hits: u64,
    pub prefault_to_user_bytes: u64,
    pub prefault_to_user_rejects: u64,
    pub prefault_from_user_attempts: u64,
    pub prefault_from_user_hits: u64,
    pub prefault_from_user_bytes: u64,
    pub prefault_from_user_rejects: u64,
    pub cow_pin_pages: u64,
    pub shared_pin_pages: u64,
    pub file_pin_pages: u64,
    pub frame_pin_attempts: u64,
    pub frame_pin_hits: u64,
    pub frame_pin_pages: u64,
    pub frame_pin_bytes: u64,
    pub frame_pin_unpins: u64,
    pub page_cache_pin_attempts: u64,
    pub page_cache_pin_hits: u64,
    pub page_cache_pin_pages: u64,
    pub page_cache_pin_bytes: u64,
    pub page_cache_pin_unpins: u64,
    pub vm_range_pin_attempts: u64,
    pub vm_range_pin_hits: u64,
    pub vm_range_pin_bytes: u64,
    pub vm_range_pin_rejects: u64,
    pub vm_range_pin_unpins: u64,
    pub unpins: u64,
}

pub fn set_user_io_pin_counters_enabled(enabled: bool) {
    ENABLE_USER_IO_PIN_COUNTERS.store(enabled, Ordering::Relaxed);
}

pub fn reset_user_io_pin_counters() {
    for counter in [
        &USER_IO_PIN_TO_USER_ATTEMPTS,
        &USER_IO_PIN_TO_USER_HITS,
        &USER_IO_PIN_TO_USER_BYTES,
        &USER_IO_PIN_FROM_USER_ATTEMPTS,
        &USER_IO_PIN_FROM_USER_HITS,
        &USER_IO_PIN_FROM_USER_BYTES,
        &USER_IO_PIN_REJECT_EMPTY,
        &USER_IO_PIN_REJECT_UNALIGNED,
        &USER_IO_PIN_REJECT_ACCESS,
        &USER_IO_PIN_REJECT_POPULATE,
        &USER_IO_PIN_REJECT_PAGETABLE,
        &USER_IO_PIN_REJECT_NONCONTIG,
        &USER_IO_PIN_REJECT_SEGMENTS,
        &USER_IO_PIN_REJECT_FRAME_PIN,
        &USER_IO_PIN_REJECT_PAGE_CACHE_PIN,
        &USER_IO_PIN_REJECT_COW_PIN,
        &USER_IO_PIN_REJECT_SHARED_PIN,
        &USER_IO_PIN_REJECT_FILE_PIN,
        &USER_IO_PIN_REJECT_LINEAR_PIN,
        &USER_IO_PIN_SG_BATCHES,
        &USER_IO_PIN_SG_SEGMENTS,
        &USER_IO_PIN_SG_BYTES,
        &USER_IO_PIN_SG_MULTI_SEGMENT_BATCHES,
        &USER_IO_PIN_DIRECT_READ_HITS,
        &USER_IO_PIN_DIRECT_READ_BYTES,
        &USER_IO_PIN_DIRECT_READ_SEGMENTS,
        &USER_IO_PIN_DIRECT_READ_FALLBACKS,
        &USER_IO_PIN_DIRECT_WRITE_HITS,
        &USER_IO_PIN_DIRECT_WRITE_BYTES,
        &USER_IO_PIN_DIRECT_WRITE_SEGMENTS,
        &USER_IO_PIN_DIRECT_WRITE_FALLBACKS,
        &USER_IO_PREFAULT_TO_USER_ATTEMPTS,
        &USER_IO_PREFAULT_TO_USER_HITS,
        &USER_IO_PREFAULT_TO_USER_BYTES,
        &USER_IO_PREFAULT_TO_USER_REJECTS,
        &USER_IO_PREFAULT_FROM_USER_ATTEMPTS,
        &USER_IO_PREFAULT_FROM_USER_HITS,
        &USER_IO_PREFAULT_FROM_USER_BYTES,
        &USER_IO_PREFAULT_FROM_USER_REJECTS,
        &USER_IO_PIN_COW_PIN_PAGES,
        &USER_IO_PIN_SHARED_PIN_PAGES,
        &USER_IO_PIN_FILE_PIN_PAGES,
        &USER_IO_PIN_FRAME_PIN_ATTEMPTS,
        &USER_IO_PIN_FRAME_PIN_HITS,
        &USER_IO_PIN_FRAME_PIN_PAGES,
        &USER_IO_PIN_FRAME_PIN_BYTES,
        &USER_IO_PIN_FRAME_PIN_UNPINS,
        &USER_IO_PIN_PAGE_CACHE_PIN_ATTEMPTS,
        &USER_IO_PIN_PAGE_CACHE_PIN_HITS,
        &USER_IO_PIN_PAGE_CACHE_PIN_PAGES,
        &USER_IO_PIN_PAGE_CACHE_PIN_BYTES,
        &USER_IO_PIN_PAGE_CACHE_PIN_UNPINS,
        &USER_IO_PIN_VM_RANGE_PIN_ATTEMPTS,
        &USER_IO_PIN_VM_RANGE_PIN_HITS,
        &USER_IO_PIN_VM_RANGE_PIN_BYTES,
        &USER_IO_PIN_VM_RANGE_PIN_REJECTS,
        &USER_IO_PIN_VM_RANGE_PIN_UNPINS,
        &USER_IO_PIN_UNPINS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

#[cfg(feature = "test-io-control")]
pub fn set_user_io_pin_test_delay_ms(delay_ms: u64) -> AxResult {
    if delay_ms > USER_IO_PIN_TEST_DELAY_MS_MAX {
        return Err(AxError::InvalidInput);
    }
    USER_IO_PIN_TEST_DELAY_MS.store(delay_ms, Ordering::Relaxed);
    Ok(())
}

#[cfg(feature = "test-io-control")]
fn user_io_pin_test_delay_ms() -> u64 {
    USER_IO_PIN_TEST_DELAY_MS.load(Ordering::Relaxed)
}

pub fn user_io_pin_counters_snapshot() -> UserIoPinCounters {
    UserIoPinCounters {
        to_user_attempts: USER_IO_PIN_TO_USER_ATTEMPTS.load(Ordering::Relaxed),
        to_user_hits: USER_IO_PIN_TO_USER_HITS.load(Ordering::Relaxed),
        to_user_bytes: USER_IO_PIN_TO_USER_BYTES.load(Ordering::Relaxed),
        from_user_attempts: USER_IO_PIN_FROM_USER_ATTEMPTS.load(Ordering::Relaxed),
        from_user_hits: USER_IO_PIN_FROM_USER_HITS.load(Ordering::Relaxed),
        from_user_bytes: USER_IO_PIN_FROM_USER_BYTES.load(Ordering::Relaxed),
        reject_empty: USER_IO_PIN_REJECT_EMPTY.load(Ordering::Relaxed),
        reject_unaligned: USER_IO_PIN_REJECT_UNALIGNED.load(Ordering::Relaxed),
        reject_access: USER_IO_PIN_REJECT_ACCESS.load(Ordering::Relaxed),
        reject_populate: USER_IO_PIN_REJECT_POPULATE.load(Ordering::Relaxed),
        reject_pagetable: USER_IO_PIN_REJECT_PAGETABLE.load(Ordering::Relaxed),
        reject_noncontig: USER_IO_PIN_REJECT_NONCONTIG.load(Ordering::Relaxed),
        reject_segments: USER_IO_PIN_REJECT_SEGMENTS.load(Ordering::Relaxed),
        reject_frame_pin: USER_IO_PIN_REJECT_FRAME_PIN.load(Ordering::Relaxed),
        reject_page_cache_pin: USER_IO_PIN_REJECT_PAGE_CACHE_PIN.load(Ordering::Relaxed),
        reject_cow_pin: USER_IO_PIN_REJECT_COW_PIN.load(Ordering::Relaxed),
        reject_shared_pin: USER_IO_PIN_REJECT_SHARED_PIN.load(Ordering::Relaxed),
        reject_file_pin: USER_IO_PIN_REJECT_FILE_PIN.load(Ordering::Relaxed),
        reject_linear_pin: USER_IO_PIN_REJECT_LINEAR_PIN.load(Ordering::Relaxed),
        sg_batches: USER_IO_PIN_SG_BATCHES.load(Ordering::Relaxed),
        sg_segments: USER_IO_PIN_SG_SEGMENTS.load(Ordering::Relaxed),
        sg_bytes: USER_IO_PIN_SG_BYTES.load(Ordering::Relaxed),
        sg_multi_segment_batches: USER_IO_PIN_SG_MULTI_SEGMENT_BATCHES.load(Ordering::Relaxed),
        direct_read_hits: USER_IO_PIN_DIRECT_READ_HITS.load(Ordering::Relaxed),
        direct_read_bytes: USER_IO_PIN_DIRECT_READ_BYTES.load(Ordering::Relaxed),
        direct_read_segments: USER_IO_PIN_DIRECT_READ_SEGMENTS.load(Ordering::Relaxed),
        direct_read_fallbacks: USER_IO_PIN_DIRECT_READ_FALLBACKS.load(Ordering::Relaxed),
        direct_write_hits: USER_IO_PIN_DIRECT_WRITE_HITS.load(Ordering::Relaxed),
        direct_write_bytes: USER_IO_PIN_DIRECT_WRITE_BYTES.load(Ordering::Relaxed),
        direct_write_segments: USER_IO_PIN_DIRECT_WRITE_SEGMENTS.load(Ordering::Relaxed),
        direct_write_fallbacks: USER_IO_PIN_DIRECT_WRITE_FALLBACKS.load(Ordering::Relaxed),
        prefault_to_user_attempts: USER_IO_PREFAULT_TO_USER_ATTEMPTS.load(Ordering::Relaxed),
        prefault_to_user_hits: USER_IO_PREFAULT_TO_USER_HITS.load(Ordering::Relaxed),
        prefault_to_user_bytes: USER_IO_PREFAULT_TO_USER_BYTES.load(Ordering::Relaxed),
        prefault_to_user_rejects: USER_IO_PREFAULT_TO_USER_REJECTS.load(Ordering::Relaxed),
        prefault_from_user_attempts: USER_IO_PREFAULT_FROM_USER_ATTEMPTS.load(Ordering::Relaxed),
        prefault_from_user_hits: USER_IO_PREFAULT_FROM_USER_HITS.load(Ordering::Relaxed),
        prefault_from_user_bytes: USER_IO_PREFAULT_FROM_USER_BYTES.load(Ordering::Relaxed),
        prefault_from_user_rejects: USER_IO_PREFAULT_FROM_USER_REJECTS.load(Ordering::Relaxed),
        cow_pin_pages: USER_IO_PIN_COW_PIN_PAGES.load(Ordering::Relaxed),
        shared_pin_pages: USER_IO_PIN_SHARED_PIN_PAGES.load(Ordering::Relaxed),
        file_pin_pages: USER_IO_PIN_FILE_PIN_PAGES.load(Ordering::Relaxed),
        frame_pin_attempts: USER_IO_PIN_FRAME_PIN_ATTEMPTS.load(Ordering::Relaxed),
        frame_pin_hits: USER_IO_PIN_FRAME_PIN_HITS.load(Ordering::Relaxed),
        frame_pin_pages: USER_IO_PIN_FRAME_PIN_PAGES.load(Ordering::Relaxed),
        frame_pin_bytes: USER_IO_PIN_FRAME_PIN_BYTES.load(Ordering::Relaxed),
        frame_pin_unpins: USER_IO_PIN_FRAME_PIN_UNPINS.load(Ordering::Relaxed),
        page_cache_pin_attempts: USER_IO_PIN_PAGE_CACHE_PIN_ATTEMPTS.load(Ordering::Relaxed),
        page_cache_pin_hits: USER_IO_PIN_PAGE_CACHE_PIN_HITS.load(Ordering::Relaxed),
        page_cache_pin_pages: USER_IO_PIN_PAGE_CACHE_PIN_PAGES.load(Ordering::Relaxed),
        page_cache_pin_bytes: USER_IO_PIN_PAGE_CACHE_PIN_BYTES.load(Ordering::Relaxed),
        page_cache_pin_unpins: USER_IO_PIN_PAGE_CACHE_PIN_UNPINS.load(Ordering::Relaxed),
        vm_range_pin_attempts: USER_IO_PIN_VM_RANGE_PIN_ATTEMPTS.load(Ordering::Relaxed),
        vm_range_pin_hits: USER_IO_PIN_VM_RANGE_PIN_HITS.load(Ordering::Relaxed),
        vm_range_pin_bytes: USER_IO_PIN_VM_RANGE_PIN_BYTES.load(Ordering::Relaxed),
        vm_range_pin_rejects: USER_IO_PIN_VM_RANGE_PIN_REJECTS.load(Ordering::Relaxed),
        vm_range_pin_unpins: USER_IO_PIN_VM_RANGE_PIN_UNPINS.load(Ordering::Relaxed),
        unpins: USER_IO_PIN_UNPINS.load(Ordering::Relaxed),
    }
}

#[inline(always)]
fn user_io_pin_counters_enabled() -> bool {
    ENABLE_USER_IO_PIN_COUNTERS.load(Ordering::Relaxed)
}

#[inline(always)]
fn record_user_io_pin_counter(counter: &AtomicU64, value: u64) {
    if user_io_pin_counters_enabled() {
        counter.fetch_add(value, Ordering::Relaxed);
    }
}

#[inline(always)]
pub fn record_user_io_direct_read(bytes: usize, segments: usize) {
    if bytes == 0 {
        return;
    }
    record_user_io_pin_counter(&USER_IO_PIN_DIRECT_READ_HITS, 1);
    record_user_io_pin_counter(&USER_IO_PIN_DIRECT_READ_BYTES, bytes as u64);
    record_user_io_pin_counter(&USER_IO_PIN_DIRECT_READ_SEGMENTS, segments as u64);
}

#[inline(always)]
pub fn record_user_io_direct_read_fallback() {
    record_user_io_pin_counter(&USER_IO_PIN_DIRECT_READ_FALLBACKS, 1);
}

#[inline(always)]
pub fn record_user_io_direct_write(bytes: usize, segments: usize) {
    if bytes == 0 {
        return;
    }
    record_user_io_pin_counter(&USER_IO_PIN_DIRECT_WRITE_HITS, 1);
    record_user_io_pin_counter(&USER_IO_PIN_DIRECT_WRITE_BYTES, bytes as u64);
    record_user_io_pin_counter(&USER_IO_PIN_DIRECT_WRITE_SEGMENTS, segments as u64);
}

#[inline(always)]
pub fn record_user_io_direct_write_fallback() {
    record_user_io_pin_counter(&USER_IO_PIN_DIRECT_WRITE_FALLBACKS, 1);
}

/// Prefaults a destination range in an explicitly selected address space.
pub fn prefault_user_io_to_user_with(
    capability: &UserMemoryCapability,
    ptr: *mut u8,
    len: usize,
) -> AxResult {
    if len == 0 {
        return Ok(());
    }
    record_user_io_pin_counter(&USER_IO_PREFAULT_TO_USER_ATTEMPTS, 1);
    match populate_user_range_with(capability, ptr as usize, len, MappingFlags::WRITE) {
        Ok(()) => {
            record_user_io_pin_counter(&USER_IO_PREFAULT_TO_USER_HITS, 1);
            record_user_io_pin_counter(&USER_IO_PREFAULT_TO_USER_BYTES, len as u64);
            Ok(())
        }
        Err(err) => {
            record_user_io_pin_counter(&USER_IO_PREFAULT_TO_USER_REJECTS, 1);
            Err(map_usercopy_error(err))
        }
    }
}

/// Prefaults a source range in an explicitly selected address space.
pub fn prefault_user_io_from_user_with(
    capability: &UserMemoryCapability,
    ptr: *const u8,
    len: usize,
) -> AxResult {
    if len == 0 {
        return Ok(());
    }
    record_user_io_pin_counter(&USER_IO_PREFAULT_FROM_USER_ATTEMPTS, 1);
    match populate_user_range_with(capability, ptr as usize, len, MappingFlags::READ) {
        Ok(()) => {
            record_user_io_pin_counter(&USER_IO_PREFAULT_FROM_USER_HITS, 1);
            record_user_io_pin_counter(&USER_IO_PREFAULT_FROM_USER_BYTES, len as u64);
            Ok(())
        }
        Err(err) => {
            record_user_io_pin_counter(&USER_IO_PREFAULT_FROM_USER_REJECTS, 1);
            Err(map_usercopy_error(err))
        }
    }
}

/// A pointer to user space memory.
#[repr(transparent)]
#[derive(PartialEq, Clone, Copy)]
pub struct UserPtr<T>(*mut T);

impl<T> From<usize> for UserPtr<T> {
    fn from(value: usize) -> Self {
        UserPtr(value as *mut _)
    }
}

impl<T> From<*mut T> for UserPtr<T> {
    fn from(value: *mut T) -> Self {
        UserPtr(value)
    }
}

impl<T> Default for UserPtr<T> {
    fn default() -> Self {
        Self(ptr::null_mut())
    }
}

impl<T> UserPtr<T> {
    pub fn address(&self) -> VirtAddr {
        VirtAddr::from_ptr_of(self.0)
    }

    pub fn cast<U>(self) -> UserPtr<U> {
        UserPtr(self.0 as *mut U)
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

/// An immutable pointer to user space memory.
#[repr(transparent)]
#[derive(PartialEq, Clone, Copy)]
pub struct UserConstPtr<T>(*const T);

impl<T> From<usize> for UserConstPtr<T> {
    fn from(value: usize) -> Self {
        UserConstPtr(value as *const _)
    }
}

impl<T> From<*const T> for UserConstPtr<T> {
    fn from(value: *const T) -> Self {
        UserConstPtr(value)
    }
}

impl<T> Default for UserConstPtr<T> {
    fn default() -> Self {
        Self(ptr::null())
    }
}

impl<T> UserConstPtr<T> {
    pub fn address(&self) -> VirtAddr {
        VirtAddr::from_ptr_of(self.0)
    }

    pub fn cast<U>(self) -> UserConstPtr<U> {
        UserConstPtr(self.0 as *const U)
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

/// Briefly checks if the given memory region is valid user memory.
pub fn check_access(start: usize, len: usize) -> AxResult {
    const USER_SPACE_END: usize = USER_SPACE_BASE + USER_SPACE_SIZE;
    let ok = (USER_SPACE_BASE..USER_SPACE_END).contains(&start) && (USER_SPACE_END - start) >= len;
    if unlikely(!ok) {
        Err(AxError::BadAddress)
    } else {
        Ok(())
    }
}

fn map_populate_error(error: AxError) -> UserCopyError {
    match error {
        AxError::NoMemory => UserCopyError::NoMemory,
        AxError::BadAddress | AxError::InvalidInput => UserCopyError::BadAddress,
        _ => UserCopyError::AccessDenied,
    }
}

fn populate_user_range_with(
    capability: &UserMemoryCapability,
    start: usize,
    len: usize,
    access_flags: MappingFlags,
) -> VmResult {
    check_access(start, len).map_err(map_populate_error)?;
    if len == 0 {
        return Ok(());
    }

    let start = VirtAddr::from(start);
    let page_start = start.align_down_4k();
    let end = start.checked_add(len).ok_or(UserCopyError::BadAddress)?;
    let page_end = VirtAddr::from(
        super::checked_align_up_4k(end.as_usize()).ok_or(UserCopyError::BadAddress)?,
    );
    let aspace_handle = capability.address_space();
    let mut aspace = aspace_handle.lock();
    if !aspace.can_access_range(start, len, access_flags) {
        return Err(UserCopyError::AccessDenied);
    }
    aspace
        .populate_area(page_start, page_end - page_start, access_flags)
        .map_err(map_populate_error)
}

fn reject_user_io_pin(counter: &AtomicU64) {
    record_user_io_pin_counter(counter, 1);
}

fn record_user_io_backend_pin_hit(backend: &Backend) {
    match backend {
        Backend::Cow(_) => record_user_io_pin_counter(&USER_IO_PIN_COW_PIN_PAGES, 1),
        Backend::Shared(_) => record_user_io_pin_counter(&USER_IO_PIN_SHARED_PIN_PAGES, 1),
        Backend::File(_) => record_user_io_pin_counter(&USER_IO_PIN_FILE_PIN_PAGES, 1),
        Backend::Linear(_) => {}
    }
}

fn record_user_io_backend_pin_reject(backend: &Backend) {
    match backend {
        Backend::Cow(_) => reject_user_io_pin(&USER_IO_PIN_REJECT_COW_PIN),
        Backend::Shared(_) => reject_user_io_pin(&USER_IO_PIN_REJECT_SHARED_PIN),
        Backend::File(_) => reject_user_io_pin(&USER_IO_PIN_REJECT_FILE_PIN),
        Backend::Linear(_) => reject_user_io_pin(&USER_IO_PIN_REJECT_LINEAR_PIN),
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct UserIoPinSegment {
    pub paddr: usize,
    pub len: usize,
}

/// Provenance of the pages covered by a user-I/O pin.
///
/// This is an aggregate observation captured while the pin owns the address
/// space's mapping snapshot.  `PrivateAnonymous` is reported only when every
/// covered page came from an anonymous private COW backend; callers must not
/// use it to bypass the pin owner or its lifetime.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum UserIoPinProvenance {
    PrivateAnonymous,
    #[default]
    Ineligible,
}

fn user_io_pin_provenance_for_backend(backend: &Backend) -> UserIoPinProvenance {
    if backend.is_private_anonymous() {
        UserIoPinProvenance::PrivateAnonymous
    } else {
        UserIoPinProvenance::Ineligible
    }
}

fn join_user_io_pin_provenance(
    aggregate: Option<UserIoPinProvenance>,
    page: UserIoPinProvenance,
) -> Option<UserIoPinProvenance> {
    Some(match (aggregate, page) {
        (None, page) => page,
        (Some(UserIoPinProvenance::PrivateAnonymous), UserIoPinProvenance::PrivateAnonymous) => {
            UserIoPinProvenance::PrivateAnonymous
        }
        _ => UserIoPinProvenance::Ineligible,
    })
}

#[derive(Debug, Clone)]
pub struct UserIoPinSegments {
    segments: Vec<UserIoPinSegment>,
    max_segments: usize,
    bytes: usize,
}

impl UserIoPinSegments {
    fn try_new(max_segments: usize) -> Option<Self> {
        let mut segments = Vec::new();
        segments.try_reserve_exact(max_segments).ok()?;
        Some(Self {
            segments,
            max_segments,
            bytes: 0,
        })
    }

    fn push_or_merge(&mut self, paddr: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        if let Some(prev) = self
            .segments
            .len()
            .checked_sub(1)
            .map(|idx| &mut self.segments[idx])
            && prev.paddr.checked_add(prev.len) == Some(paddr)
        {
            let Some(merged_len) = prev.len.checked_add(len) else {
                return false;
            };
            let Some(bytes) = self.bytes.checked_add(len) else {
                return false;
            };
            prev.len = merged_len;
            self.bytes = bytes;
            return true;
        }
        if self.segments.len() == self.max_segments {
            return false;
        }
        let Some(bytes) = self.bytes.checked_add(len) else {
            return false;
        };
        self.segments.push(UserIoPinSegment { paddr, len });
        self.bytes = bytes;
        true
    }

    pub fn as_slice(&self) -> &[UserIoPinSegment] {
        &self.segments
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    fn physical_ranges_are_disjoint(&self) -> bool {
        physical_pin_segments_are_disjoint(self.as_slice().iter())
    }
}

fn physical_pin_segments_are_disjoint<'a, I>(segments: I) -> bool
where
    I: Iterator<Item = &'a UserIoPinSegment> + Clone,
{
    for (index, segment) in segments.clone().enumerate() {
        let Some(end) = segment.paddr.checked_add(segment.len) else {
            return false;
        };
        if segment.len == 0 {
            continue;
        }
        for other in segments.clone().skip(index + 1) {
            let Some(other_end) = other.paddr.checked_add(other.len) else {
                return false;
            };
            if other.len != 0 && segment.paddr < other_end && other.paddr < end {
                return false;
            }
        }
    }
    true
}

fn record_user_io_pin_segments(segments: &UserIoPinSegments) {
    record_user_io_pin_counter(&USER_IO_PIN_SG_BATCHES, 1);
    record_user_io_pin_counter(&USER_IO_PIN_SG_SEGMENTS, segments.len() as u64);
    record_user_io_pin_counter(&USER_IO_PIN_SG_BYTES, segments.bytes() as u64);
    if segments.len() > 1 {
        record_user_io_pin_counter(&USER_IO_PIN_SG_MULTI_SEGMENT_BATCHES, 1);
    }
}

struct UserIoFramePins {
    _pins: Vec<PhysicalFramePins>,
    pages: usize,
}

impl UserIoFramePins {
    fn new(pins: Vec<PhysicalFramePins>) -> Self {
        let pages = pins.iter().map(PhysicalFramePins::len).sum();
        Self { _pins: pins, pages }
    }
}

impl Drop for UserIoFramePins {
    fn drop(&mut self) {
        record_user_io_pin_counter(&USER_IO_PIN_FRAME_PIN_UNPINS, self.pages as u64);
    }
}

struct UserIoPageCachePins {
    pins: Vec<CachedFilePagePin>,
}

impl UserIoPageCachePins {
    fn new(pins: Vec<CachedFilePagePin>) -> Self {
        Self { pins }
    }
}

impl Drop for UserIoPageCachePins {
    fn drop(&mut self) {
        record_user_io_pin_counter(&USER_IO_PIN_PAGE_CACHE_PIN_UNPINS, self.pins.len() as u64);
    }
}

struct UserIoRangePin {
    aspace: Arc<Mutex<AddrSpace>>,
    token: PinToken,
    _system_charge: super::UserIoSystemPinCharge,
}

impl Drop for UserIoRangePin {
    fn drop(&mut self) {
        let cow_frames = {
            let mut aspace = super::lock_mm_diagnosed!(self.aspace, UserPinRelease);
            aspace.end_user_io_pin(self.token)
        };
        // The owner vector can be large; reclaim it only after releasing the
        // address-space lock.
        drop(cow_frames);
        record_user_io_pin_counter(&USER_IO_PIN_VM_RANGE_PIN_UNPINS, 1);
    }
}

/// Owns every unpublished resource in the user-I/O pin transaction.
///
/// Failure cleanup releases exact frame/page-cache owners before cancelling
/// the VM reservation and finally refunding the system-wide charge. This keeps
/// the same ordering on allocation failure, mapping rejection, stale
/// revalidation, and successful-call construction failures.
struct UnpublishedUserIoPin {
    aspace: Arc<Mutex<AddrSpace>>,
    reservation: Option<thekernel_linux_mm::PinReservation>,
    system_charge: Option<super::UserIoSystemPinCharge>,
    expectations: Vec<UserIoMappingExpectation>,
    frame_pins: Vec<PhysicalFramePins>,
    page_cache_pins: Vec<CachedFilePagePin>,
    cow_frames: Vec<PhysAddr>,
    charged_pages: usize,
    frame_pages_admitted: usize,
    provenance: Option<UserIoPinProvenance>,
}

impl UnpublishedUserIoPin {
    fn try_new(
        aspace: Arc<Mutex<AddrSpace>>,
        reservation: thekernel_linux_mm::PinReservation,
        system_charge: super::UserIoSystemPinCharge,
        page_count: usize,
    ) -> Option<Self> {
        let mut preparation = Self {
            aspace,
            reservation: Some(reservation),
            system_charge: Some(system_charge),
            expectations: Vec::new(),
            frame_pins: Vec::new(),
            page_cache_pins: Vec::new(),
            cow_frames: Vec::new(),
            charged_pages: page_count,
            frame_pages_admitted: 0,
            provenance: None,
        };
        let frame_batches = page_count.div_ceil(USER_IO_PIN_SCAN_CHUNK_PAGES);
        if preparation
            .expectations
            .try_reserve_exact(page_count)
            .is_err()
            || preparation
                .frame_pins
                .try_reserve_exact(frame_batches)
                .is_err()
            || preparation
                .page_cache_pins
                .try_reserve_exact(page_count)
                .is_err()
            || preparation
                .cow_frames
                .try_reserve_exact(page_count)
                .is_err()
        {
            return None;
        }
        Some(preparation)
    }

    fn reservation(&self) -> thekernel_linux_mm::PinReservation {
        self.reservation.expect("active user-I/O pin preparation")
    }

    fn admit_frame_pages(&mut self, pages: usize) -> AxResult<&super::UserIoSystemPinCharge> {
        let admitted = self
            .frame_pages_admitted
            .checked_add(pages)
            .ok_or(AxError::NoMemory)?;
        if admitted > self.charged_pages {
            return Err(AxError::ResourceBusy);
        }
        self.frame_pages_admitted = admitted;
        Ok(self
            .system_charge
            .as_ref()
            .expect("active user-I/O system charge"))
    }

    fn expectations(&self) -> &[UserIoMappingExpectation] {
        &self.expectations
    }

    fn observe_backend(&mut self, backend: &Backend) {
        self.provenance = join_user_io_pin_provenance(
            self.provenance,
            user_io_pin_provenance_for_backend(backend),
        );
    }

    fn provenance(&self) -> UserIoPinProvenance {
        self.provenance.unwrap_or_default()
    }

    fn finish(mut self, segments: UserIoPinSegments, token: PinToken) -> PreparedUserIoPin {
        self.reservation = None;
        let system_charge = self
            .system_charge
            .take()
            .expect("published user-I/O pin lost its system charge");
        let frame_pins = UserIoFramePins::new(core::mem::take(&mut self.frame_pins));
        let page_cache_pins = UserIoPageCachePins::new(core::mem::take(&mut self.page_cache_pins));
        let range_pin = UserIoRangePin {
            aspace: self.aspace.clone(),
            token,
            _system_charge: system_charge,
        };
        PreparedUserIoPin {
            segments,
            provenance: self.provenance(),
            frame_pins,
            page_cache_pins,
            range_pin,
        }
    }
}

impl Drop for UnpublishedUserIoPin {
    fn drop(&mut self) {
        // Vec::clear performs no allocation. Deferred physical-frame frees and
        // cached-page unpins run without the address-space lock held.
        self.frame_pins.clear();
        self.page_cache_pins.clear();
        if let Some(reservation) = self.reservation.take() {
            let mut aspace = super::lock_mm_diagnosed!(self.aspace, UserPinRelease);
            aspace.cancel_user_io_pin(reservation);
        }
        drop(self.system_charge.take());
    }
}

struct PreparedUserIoPin {
    segments: UserIoPinSegments,
    provenance: UserIoPinProvenance,
    frame_pins: UserIoFramePins,
    page_cache_pins: UserIoPageCachePins,
    range_pin: UserIoRangePin,
}

fn prepare_user_io_pin_with(
    capability: &UserMemoryCapability,
    start: usize,
    len: usize,
    access_flags: MappingFlags,
    require_contiguous: bool,
) -> Option<PreparedUserIoPin> {
    prepare_user_io_pin_with_duration(
        capability,
        start,
        len,
        access_flags,
        require_contiguous,
        PinDuration::AsyncIo,
    )
}

fn prepare_user_io_pin_with_duration(
    capability: &UserMemoryCapability,
    start: usize,
    len: usize,
    access_flags: MappingFlags,
    require_contiguous: bool,
    duration: PinDuration,
) -> Option<PreparedUserIoPin> {
    if len == 0 {
        reject_user_io_pin(&USER_IO_PIN_REJECT_EMPTY);
        return None;
    }
    if len < PAGE_SIZE_4K {
        reject_user_io_pin(&USER_IO_PIN_REJECT_UNALIGNED);
        return None;
    }
    if check_access(start, len).is_err() {
        reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
        return None;
    }
    let Some(end) = start.checked_add(len) else {
        reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
        return None;
    };

    let start_addr = VirtAddr::from(start);
    let page_start = start_addr.align_down_4k();
    let Some(page_end) = super::checked_align_up_4k(end).map(VirtAddr::from) else {
        reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
        return None;
    };
    let page_len = page_end - page_start;
    let aspace_handle = capability.address_space().clone();
    // Demote before installing our own full-range reservation: the demotion
    // correctly rejects other pins, whereas this reservation would otherwise
    // self-conflict with its pin preflight. Once reserved, the pin fence
    // prevents a concurrent collapse from recreating a compound folio.
    if ensure_4k_granularity_across_aliases(&aspace_handle, page_start, page_len).is_err() {
        reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
        return None;
    }
    let admission = {
        let mut aspace = super::lock_mm_diagnosed!(aspace_handle, UserPinAdmission);
        record_user_io_pin_counter(&USER_IO_PIN_VM_RANGE_PIN_ATTEMPTS, 1);
        let request = PinRequest::new(
            UserRange::new(start, len).ok()?,
            if access_flags.contains(MappingFlags::WRITE) {
                PinAccess::Write
            } else {
                PinAccess::Read
            },
            duration,
            PinUse::BlockIo,
            aspace.user_io_pin_owner(),
        );
        aspace.begin_user_io_pin(request)
    };
    let (reservation, system_charge) = match admission {
        Ok(admission) => admission,
        Err(_) => {
            reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
            return None;
        }
    };
    let page_count = page_len / PAGE_SIZE_4K;
    let Some(mut preparation) = UnpublishedUserIoPin::try_new(
        aspace_handle.clone(),
        reservation,
        system_charge,
        page_count,
    ) else {
        reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
        return None;
    };
    // A page can introduce one physical SG fragment. Reserve the complete
    // admitted page-count bound before taking any AddrSpace lock; push_or_merge
    // is then allocation-free in every mapping scan window.
    let Some(mut segments) = UserIoPinSegments::try_new(page_count) else {
        reject_user_io_pin(&USER_IO_PIN_REJECT_SEGMENTS);
        reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
        return None;
    };

    let populate_windows = duration == PinDuration::LongTerm;

    // The initial lock scope only installs the full-range Reserved barrier and
    // its system charge. Resident-only callers build mapping expectations in
    // bounded windows, using storage reserved outside every address-space lock.
    // Long-term callers collect each expectation after populating its window.
    // The Reserved record blocks overlapping topology/access mutations across
    // the gaps between windows.
    if !populate_windows {
        let mut expectation_cursor = page_start;
        while expectation_cursor < page_end {
            let chunk_end = user_io_pin_scan_chunk_end(expectation_cursor, page_end);
            let expectation_result = {
                let mut aspace = super::lock_mm_diagnosed!(aspace_handle, UserPinExpectation);
                aspace.append_user_io_mapping_expectations(
                    expectation_cursor,
                    chunk_end - expectation_cursor,
                    access_flags,
                    &mut preparation.expectations,
                )
            };
            if expectation_result.is_err() {
                reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
                reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
                return None;
            }
            expectation_cursor = chunk_end;
        }
    }
    let mut needs_frame_registry = preparation
        .expectations()
        .iter()
        .any(UserIoMappingExpectation::needs_frame_registry);
    // Long-term windows may populate a previously absent anonymous/COW page
    // while the AddrSpace lock is held. Prepare the physical owner registry
    // before that lock is acquired; ordinary direct-I/O remains resident-only.
    if (needs_frame_registry || populate_windows) && prepare_physical_pin_registry().is_err() {
        reject_user_io_pin(&USER_IO_PIN_REJECT_FRAME_PIN);
        reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
        return None;
    }

    // Reuse one preallocated window owner list. At most one distinct VMA can
    // begin per scanned page, so this bound makes every push infallible while
    // the address-space lock is held.
    let mut pin_windows = Vec::new();
    if pin_windows
        .try_reserve_exact(USER_IO_PIN_SCAN_CHUNK_PAGES)
        .is_err()
    {
        reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
        return None;
    }

    // This direct-I/O path remains resident-only. Each critical section first
    // identifies at most 64 mapped pages and acquires their exact lower owners
    // before releasing the address-space lock. In particular, COW frames are
    // never queried, unlocked, and only then pinned: a concurrent write fault
    // could otherwise replace and free the observed frame in that gap.
    let mut copied = 0usize;
    let mut cow_frame_pages = 0usize;
    let mut shared_frame_pages = 0usize;
    let mut frame_pin_attempted = false;
    let mut validated_expectations = 0usize;
    let mut scan_cursor = page_start;
    while scan_cursor < page_end {
        let chunk_end = user_io_pin_scan_chunk_end(scan_cursor, page_end);
        let chunk_pages = (chunk_end - scan_cursor) / PAGE_SIZE_4K;
        // Both address and deferred-free owners are allocated before taking
        // AddrSpace. File-only requests use a zero-capacity preparation and do
        // not pay for physical-registry batch storage.
        let frame_capacity = if needs_frame_registry || populate_windows {
            chunk_pages
        } else {
            0
        };
        let mut frame_preparation = match PreparedPhysicalFramePins::try_new(frame_capacity) {
            Ok(preparation) => preparation,
            Err(_) => {
                reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
                return None;
            }
        };

        let chunk_pin = {
            let mut aspace = super::lock_mm_diagnosed!(aspace_handle, UserPinCollectOwners);
            (|| {
                if populate_windows {
                    if aspace
                        .populate_area(scan_cursor, chunk_end - scan_cursor, access_flags)
                        .is_err()
                    {
                        reject_user_io_pin(&USER_IO_PIN_REJECT_POPULATE);
                        return None;
                    }
                    let expectation_start = preparation.expectations.len();
                    aspace
                        .append_user_io_mapping_expectations(
                            scan_cursor,
                            chunk_end - scan_cursor,
                            access_flags,
                            &mut preparation.expectations,
                        )
                        .map_err(|_| {
                            reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
                        })
                        .ok()?;
                    if preparation.expectations()[expectation_start..]
                        .iter()
                        .any(UserIoMappingExpectation::needs_frame_registry)
                    {
                        needs_frame_registry = true;
                    }
                }

                let mut window_cursor = scan_cursor;
                while window_cursor < chunk_end {
                    let Some(area) = aspace.find_area(window_cursor) else {
                        reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
                        return None;
                    };
                    if area.start() > window_cursor {
                        reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
                        return None;
                    }
                    match area.backend().begin_user_io_pin_window() {
                        Ok(Some(window)) => pin_windows.push(window),
                        Ok(None) => {}
                        Err(_) => {
                            reject_user_io_pin(&USER_IO_PIN_REJECT_FILE_PIN);
                            return None;
                        }
                    }
                    window_cursor = area.end().min(chunk_end);
                }

                let mut vaddr = scan_cursor;
                while vaddr < chunk_end {
                    let Ok((paddr, flags, page_size)) = aspace.page_table().query(vaddr) else {
                        reject_user_io_pin(&USER_IO_PIN_REJECT_PAGETABLE);
                        return None;
                    };
                    if page_size != PageSize::Size4K || !flags.contains(access_flags) {
                        reject_user_io_pin(&USER_IO_PIN_REJECT_PAGETABLE);
                        return None;
                    }
                    let Some(area) = aspace.find_area(vaddr) else {
                        reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
                        return None;
                    };
                    let backend = area.backend();
                    preparation.observe_backend(backend);
                    if backend.supports_user_io_frame_pin() {
                        match backend {
                            Backend::Cow(_) => {
                                cow_frame_pages += 1;
                                preparation.cow_frames.push(paddr);
                            }
                            Backend::Shared(_) => shared_frame_pages += 1,
                            Backend::Linear(_) | Backend::File(_) => unreachable!(),
                        }
                        if !frame_preparation.push(paddr) {
                            reject_user_io_pin(&USER_IO_PIN_REJECT_FRAME_PIN);
                            return None;
                        }
                    } else {
                        record_user_io_pin_counter(&USER_IO_PIN_PAGE_CACHE_PIN_ATTEMPTS, 1);
                        match backend.pin_user_io_page_cache(
                            vaddr,
                            paddr,
                            access_flags.contains(MappingFlags::WRITE),
                        ) {
                            Ok(Some(pin)) => {
                                record_user_io_backend_pin_hit(backend);
                                preparation.page_cache_pins.push(pin);
                            }
                            Ok(None) => {
                                record_user_io_backend_pin_reject(backend);
                                reject_user_io_pin(&USER_IO_PIN_REJECT_FRAME_PIN);
                                return None;
                            }
                            Err(_) => {
                                record_user_io_backend_pin_reject(backend);
                                reject_user_io_pin(&USER_IO_PIN_REJECT_PAGE_CACHE_PIN);
                                return None;
                            }
                        }
                    }

                    let page_offset = if vaddr == page_start {
                        start_addr.sub_addr(page_start)
                    } else {
                        0
                    };
                    let chunk_len = (len - copied).min(PAGE_SIZE_4K - page_offset);
                    if !segments.push_or_merge(paddr.as_usize() + page_offset, chunk_len) {
                        reject_user_io_pin(&USER_IO_PIN_REJECT_SEGMENTS);
                        return None;
                    }
                    copied += chunk_len;
                    vaddr += PAGE_SIZE_4K;
                }

                if frame_preparation.is_empty() {
                    return Some(None);
                }
                if !frame_pin_attempted {
                    record_user_io_pin_counter(&USER_IO_PIN_FRAME_PIN_ATTEMPTS, 1);
                    frame_pin_attempted = true;
                }
                let system_charge = match preparation.admit_frame_pages(frame_preparation.len()) {
                    Ok(charge) => charge,
                    Err(_) => {
                        reject_user_io_pin(&USER_IO_PIN_REJECT_FRAME_PIN);
                        return None;
                    }
                };
                match frame_preparation.publish(system_charge) {
                    Ok(pins) => Some(Some(pins)),
                    Err(_) => {
                        if cow_frame_pages != 0 {
                            reject_user_io_pin(&USER_IO_PIN_REJECT_COW_PIN);
                        }
                        if shared_frame_pages != 0 {
                            reject_user_io_pin(&USER_IO_PIN_REJECT_SHARED_PIN);
                        }
                        reject_user_io_pin(&USER_IO_PIN_REJECT_FRAME_PIN);
                        None
                    }
                }
            })()
        };

        // Exact page pins now replace these conservative file-wide windows.
        // Drop them without holding the address-space lock.
        pin_windows.clear();
        let Some(chunk_pin) = chunk_pin else {
            reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
            return None;
        };
        if let Some(chunk_pin) = chunk_pin {
            preparation.frame_pins.push(chunk_pin);
        }

        // Revalidate only after every lower owner for this window has been
        // acquired. The live Reserved record rejects overlapping unmap,
        // protect, remap, and discard publication between windows, so releasing
        // AddrSpace here cannot make an earlier validated prefix stale.
        let validation = {
            let mut aspace = super::lock_mm_diagnosed!(aspace_handle, UserPinRevalidate);
            aspace.revalidate_user_io_pin_window(
                preparation.reservation(),
                &preparation.expectations()[validated_expectations..],
                scan_cursor,
                chunk_end - scan_cursor,
            )
        };
        let consumed = match validation {
            Ok(consumed) => consumed,
            Err(_) => {
                reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
                return None;
            }
        };
        validated_expectations = match validated_expectations.checked_add(consumed) {
            Some(validated) => validated,
            None => {
                reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
                return None;
            }
        };
        scan_cursor = chunk_end;
    }

    if preparation.frame_pins.is_empty() && preparation.page_cache_pins.is_empty() {
        reject_user_io_pin(&USER_IO_PIN_REJECT_FRAME_PIN);
        reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
        return None;
    }
    if copied != len {
        reject_user_io_pin(&USER_IO_PIN_REJECT_PAGETABLE);
        reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
        return None;
    }
    if require_contiguous && segments.len() != 1 {
        reject_user_io_pin(&USER_IO_PIN_REJECT_NONCONTIG);
        reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
        return None;
    }
    debug_assert_eq!(frame_pin_attempted, needs_frame_registry);
    record_user_io_pin_counter(&USER_IO_PIN_COW_PIN_PAGES, cow_frame_pages as u64);
    record_user_io_pin_counter(&USER_IO_PIN_SHARED_PIN_PAGES, shared_frame_pages as u64);
    let frame_pin_pages = preparation
        .frame_pins
        .iter()
        .map(PhysicalFramePins::len)
        .sum::<usize>();
    let page_cache_pin_pages = preparation.page_cache_pins.len();
    debug_assert_eq!(frame_pin_pages + page_cache_pin_pages, page_count);
    if validated_expectations != preparation.expectations().len() {
        reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
        return None;
    }

    // All per-page and per-VMA work completed in bounded windows. The final
    // policy publication is now only a constant-time Reserved -> Active state
    // transition. Keep the guard in an explicit scope so an error cannot drop
    // unpublished owners and recursively cancel while AddrSpace is still held.
    let publication = {
        let mut aspace = super::lock_mm_diagnosed!(aspace_handle, UserPinCommit);
        aspace.commit_user_io_pin(preparation.reservation(), &mut preparation.cow_frames)
    };
    let token = match publication {
        Ok(token) => token,
        Err(_) => {
            reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
            return None;
        }
    };
    // Disarm unpublished cleanup immediately after the core state transition;
    // everything below is infallible bookkeeping over the published RAII pin.
    let prepared = preparation.finish(segments, token);
    record_user_io_pin_counter(&USER_IO_PIN_VM_RANGE_PIN_HITS, 1);
    record_user_io_pin_counter(&USER_IO_PIN_VM_RANGE_PIN_BYTES, page_len as u64);
    if frame_pin_pages != 0 {
        record_user_io_pin_counter(&USER_IO_PIN_FRAME_PIN_HITS, 1);
        record_user_io_pin_counter(&USER_IO_PIN_FRAME_PIN_PAGES, frame_pin_pages as u64);
        record_user_io_pin_counter(&USER_IO_PIN_FRAME_PIN_BYTES, page_len as u64);
    }
    if page_cache_pin_pages != 0 {
        record_user_io_pin_counter(&USER_IO_PIN_PAGE_CACHE_PIN_HITS, 1);
        record_user_io_pin_counter(
            &USER_IO_PIN_PAGE_CACHE_PIN_PAGES,
            page_cache_pin_pages as u64,
        );
        record_user_io_pin_counter(&USER_IO_PIN_PAGE_CACHE_PIN_BYTES, page_len as u64);
    }

    #[cfg(feature = "test-io-control")]
    {
        let delay_ms = user_io_pin_test_delay_ms();
        if delay_ms != 0
            && user_io_pin_counters_enabled()
            && sleep(Duration::from_millis(delay_ms)).is_err()
        {
            reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
            return None;
        }
    }

    Some(prepared)
}

/// Short-lived source slice for direct file I/O.
///
/// This is intentionally stricter than normal user-copy helpers: it only
/// accepts already-resident pages and succeeds when all covered 4 KiB pages
/// are physically contiguous. It is a syscall-local borrow used by synchronous
/// I/O, not a long-term DMA pin that survives remap/unmap activity.
pub struct PinnedUserSlice {
    _ptr: *const u8,
    segments: UserIoPinSegments,
    _frame_pins: UserIoFramePins,
    _page_cache_pins: UserIoPageCachePins,
    _range_pin: UserIoRangePin,
}

impl PinnedUserSlice {
    pub fn segments(&self) -> &[UserIoPinSegment] {
        self.segments.as_slice()
    }
}

impl Drop for PinnedUserSlice {
    fn drop(&mut self) {
        record_user_io_pin_counter(&USER_IO_PIN_UNPINS, 1);
    }
}

/// Short-lived destination slice for direct file I/O.
pub struct PinnedUserSliceMut {
    _ptr: *mut u8,
    segments: UserIoPinSegments,
    _frame_pins: UserIoFramePins,
    _page_cache_pins: UserIoPageCachePins,
    _range_pin: UserIoRangePin,
}

impl PinnedUserSliceMut {
    pub fn segments(&self) -> &[UserIoPinSegment] {
        self.segments.as_slice()
    }
}

impl Drop for PinnedUserSliceMut {
    fn drop(&mut self) {
        record_user_io_pin_counter(&USER_IO_PIN_UNPINS, 1);
    }
}

/// Attempts a resident contiguous source pin in an explicitly selected
/// address space.
pub fn try_pin_user_slice_from_user_with(
    capability: &UserMemoryCapability,
    ptr: *const u8,
    len: usize,
) -> Option<PinnedUserSlice> {
    record_user_io_pin_counter(&USER_IO_PIN_FROM_USER_ATTEMPTS, 1);
    let prepared =
        prepare_user_io_pin_with(capability, ptr as usize, len, MappingFlags::READ, true)?;
    record_user_io_pin_counter(&USER_IO_PIN_FROM_USER_HITS, 1);
    record_user_io_pin_counter(&USER_IO_PIN_FROM_USER_BYTES, len as u64);
    record_user_io_pin_segments(&prepared.segments);
    Some(PinnedUserSlice {
        _ptr: ptr,
        segments: prepared.segments,
        _frame_pins: prepared.frame_pins,
        _page_cache_pins: prepared.page_cache_pins,
        _range_pin: prepared.range_pin,
    })
}

/// Attempts a resident contiguous destination pin in an explicitly selected
/// address space.
pub fn try_pin_user_slice_to_user_with(
    capability: &UserMemoryCapability,
    ptr: *mut u8,
    len: usize,
) -> Option<PinnedUserSliceMut> {
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_ATTEMPTS, 1);
    let prepared =
        prepare_user_io_pin_with(capability, ptr as usize, len, MappingFlags::WRITE, true)?;
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_HITS, 1);
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_BYTES, len as u64);
    record_user_io_pin_segments(&prepared.segments);
    Some(PinnedUserSliceMut {
        _ptr: ptr,
        segments: prepared.segments,
        _frame_pins: prepared.frame_pins,
        _page_cache_pins: prepared.page_cache_pins,
        _range_pin: prepared.range_pin,
    })
}

#[allow(dead_code)]
pub struct PinnedUserSegments {
    _ptr: *const u8,
    segments: UserIoPinSegments,
    _frame_pins: UserIoFramePins,
    _page_cache_pins: UserIoPageCachePins,
    _range_pin: UserIoRangePin,
}

#[allow(dead_code)]
impl PinnedUserSegments {
    pub fn segments(&self) -> &[UserIoPinSegment] {
        self.segments.as_slice()
    }
}

impl Drop for PinnedUserSegments {
    fn drop(&mut self) {
        record_user_io_pin_counter(&USER_IO_PIN_UNPINS, 1);
    }
}

#[allow(dead_code)]
pub struct PinnedUserSegmentsMut {
    _ptr: *mut u8,
    segments: UserIoPinSegments,
    provenance: UserIoPinProvenance,
    _frame_pins: UserIoFramePins,
    _page_cache_pins: UserIoPageCachePins,
    _range_pin: UserIoRangePin,
}

#[allow(dead_code)]
impl PinnedUserSegmentsMut {
    pub fn segments(&self) -> &[UserIoPinSegment] {
        self.segments.as_slice()
    }

    /// Returns the aggregate provenance captured by this pin's page scan.
    pub fn provenance(&self) -> UserIoPinProvenance {
        self.provenance
    }
}

impl Drop for PinnedUserSegmentsMut {
    fn drop(&mut self) {
        record_user_io_pin_counter(&USER_IO_PIN_UNPINS, 1);
    }
}

pub fn pinned_user_mut_segments_are_disjoint(pins: &[PinnedUserSegmentsMut]) -> bool {
    physical_pin_segments_are_disjoint(pins.iter().flat_map(|pin| pin.segments.as_slice().iter()))
}

/// Returns whether a physical SG stream has no overlapping byte ranges.
/// Mutable axfs pinned destinations require this property; callers can use
/// the raw physical cursor when an intentionally aliased mapping is selected.
pub fn physical_segments_are_disjoint(segments: &[UserIoPinSegment]) -> bool {
    physical_pin_segments_are_disjoint(segments.iter())
}

struct PinnedPhysicalCursor<'a> {
    segments: &'a [UserIoPinSegment],
    index: usize,
    offset: usize,
    remaining: usize,
}

impl<'a> PinnedPhysicalCursor<'a> {
    fn new(segments: &'a [UserIoPinSegment], offset: usize, len: usize) -> Option<Self> {
        let total = segments
            .iter()
            .try_fold(0usize, |total, segment| total.checked_add(segment.len))?;
        if offset.checked_add(len)? > total {
            return None;
        }
        let mut cursor = Self {
            segments,
            index: 0,
            offset: 0,
            remaining: total,
        };
        let mut skipped = offset;
        while skipped != 0 {
            let (_, part) = cursor.take(skipped)?;
            skipped -= part;
        }
        cursor.remaining = len;
        Some(cursor)
    }

    fn take(&mut self, limit: usize) -> Option<(usize, usize)> {
        while let Some(segment) = self.segments.get(self.index).copied() {
            if self.offset == segment.len {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let len = limit
                .min(self.remaining)
                .min(segment.len.checked_sub(self.offset)?);
            let paddr = segment.paddr.checked_add(self.offset)?;
            paddr.checked_add(len)?;
            self.offset = self.offset.checked_add(len)?;
            self.remaining = self.remaining.checked_sub(len)?;
            return Some((paddr, len));
        }
        None
    }

    fn remaining(&self) -> usize {
        self.remaining
    }
}

/// A raw physical scatter/gather source used while its MM pin owner is held.
///
/// This adapter is intentionally byte-oriented.  It does not expose a Rust
/// slice into physical memory, avoiding a long-lived Rust alias while still
/// allowing stream and generic file backends to consume registered pages
/// without re-resolving their virtual addresses.
pub struct PinnedPhysicalReader<'a> {
    cursor: PinnedPhysicalCursor<'a>,
}

impl<'a> PinnedPhysicalReader<'a> {
    pub fn new(segments: &'a [UserIoPinSegment], offset: usize, len: usize) -> Option<Self> {
        Some(Self {
            cursor: PinnedPhysicalCursor::new(segments, offset, len)?,
        })
    }

    /// Constructs a cursor for a range already bounded by its pin owner.
    /// The caller must retain that owner and provide a descriptor suffix that
    /// covers `offset + len` bytes.
    pub(crate) fn from_validated_range(
        segments: &'a [UserIoPinSegment],
        offset: usize,
        len: usize,
    ) -> Self {
        Self {
            cursor: PinnedPhysicalCursor {
                segments,
                index: 0,
                offset,
                remaining: len,
            },
        }
    }
}

impl axio::Read for PinnedPhysicalReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> axio::Result<usize> {
        let target = buf.len().min(self.cursor.remaining());
        let mut copied = 0usize;
        while copied < target {
            let (paddr, len) = self
                .cursor
                .take(target - copied)
                .ok_or(AxError::InvalidInput)?;
            let src = phys_to_virt(memory_addr::PhysAddr::from(paddr)).as_ptr();
            // SAFETY: the caller holds the long-term pin for every descriptor
            // and the physical addresses were produced by the MM pin scan.
            unsafe { ptr::copy(src, buf.as_mut_ptr().add(copied), len) };
            copied += len;
        }
        Ok(copied)
    }
}

impl axio::IoBuf for PinnedPhysicalReader<'_> {
    fn remaining(&self) -> usize {
        self.cursor.remaining()
    }
}

/// A raw physical scatter/gather destination used while its MM pin owner is
/// held.  See [`PinnedPhysicalReader`] for the aliasing contract.
pub struct PinnedPhysicalWriter<'a> {
    cursor: PinnedPhysicalCursor<'a>,
}

impl<'a> PinnedPhysicalWriter<'a> {
    pub fn new(segments: &'a [UserIoPinSegment], offset: usize, len: usize) -> Option<Self> {
        Some(Self {
            cursor: PinnedPhysicalCursor::new(segments, offset, len)?,
        })
    }

    /// See [`PinnedPhysicalReader::from_validated_range`].
    pub(crate) fn from_validated_range(
        segments: &'a [UserIoPinSegment],
        offset: usize,
        len: usize,
    ) -> Self {
        Self {
            cursor: PinnedPhysicalCursor {
                segments,
                index: 0,
                offset,
                remaining: len,
            },
        }
    }
}

impl axio::Write for PinnedPhysicalWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> axio::Result<usize> {
        let target = buf.len().min(self.cursor.remaining());
        let mut copied = 0usize;
        while copied < target {
            let (paddr, len) = self
                .cursor
                .take(target - copied)
                .ok_or(AxError::InvalidInput)?;
            let dst = phys_to_virt(memory_addr::PhysAddr::from(paddr)).as_mut_ptr();
            // SAFETY: the caller holds the long-term pin for every descriptor
            // and the physical addresses were produced by the MM pin scan.
            unsafe { ptr::copy(buf.as_ptr().add(copied), dst, len) };
            copied += len;
        }
        Ok(copied)
    }

    fn flush(&mut self) -> axio::Result<()> {
        Ok(())
    }
}

impl axio::IoBufMut for PinnedPhysicalWriter<'_> {
    fn remaining_mut(&self) -> usize {
        self.cursor.remaining()
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};
    use axhal::paging::{MappingFlags, PageSize, PagingError};
    use axio::{IoBuf, IoBufMut};
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::{
        Backend, FutexMappingNamespace, PinnedPhysicalCursor, PinnedPhysicalReader,
        PinnedPhysicalWriter, USER_IO_PIN_SCAN_CHUNK_PAGES, UserIoPinProvenance, UserIoPinSegment,
        UserIoPinSegments, UserNofaultError, classify_nofault_page, classify_nofault_query,
        futex_mapping_namespace_matches, join_user_io_pin_provenance,
        physical_pin_segments_are_disjoint, user_io_pin_provenance_for_backend,
        user_io_pin_scan_chunk_end,
    };
    use crate::mm::SharedPages;

    #[test]
    fn user_io_pin_provenance_is_private_only_for_anonymous_cow() {
        let anonymous = Backend::new_alloc(VirtAddr::from(0x4000), PageSize::Size4K);
        assert_eq!(
            user_io_pin_provenance_for_backend(&anonymous),
            UserIoPinProvenance::PrivateAnonymous
        );

        let fs = crate::pseudofs::tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount
            .root_location()
            .create(
                "pin-provenance-file-cow",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file_private = Backend::new_cow(
            VirtAddr::from(0x8000),
            PageSize::Size4K,
            location,
            0,
            None,
            false,
        );
        assert_eq!(
            user_io_pin_provenance_for_backend(&file_private),
            UserIoPinProvenance::Ineligible
        );
    }

    #[test]
    fn user_io_pin_provenance_rejects_other_and_mixed_backends() {
        let shared = Backend::new_shared(
            VirtAddr::from(0x4000),
            Arc::new(SharedPages::new(0, PageSize::Size4K).unwrap()),
        );
        let linear = Backend::new_linear(
            VirtAddr::from(0x8000),
            memory_addr::PhysAddr::from(0x10_000),
            PAGE_SIZE_4K,
        );
        assert_eq!(
            user_io_pin_provenance_for_backend(&shared),
            UserIoPinProvenance::Ineligible
        );
        assert_eq!(
            user_io_pin_provenance_for_backend(&linear),
            UserIoPinProvenance::Ineligible
        );
        assert_eq!(
            join_user_io_pin_provenance(
                join_user_io_pin_provenance(None, UserIoPinProvenance::PrivateAnonymous),
                UserIoPinProvenance::PrivateAnonymous,
            ),
            Some(UserIoPinProvenance::PrivateAnonymous)
        );
        assert_eq!(
            join_user_io_pin_provenance(
                Some(UserIoPinProvenance::PrivateAnonymous),
                UserIoPinProvenance::Ineligible,
            ),
            Some(UserIoPinProvenance::Ineligible)
        );
    }

    #[test]
    fn user_io_pin_provenance_defaults_conservatively() {
        assert_eq!(
            UserIoPinProvenance::default(),
            UserIoPinProvenance::Ineligible
        );
        assert_eq!(
            join_user_io_pin_provenance(None, UserIoPinProvenance::Ineligible),
            Some(UserIoPinProvenance::Ineligible)
        );
    }

    #[test]
    fn user_io_pin_scan_windows_never_exceed_the_page_bound() {
        let start = VirtAddr::from(0x1000);
        let end = start + (USER_IO_PIN_SCAN_CHUNK_PAGES * 2 + 1) * PAGE_SIZE_4K;

        let first = user_io_pin_scan_chunk_end(start, end);
        let second = user_io_pin_scan_chunk_end(first, end);
        let third = user_io_pin_scan_chunk_end(second, end);

        assert_eq!(first - start, USER_IO_PIN_SCAN_CHUNK_PAGES * PAGE_SIZE_4K);
        assert_eq!(second - first, USER_IO_PIN_SCAN_CHUNK_PAGES * PAGE_SIZE_4K);
        assert_eq!(third - second, PAGE_SIZE_4K);
        assert_eq!(third, end);
    }

    #[test]
    fn mutable_pin_segments_reject_physical_aliases() {
        let mut aliases = UserIoPinSegments::try_new(2).unwrap();
        assert!(aliases.push_or_merge(0x1000, 0x800));
        assert!(aliases.push_or_merge(0x1000, 0x800));
        assert!(!aliases.physical_ranges_are_disjoint());

        let mut disjoint = UserIoPinSegments::try_new(2).unwrap();
        assert!(disjoint.push_or_merge(0x1000, 0x800));
        assert!(disjoint.push_or_merge(0x2000, 0x800));
        assert!(disjoint.physical_ranges_are_disjoint());
    }

    #[test]
    fn fragmented_pin_segments_reserve_and_accept_more_than_32_fragments() {
        let mut segments = UserIoPinSegments::try_new(33).unwrap();
        for index in 0..33 {
            assert!(segments.push_or_merge(0x1000 + index * 0x2000, PAGE_SIZE_4K));
        }
        assert_eq!(segments.len(), 33);
        assert_eq!(segments.bytes(), 33 * PAGE_SIZE_4K);
        assert!(segments.physical_ranges_are_disjoint());
        assert!(!segments.push_or_merge(0x1000 + 33 * 0x2000, PAGE_SIZE_4K));
    }

    #[test]
    fn physical_adjacent_pages_merge_with_checked_accounting() {
        let mut segments = UserIoPinSegments::try_new(64).unwrap();
        for index in 0..64 {
            assert!(segments.push_or_merge(0x20_000 + index * PAGE_SIZE_4K, PAGE_SIZE_4K));
        }
        assert_eq!(segments.len(), 1);
        assert_eq!(segments.as_slice()[0].paddr, 0x20_000);
        assert_eq!(segments.as_slice()[0].len, 256 * 1024);
        assert_eq!(segments.bytes(), 256 * 1024);
    }

    #[test]
    fn mutable_pin_range_check_handles_cross_pin_aliases_without_storage() {
        let left = [UserIoPinSegment {
            paddr: 0x1000,
            len: 0x1000,
        }];
        let disjoint = [UserIoPinSegment {
            paddr: 0x3000,
            len: 0x1000,
        }];
        assert!(physical_pin_segments_are_disjoint(
            left.iter().chain(disjoint.iter())
        ));

        let overlapping = [UserIoPinSegment {
            paddr: 0x1800,
            len: 0x1000,
        }];
        assert!(!physical_pin_segments_are_disjoint(
            left.iter().chain(overlapping.iter())
        ));

        let overflowing = [UserIoPinSegment {
            paddr: usize::MAX,
            len: 2,
        }];
        assert!(!physical_pin_segments_are_disjoint(overflowing.iter()));
    }

    #[test]
    fn physical_cursor_clips_edges_and_crosses_segments_without_allocation() {
        let segments = [
            UserIoPinSegment {
                paddr: 0x1000,
                len: 4,
            },
            UserIoPinSegment {
                paddr: 0x9000,
                len: 8,
            },
        ];
        let mut cursor = PinnedPhysicalCursor::new(&segments, 2, 7).unwrap();
        assert_eq!(cursor.take(7), Some((0x1002, 2)));
        assert_eq!(cursor.take(7), Some((0x9000, 5)));
        assert_eq!(cursor.remaining(), 0);
        assert!(PinnedPhysicalCursor::new(&segments, usize::MAX, 1).is_none());
        assert!(PinnedPhysicalCursor::new(&segments, 0, 13).is_none());
        let overflowing = [UserIoPinSegment {
            paddr: usize::MAX,
            len: 2,
        }];
        let mut cursor = PinnedPhysicalCursor::new(&overflowing, 0, 2).unwrap();
        assert_eq!(cursor.take(2), None);
    }

    #[test]
    fn physical_adapters_keep_checked_cursor_bounds() {
        let segments = [
            UserIoPinSegment {
                paddr: 0x1000,
                len: 4,
            },
            UserIoPinSegment {
                paddr: 0x9000,
                len: 8,
            },
        ];
        assert_eq!(
            PinnedPhysicalReader::new(&segments, 3, 6)
                .unwrap()
                .remaining(),
            6
        );
        assert_eq!(
            PinnedPhysicalWriter::new(&segments, 3, 6)
                .unwrap()
                .remaining_mut(),
            6
        );
        assert!(PinnedPhysicalReader::new(&segments, 13, 1).is_none());
        assert!(PinnedPhysicalWriter::new(&segments, 0, usize::MAX).is_none());
    }

    #[test]
    fn nofault_missing_leaf_requires_task_context_progress() {
        assert_eq!(
            classify_nofault_query(PagingError::NotMapped),
            UserNofaultError::Retry
        );
        for error in [
            PagingError::NoMemory,
            PagingError::NotAligned,
            PagingError::AlreadyMapped,
            PagingError::MappedToHugePage,
            PagingError::NotPromotable,
            PagingError::RollbackMismatch,
        ] {
            assert_eq!(classify_nofault_query(error), UserNofaultError::BadAddress);
        }
    }

    #[test]
    fn nofault_writable_cow_leaf_requests_write_fault() {
        let cow_leaf = MappingFlags::USER | MappingFlags::READ;
        assert_eq!(
            classify_nofault_page(cow_leaf, PageSize::Size4K, MappingFlags::WRITE),
            Err(UserNofaultError::Retry)
        );
        assert_eq!(
            classify_nofault_page(cow_leaf, PageSize::Size4K, MappingFlags::READ),
            Ok(())
        );
        assert_eq!(
            classify_nofault_page(
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                PageSize::Size4K,
                MappingFlags::WRITE,
            ),
            Ok(())
        );
        assert_eq!(
            classify_nofault_page(cow_leaf, PageSize::Size2M, MappingFlags::READ),
            Err(UserNofaultError::BadAddress)
        );
    }

    #[test]
    fn futex_namespace_race_rejects_private_to_shared_or_unmapped() {
        assert!(futex_mapping_namespace_matches(
            FutexMappingNamespace::Private,
            FutexMappingNamespace::Private,
        ));
        assert!(!futex_mapping_namespace_matches(
            FutexMappingNamespace::Private,
            FutexMappingNamespace::Shared,
        ));
        assert!(!futex_mapping_namespace_matches(
            FutexMappingNamespace::Private,
            FutexMappingNamespace::Unmapped,
        ));
    }
}

#[allow(dead_code)]
/// Attempts a resident source scatter/gather pin in an explicitly selected
/// address space.
pub fn try_pin_user_segments_from_user_with(
    capability: &UserMemoryCapability,
    ptr: *const u8,
    len: usize,
) -> Option<PinnedUserSegments> {
    record_user_io_pin_counter(&USER_IO_PIN_FROM_USER_ATTEMPTS, 1);
    let prepared =
        prepare_user_io_pin_with(capability, ptr as usize, len, MappingFlags::READ, false)?;
    record_user_io_pin_counter(&USER_IO_PIN_FROM_USER_HITS, 1);
    record_user_io_pin_counter(&USER_IO_PIN_FROM_USER_BYTES, len as u64);
    record_user_io_pin_segments(&prepared.segments);
    Some(PinnedUserSegments {
        _ptr: ptr,
        segments: prepared.segments,
        _frame_pins: prepared.frame_pins,
        _page_cache_pins: prepared.page_cache_pins,
        _range_pin: prepared.range_pin,
    })
}

#[allow(dead_code)]
/// Attempts a resident destination scatter/gather pin in an explicitly
/// selected address space.
pub fn try_pin_user_segments_to_user_with(
    capability: &UserMemoryCapability,
    ptr: *mut u8,
    len: usize,
) -> Option<PinnedUserSegmentsMut> {
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_ATTEMPTS, 1);
    let prepared =
        prepare_user_io_pin_with(capability, ptr as usize, len, MappingFlags::WRITE, false)?;
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_HITS, 1);
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_BYTES, len as u64);
    record_user_io_pin_segments(&prepared.segments);
    Some(PinnedUserSegmentsMut {
        _ptr: ptr,
        segments: prepared.segments,
        provenance: prepared.provenance,
        _frame_pins: prepared.frame_pins,
        _page_cache_pins: prepared.page_cache_pins,
        _range_pin: prepared.range_pin,
    })
}

#[allow(dead_code)]
/// Attempts a long-term destination scatter/gather pin for registered
/// asynchronous I/O. Unlike the direct-I/O helper above, this path faults in
/// each bounded window before acquiring its lower-level owners and publishes
/// the reservation with [`PinDuration::LongTerm`].
pub fn try_pin_user_segments_to_user_longterm_with(
    capability: &UserMemoryCapability,
    ptr: *mut u8,
    len: usize,
) -> Option<PinnedUserSegmentsMut> {
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_ATTEMPTS, 1);
    let prepared = prepare_user_io_pin_with_duration(
        capability,
        ptr as usize,
        len,
        MappingFlags::WRITE,
        false,
        PinDuration::LongTerm,
    )?;
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_HITS, 1);
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_BYTES, len as u64);
    record_user_io_pin_segments(&prepared.segments);
    Some(PinnedUserSegmentsMut {
        _ptr: ptr,
        segments: prepared.segments,
        provenance: prepared.provenance,
        _frame_pins: prepared.frame_pins,
        _page_cache_pins: prepared.page_cache_pins,
        _range_pin: prepared.range_pin,
    })
}

/// Checks and populates a readable range in an explicitly selected address
/// space.
pub fn check_user_readable_with(
    capability: &UserMemoryCapability,
    start: usize,
    len: usize,
) -> AxResult {
    populate_user_range_with(capability, start, len, MappingFlags::READ).map_err(map_usercopy_error)
}

/// Failure returned by the locked user-u32 nofault helpers.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UserU32NofaultError {
    /// The address-space/page-table snapshot raced with a mapping change or
    /// could not be acquired without blocking. The caller must leave any
    /// queue gates, fault/read in task context, and retry.
    Retry,
    /// The integer address is not an aligned, in-range user u32.
    BadAddress,
}

/// Mapping namespace captured while resolving a non-PRIVATE futex.
///
/// A private/COW VMA intentionally produces an address-space-local futex key.
/// Linux tags that non-PRIVATE resolution with `FUT_OFF_MMSHARED`, so it does
/// not alias an explicit `FUTEX_PRIVATE_FLAG` key at the same address.  The
/// mapping namespace is also kept separately so the former can be checked at
/// publication time; `None` in the shared-backing field must not disable that
/// check.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FutexMappingNamespace {
    Private,
    Shared,
    Unmapped,
}

/// Resolves the namespace of one futex word under an already-held address
/// space guard.  The result deliberately distinguishes an unmapped address
/// from a private VMA, so a stale private resolution cannot be published
/// after an unmap/remap race.
pub fn futex_mapping_namespace_at(aspace: &AddrSpace, start: usize) -> FutexMappingNamespace {
    let Some(end) = start.checked_add(size_of::<u32>()) else {
        return FutexMappingNamespace::Unmapped;
    };
    let Some(area) = aspace.find_area(VirtAddr::from_usize(start)) else {
        return FutexMappingNamespace::Unmapped;
    };
    if start < area.start().as_usize() || end > area.end().as_usize() {
        return FutexMappingNamespace::Unmapped;
    }
    match area.backend() {
        Backend::Shared(_) | Backend::File(_) => FutexMappingNamespace::Shared,
        Backend::Linear(_) | Backend::Cow(_) => FutexMappingNamespace::Private,
    }
}

#[inline]
fn futex_mapping_namespace_matches(
    expected: FutexMappingNamespace,
    actual: FutexMappingNamespace,
) -> bool {
    expected == actual
}

/// Validates the mapping identity and reads one futex word while the caller's
/// address-space guard is held.  A shared futex's expected backing/offset is
/// part of the same snapshot as the page-table translation; if either changed
/// after key derivation this returns `Retry`, never a value from the new
/// mapping.
pub fn try_read_user_u32_nofault_locked(
    aspace: &AddrSpace,
    start: usize,
    expected_namespace: Option<FutexMappingNamespace>,
    expected: Option<&SharedFutexKey>,
) -> Result<u32, UserU32NofaultError> {
    if start & (size_of::<u32>() - 1) != 0 {
        return Err(UserU32NofaultError::BadAddress);
    }
    validate_futex_mapping_locked(aspace, start, expected_namespace, expected)?;
    let mut bytes = [0; size_of::<u32>()];
    let mut pages = [NofaultPage::EMPTY; USER_NOFAULT_PAGE_SLOTS];
    let page_count = prepare_user_nofault_span(
        aspace,
        start,
        size_of::<u32>(),
        MappingFlags::READ,
        &mut pages,
    )
    .map_err(|error| match error {
        UserNofaultError::Retry => UserU32NofaultError::Retry,
        UserNofaultError::BadAddress => UserU32NofaultError::BadAddress,
    })?;
    copy_from_user_nofault_pages(start, &mut bytes, &pages[..page_count]);
    Ok(u32::from_ne_bytes(bytes))
}

/// Validates one futex mapping under an already-held address-space guard,
/// without reading its user value.  Requeue/wake paths use this for the target
/// address so an unconditional operation never faults or samples target data.
pub fn try_validate_futex_mapping_nofault_locked(
    aspace: &AddrSpace,
    start: usize,
    expected_namespace: Option<FutexMappingNamespace>,
    expected: Option<&SharedFutexKey>,
) -> Result<(), UserU32NofaultError> {
    if start & (size_of::<u32>() - 1) != 0 {
        return Err(UserU32NofaultError::BadAddress);
    }
    validate_futex_mapping_locked(aspace, start, expected_namespace, expected)?;
    let mut pages = [NofaultPage::EMPTY; USER_NOFAULT_PAGE_SLOTS];
    prepare_user_nofault_span(
        aspace,
        start,
        size_of::<u32>(),
        MappingFlags::READ,
        &mut pages,
    )
    .map(|_| ())
    .map_err(|error| match error {
        UserNofaultError::Retry => UserU32NofaultError::Retry,
        UserNofaultError::BadAddress => UserU32NofaultError::BadAddress,
    })
}

fn validate_futex_mapping_locked(
    aspace: &AddrSpace,
    start: usize,
    expected_namespace: Option<FutexMappingNamespace>,
    expected: Option<&SharedFutexKey>,
) -> Result<(), UserU32NofaultError> {
    let Some(expected_namespace) = expected_namespace else {
        // Explicit FUTEX_PRIVATE operations intentionally retain Linux's
        // address-space/address semantics and do not inspect the VMA here.
        return Ok(());
    };

    if !futex_mapping_namespace_matches(
        expected_namespace,
        futex_mapping_namespace_at(aspace, start),
    ) {
        return Err(UserU32NofaultError::Retry);
    }

    if expected_namespace == FutexMappingNamespace::Shared {
        let Some(expected) = expected else {
            return Err(UserU32NofaultError::Retry);
        };
        let Some((actual_id, actual_offset)) = aspace.futex_shared_id_at(start) else {
            return Err(UserU32NofaultError::Retry);
        };
        if actual_id != expected.backing().id() || actual_offset != expected.offset() {
            return Err(UserU32NofaultError::Retry);
        }
    }
    Ok(())
}

/// Maximum size accepted by the bounded fixed-span nofault helpers.
pub const USER_NOFAULT_MAX_SPAN: usize = 32;

const USER_NOFAULT_PAGE_SLOTS: usize = 2;

/// Failure returned by the bounded fixed-span nofault helpers.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UserNofaultError {
    /// The address-space lock or a resident page could not be acquired
    /// without blocking. The caller must return to task context and retry.
    Retry,
    /// The span is outside the selected user address space, has no suitable
    /// VMA, has an unrecoverable page-table state, or lacks the requested
    /// user/PTE permission.
    BadAddress,
}

/// A translation captured while the selected address-space lock is held.
#[derive(Clone, Copy)]
struct NofaultPage {
    start: usize,
    paddr: memory_addr::PhysAddr,
}

impl NofaultPage {
    const EMPTY: Self = Self {
        start: 0,
        paddr: memory_addr::PhysAddr::from_usize(0),
    };
}

/// Only an absent leaf is recoverable by the task-context population pass.
/// Other page-table errors are terminal for this bounded nofault operation.
#[inline]
fn classify_nofault_query(error: PagingError) -> UserNofaultError {
    match error {
        PagingError::NotMapped => UserNofaultError::Retry,
        PagingError::NoMemory
        | PagingError::NotAligned
        | PagingError::AlreadyMapped
        | PagingError::MappedToHugePage
        | PagingError::NotPromotable
        | PagingError::RollbackMismatch => UserNofaultError::BadAddress,
    }
}

/// A writable VMA with a read-only user leaf is the post-fork COW state. It
/// must leave the IRQ-safe transaction for `populate_area(WRITE)`; every other
/// permission or page-size mismatch is a one-shot fault.
#[inline]
fn classify_nofault_page(
    pte_flags: MappingFlags,
    page_size: PageSize,
    access_flags: MappingFlags,
) -> Result<(), UserNofaultError> {
    if page_size != PageSize::Size4K || !pte_flags.contains(MappingFlags::USER) {
        return Err(UserNofaultError::BadAddress);
    }
    if pte_flags.contains(access_flags) {
        return Ok(());
    }
    if access_flags.contains(MappingFlags::WRITE) {
        return Err(UserNofaultError::Retry);
    }
    Err(UserNofaultError::BadAddress)
}

/// Checks and captures every page needed by one fixed-span operation.
///
/// The returned array is stack-backed and has exactly two slots. `len` is
/// restricted to the rseq field sizes (4, 8, or 32 bytes), so a valid span
/// can never need more than two 4 KiB pages. A missing PTE is intentionally a
/// retry: this operation never populates or faults a page.
fn prepare_user_nofault_span(
    aspace: &AddrSpace,
    start: usize,
    len: usize,
    access_flags: MappingFlags,
    pages: &mut [NofaultPage; USER_NOFAULT_PAGE_SLOTS],
) -> Result<usize, UserNofaultError> {
    if !matches!(len, 4 | 8 | USER_NOFAULT_MAX_SPAN) {
        return Err(UserNofaultError::BadAddress);
    }
    let end = start.checked_add(len).ok_or(UserNofaultError::BadAddress)?;
    if !aspace.contains_range(VirtAddr::from(start), len) {
        return Err(UserNofaultError::BadAddress);
    }

    // A VMA check is separate from PTE translation: a registered rseq area or
    // descriptor may be in a valid VMA while one of its pages is still
    // nonresident. Requiring USER here also prevents a kernel-only mapping
    // accidentally exposed through a user virtual address from passing the
    // nofault gate.
    let required_vma_flags = access_flags | MappingFlags::USER;
    if !aspace.can_access_range(VirtAddr::from(start), len, required_vma_flags) {
        return Err(UserNofaultError::BadAddress);
    }

    let first_page = start & !(PAGE_SIZE_4K - 1);
    let last_page = (end - 1) & !(PAGE_SIZE_4K - 1);
    let page_count = if first_page == last_page { 1 } else { 2 };
    debug_assert!(page_count <= USER_NOFAULT_PAGE_SLOTS);

    for (index, page) in pages[..page_count].iter_mut().enumerate() {
        let page_start = first_page + index * PAGE_SIZE_4K;
        // Nofault/process_vm-style paths copy through the direct map and are
        // forbidden for secret frames.  They must fail rather than creating a
        // transient physical alias outside the per-CPU secret window.
        if aspace.areas().any(|area| {
            area.start().as_usize() <= page_start
                && page_start < area.end().as_usize()
                && area.backend().is_secret()
        }) {
            return Err(UserNofaultError::BadAddress);
        }
        let (paddr, pte_flags, page_size) =
            match aspace.page_table().query(VirtAddr::from(page_start)) {
                Ok(translation) => translation,
                // A nonresident page is recoverable by a task-context fault, but
                // this bounded operation must not perform that fault itself.
                Err(error) => return Err(classify_nofault_query(error)),
            };
        classify_nofault_page(pte_flags, page_size, access_flags)?;
        *page = NofaultPage {
            start: page_start,
            paddr,
        };
    }
    Ok(page_count)
}

/// Copies one already-resident fixed-size user span into kernel storage.
///
/// The address-space `Arc<Mutex<AddrSpace>>` is explicit so callers can select
/// the process image before entering an IRQ/queue gate. This helper uses
/// `try_lock`, performs no allocation, blocking, population, or fault, and
/// holds the same mutex guard across both translation and copy. Supported
/// lengths are exactly 4, 8, and 32 bytes, covering rseq scalar fields and
/// area/descriptor records.
pub fn try_read_user_nofault(
    start: usize,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    dst: &mut [u8],
) -> Result<(), UserNofaultError> {
    if !matches!(dst.len(), 4 | 8 | USER_NOFAULT_MAX_SPAN) {
        return Err(UserNofaultError::BadAddress);
    }
    let Some(aspace) = aspace_handle.try_lock() else {
        return Err(UserNofaultError::Retry);
    };
    let mut pages = [NofaultPage::EMPTY; USER_NOFAULT_PAGE_SLOTS];
    let page_count =
        prepare_user_nofault_span(&aspace, start, dst.len(), MappingFlags::READ, &mut pages)?;
    copy_from_user_nofault_pages(start, dst, &pages[..page_count]);
    Ok(())
}

/// Commits one fixed-size kernel span into already-resident user memory.
///
/// Every VMA/PTE translation and write permission is checked before the first
/// destination byte is changed. Consequently a missing or read-only second
/// page cannot leave a prefix of a 32-byte descriptor or area update visible.
/// The same address-space mutex guard remains held through validation and all
/// copies, so concurrent unmap/remap cannot invalidate the captured physical
/// addresses between the preflight and commit phases.
pub fn try_write_user_nofault(
    start: usize,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    src: &[u8],
) -> Result<(), UserNofaultError> {
    if !matches!(src.len(), 4 | 8 | USER_NOFAULT_MAX_SPAN) {
        return Err(UserNofaultError::BadAddress);
    }
    let Some(aspace) = aspace_handle.try_lock() else {
        return Err(UserNofaultError::Retry);
    };
    let mut pages = [NofaultPage::EMPTY; USER_NOFAULT_PAGE_SLOTS];
    let page_count =
        prepare_user_nofault_span(&aspace, start, src.len(), MappingFlags::WRITE, &mut pages)?;
    copy_to_user_nofault_pages(start, src, &pages[..page_count]);
    Ok(())
}

/// A bounded user-memory transaction which keeps one address-space guard for
/// every read, preflight, and write in the caller's operation.
///
/// The transaction is intentionally small and policy-neutral. It performs no
/// allocation, population, blocking, or page fault. Callers must finish all
/// reads before the first write and preflight every destination span before
/// committing a multi-step protocol. Because the address-space guard remains
/// held for the complete closure, an unmap/remap cannot invalidate a span
/// between its preflight and copy.
pub struct UserNofaultTransaction<'a> {
    aspace: &'a AddrSpace,
}

impl UserNofaultTransaction<'_> {
    /// Copies one fixed-size, resident user span into kernel storage.
    pub fn read(&self, start: usize, dst: &mut [u8]) -> Result<(), UserNofaultError> {
        let mut pages = [NofaultPage::EMPTY; USER_NOFAULT_PAGE_SLOTS];
        let page_count = prepare_user_nofault_span(
            self.aspace,
            start,
            dst.len(),
            MappingFlags::READ,
            &mut pages,
        )?;
        copy_from_user_nofault_pages(start, dst, &pages[..page_count]);
        Ok(())
    }

    /// Checks one fixed-size destination span without changing user memory.
    pub fn preflight_write(&self, start: usize, src: &[u8]) -> Result<(), UserNofaultError> {
        let mut pages = [NofaultPage::EMPTY; USER_NOFAULT_PAGE_SLOTS];
        prepare_user_nofault_span(
            self.aspace,
            start,
            src.len(),
            MappingFlags::WRITE,
            &mut pages,
        )?;
        Ok(())
    }

    /// Copies one fixed-size kernel span into a destination previously
    /// preflighted by this transaction.
    pub fn write(&self, start: usize, src: &[u8]) -> Result<(), UserNofaultError> {
        let mut pages = [NofaultPage::EMPTY; USER_NOFAULT_PAGE_SLOTS];
        let page_count = prepare_user_nofault_span(
            self.aspace,
            start,
            src.len(),
            MappingFlags::WRITE,
            &mut pages,
        )?;
        copy_to_user_nofault_pages(start, src, &pages[..page_count]);
        Ok(())
    }
}

/// Executes one bounded user-memory transaction while holding one explicit
/// address-space `try_lock` guard.
pub fn try_user_nofault_transaction<R>(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    operation: impl FnOnce(&UserNofaultTransaction<'_>) -> Result<R, UserNofaultError>,
) -> Result<R, UserNofaultError> {
    let Some(aspace) = aspace_handle.try_lock() else {
        return Err(UserNofaultError::Retry);
    };
    let transaction = UserNofaultTransaction { aspace: &aspace };
    operation(&transaction)
}

/// Populates one bounded user span in task context for a nofault retry.
///
/// Unlike the nofault transaction, this helper may block, allocate page-table
/// and data frames, and complete a writable COW fault. It is deliberately
/// passed the address-space handle selected by the caller so a retry cannot
/// accidentally fault an unrelated image after an exec transition. Any
/// failure is terminal for the return hook; only the nofault snapshot itself
/// reports [`UserNofaultError::Retry`].
pub(crate) fn fault_user_range_task(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    start: usize,
    len: usize,
    access_flags: MappingFlags,
) -> Result<(), UserNofaultError> {
    if len == 0 {
        return Ok(());
    }
    let end = start.checked_add(len).ok_or(UserNofaultError::BadAddress)?;
    check_access(start, len).map_err(|_| UserNofaultError::BadAddress)?;

    let page_start = VirtAddr::from(start).align_down_4k();
    let page_end =
        VirtAddr::from(super::checked_align_up_4k(end).ok_or(UserNofaultError::BadAddress)?);
    let mut aspace = aspace_handle.lock();
    if !aspace.contains_range(VirtAddr::from(start), len)
        || !aspace.can_access_range(
            VirtAddr::from(start),
            len,
            access_flags | MappingFlags::USER,
        )
    {
        return Err(UserNofaultError::BadAddress);
    }
    aspace
        .populate_area(page_start, page_end - page_start, access_flags)
        .map_err(|_| UserNofaultError::BadAddress)
}

/// Reads one bounded span after [`fault_user_range_task`] has completed.
///
/// This is the blocking/task-context counterpart to `try_read_user_nofault`.
/// It keeps the address-space guard across translation and the physical copy,
/// so recovery code can discover a newly resident descriptor without
/// reopening the nofault race window.
pub(crate) fn read_user_nofault_task(
    start: usize,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    dst: &mut [u8],
) -> Result<(), UserNofaultError> {
    if !matches!(dst.len(), 4 | 8 | USER_NOFAULT_MAX_SPAN) {
        return Err(UserNofaultError::BadAddress);
    }
    let aspace = aspace_handle.lock();
    let mut pages = [NofaultPage::EMPTY; USER_NOFAULT_PAGE_SLOTS];
    let page_count =
        prepare_user_nofault_span(&aspace, start, dst.len(), MappingFlags::READ, &mut pages)?;
    copy_from_user_nofault_pages(start, dst, &pages[..page_count]);
    Ok(())
}

fn copy_from_user_nofault_pages(start: usize, dst: &mut [u8], pages: &[NofaultPage]) {
    let mut copied = 0;
    for page in pages {
        let offset = start.saturating_sub(page.start);
        let count = (PAGE_SIZE_4K - offset).min(dst.len() - copied);
        let source = phys_to_virt(page.paddr + offset).as_ptr();
        // SAFETY: `prepare_user_nofault_span` validated every page and the
        // caller still holds the address-space mutex guard.
        unsafe {
            ptr::copy_nonoverlapping(source, dst.as_mut_ptr().add(copied), count);
        }
        copied += count;
        if copied == dst.len() {
            break;
        }
    }
    debug_assert_eq!(copied, dst.len());
}

fn copy_to_user_nofault_pages(start: usize, src: &[u8], pages: &[NofaultPage]) {
    let mut copied = 0;
    for page in pages {
        let offset = start.saturating_sub(page.start);
        let count = (PAGE_SIZE_4K - offset).min(src.len() - copied);
        let destination = phys_to_virt(page.paddr + offset).as_mut_ptr();
        // SAFETY: `prepare_user_nofault_span` prevalidated every page's write
        // permission before this first or subsequent destination copy.
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr().add(copied), destination, count);
        }
        copied += count;
        if copied == src.len() {
            break;
        }
    }
    debug_assert_eq!(copied, src.len());
}

/// Checks and populates a writable range in an explicitly selected address
/// space.
pub fn check_user_writable_with(
    capability: &UserMemoryCapability,
    start: usize,
    len: usize,
) -> AxResult {
    populate_user_range_with(capability, start, len, MappingFlags::WRITE)
        .map_err(map_usercopy_error)
}

/// A read-only buffer in the VM's memory.
///
/// It implements the `axio::Read` trait, allowing it to be used with other I/O
/// operations.
pub struct VmBytes {
    /// Explicit address-space capability used for every operation.
    pub capability: UserMemoryCapability,
    /// The pointer to the start of the buffer in the VM's memory.
    pub ptr: *const u8,
    /// The length of the buffer.
    pub len: usize,
}

impl VmBytes {
    /// Creates a new `VmBytes` from a raw pointer and a length.
    pub fn new(capability: UserMemoryCapability, ptr: *const u8, len: usize) -> Self {
        Self {
            capability,
            ptr,
            len,
        }
    }

    /// Casts the `VmBytes` to a mutable `VmBytesMut`.
    pub fn cast_mut(&self) -> VmBytesMut {
        VmBytesMut::new(self.capability.clone(), self.ptr as *mut u8, self.len)
    }
}

impl Read for VmBytes {
    /// Reads bytes from the VM's memory into the provided buffer.
    fn read(&mut self, buf: &mut [u8]) -> axio::Result<usize> {
        let len = self.len.min(buf.len());
        let destination = unsafe {
            slice::from_raw_parts_mut(buf[..len].as_mut_ptr().cast::<MaybeUninit<u8>>(), len)
        };
        self.capability
            .read_slice(self.ptr, destination)
            .map_err(map_usercopy_error)?;
        self.ptr = self.ptr.wrapping_add(len);
        self.len -= len;
        Ok(len)
    }
}

impl IoBuf for VmBytes {
    fn remaining(&self) -> usize {
        self.len
    }
}

/// A mutable buffer in the VM's memory.
///
/// It implements the `axio::Write` trait, allowing it to be used with other I/O
/// operations.
pub struct VmBytesMut {
    /// Explicit address-space capability used for every operation.
    pub capability: UserMemoryCapability,
    /// The pointer to the start of the buffer in the VM's memory.
    pub ptr: *mut u8,
    /// The length of the buffer.
    pub len: usize,
}

impl VmBytesMut {
    /// Creates a new `VmBytesMut` from a raw pointer and a length.
    pub fn new(capability: UserMemoryCapability, ptr: *mut u8, len: usize) -> Self {
        Self {
            capability,
            ptr,
            len,
        }
    }

    /// Casts the `VmBytesMut` to a read-only `VmBytes`.
    pub fn cast_const(&self) -> VmBytes {
        VmBytes::new(self.capability.clone(), self.ptr, self.len)
    }
}

impl Write for VmBytesMut {
    /// Writes bytes from the provided buffer into the VM's memory.
    fn write(&mut self, buf: &[u8]) -> axio::Result<usize> {
        let len = self.len.min(buf.len());
        self.capability
            .write_bytes(self.ptr as usize, &buf[..len])
            .map_err(map_usercopy_error)?;
        self.ptr = self.ptr.wrapping_add(len);
        self.len -= len;
        Ok(len)
    }

    /// Flushes the buffer. This is a no-op for `VmBytesMut`.
    fn flush(&mut self) -> axio::Result {
        Ok(())
    }
}

impl IoBufMut for VmBytesMut {
    fn remaining_mut(&self) -> usize {
        self.len
    }
}
