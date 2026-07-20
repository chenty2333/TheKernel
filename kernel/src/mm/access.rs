use alloc::{string::String, sync::Arc, vec::Vec};
#[cfg(feature = "test-io-control")]
use core::time::Duration;
use core::{
    alloc::Layout,
    cmp::min,
    ffi::c_char,
    hint::unlikely,
    mem::{MaybeUninit, transmute},
    ptr::{self, NonNull},
    slice, str,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs::CachedFilePagePin;
use axhal::{
    asm::user_copy,
    paging::{MappingFlags, PageSize},
    trap::{PAGE_FAULT, register_trap_handler},
};
use axio::prelude::*;
use axsync::Mutex;
#[cfg(feature = "test-io-control")]
use axtask::sleep;
use axtask::{current, current_may_uninit};
use extern_trait::extern_trait;
use kernel_guard::IrqSave;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use starry_vm::{
    VmError, VmIo, VmResult, vm_load_until_nul, vm_load_until_nul_bounded, vm_read_slice,
    vm_write_slice,
};
use thekernel_linux_mm::{PinAccess, PinDuration, PinRequest, PinToken, PinUse, UserRange};

use super::{
    AddrSpace, Backend, PhysicalFramePins, PreparedPhysicalFramePins, UserIoMappingExpectation,
    prepare_physical_pin_registry,
};
use crate::{
    config::{USER_SPACE_BASE, USER_SPACE_SIZE},
    task::{AsThread, Thread},
};

/// RAII guard that resets the `accessing_user_memory` flag on drop, ensuring
/// cleanup even if the closure panics.
struct UserMemoryAccessGuard<'a>(&'a Thread);

static PAGE_FAULT_THREAD_CONTEXT_READY: AtomicBool = AtomicBool::new(false);
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

const MAX_USER_IO_PIN_SEGMENTS: usize = 32;
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

#[inline(always)]
pub fn prefault_user_io_to_user(ptr: *mut u8, len: usize) -> VmResult {
    if len == 0 {
        return Ok(());
    }
    record_user_io_pin_counter(&USER_IO_PREFAULT_TO_USER_ATTEMPTS, 1);
    match populate_user_range(ptr as usize, len, MappingFlags::WRITE) {
        Ok(()) => {
            record_user_io_pin_counter(&USER_IO_PREFAULT_TO_USER_HITS, 1);
            record_user_io_pin_counter(&USER_IO_PREFAULT_TO_USER_BYTES, len as u64);
            Ok(())
        }
        Err(err) => {
            record_user_io_pin_counter(&USER_IO_PREFAULT_TO_USER_REJECTS, 1);
            Err(err)
        }
    }
}

#[inline(always)]
pub fn prefault_user_io_from_user(ptr: *const u8, len: usize) -> VmResult {
    if len == 0 {
        return Ok(());
    }
    record_user_io_pin_counter(&USER_IO_PREFAULT_FROM_USER_ATTEMPTS, 1);
    match populate_user_range(ptr as usize, len, MappingFlags::READ) {
        Ok(()) => {
            record_user_io_pin_counter(&USER_IO_PREFAULT_FROM_USER_HITS, 1);
            record_user_io_pin_counter(&USER_IO_PREFAULT_FROM_USER_BYTES, len as u64);
            Ok(())
        }
        Err(err) => {
            record_user_io_pin_counter(&USER_IO_PREFAULT_FROM_USER_REJECTS, 1);
            Err(err)
        }
    }
}

pub fn mark_page_fault_thread_context_ready() {
    PAGE_FAULT_THREAD_CONTEXT_READY.store(true, Ordering::Release);
}

impl Drop for UserMemoryAccessGuard<'_> {
    fn drop(&mut self) {
        self.0.set_accessing_user_memory(false);
    }
}

/// Enables scoped access into user memory, allowing page faults to occur inside
/// kernel.
pub fn access_user_memory<R>(f: impl FnOnce() -> R) -> R {
    let curr = current();
    let Some(thr) = curr.try_as_thread() else {
        panic!("access_user_memory called outside of thread context");
    };

    thr.set_accessing_user_memory(true);
    let _guard = UserMemoryAccessGuard(thr);
    f()
}

fn check_region(start: VirtAddr, layout: Layout, access_flags: MappingFlags) -> AxResult<()> {
    let align = layout.align();
    if start.as_usize() & (align - 1) != 0 {
        return Err(AxError::BadAddress);
    }

    let curr = current();
    let aspace_handle = curr.as_thread().proc_data.aspace();
    let mut aspace = aspace_handle.lock();

    if !aspace.can_access_range(start, layout.size(), access_flags) {
        return Err(AxError::BadAddress);
    }

    let page_start = start.align_down_4k();
    let end = start
        .checked_add(layout.size())
        .ok_or(AxError::BadAddress)?;
    let page_end =
        VirtAddr::from(super::checked_align_up_4k(end.as_usize()).ok_or(AxError::BadAddress)?);
    aspace.populate_area(page_start, page_end - page_start, access_flags)?;

    Ok(())
}

fn check_null_terminated<T: PartialEq + Default>(
    start: VirtAddr,
    access_flags: MappingFlags,
) -> AxResult<usize> {
    let align = Layout::new::<T>().align();
    if start.as_usize() & (align - 1) != 0 {
        return Err(AxError::BadAddress);
    }

    let zero = T::default();

    let mut page = start.align_down_4k();

    let start = start.as_ptr_of::<T>();
    let mut len = 0;

    access_user_memory(|| {
        loop {
            // SAFETY: This won't overflow the address space since we'll check
            // it below.
            let ptr = unsafe { start.add(len) };
            while ptr as usize >= page.as_ptr() as usize {
                // We cannot prepare `aspace` outside of the loop, since holding
                // aspace requires a mutex which would be required on page
                // fault, and page faults can trigger inside the loop.

                // TODO: this is inefficient, but we have to do this instead of
                // querying the page table since the page might has not been
                // allocated yet.
                let curr = current();
                let aspace_handle = curr.as_thread().proc_data.aspace();
                let aspace = aspace_handle.lock();
                if !aspace.can_access_range(page, PAGE_SIZE_4K, access_flags) {
                    return Err(AxError::BadAddress);
                }

                page += PAGE_SIZE_4K;
            }

            // This might trigger a page fault
            // SAFETY: The pointer is valid and points to a valid memory region.
            if unsafe { ptr.read_volatile() } == zero {
                break;
            }
            len += 1;
        }
        Ok(())
    })?;

    Ok(len)
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
    const ACCESS_FLAGS: MappingFlags = MappingFlags::READ.union(MappingFlags::WRITE);

    pub fn address(&self) -> VirtAddr {
        VirtAddr::from_ptr_of(self.0)
    }

    pub fn cast<U>(self) -> UserPtr<U> {
        UserPtr(self.0 as *mut U)
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    pub fn get_as_mut(self) -> AxResult<&'static mut T> {
        check_region(self.address(), Layout::new::<T>(), Self::ACCESS_FLAGS)?;
        Ok(unsafe { &mut *self.0 })
    }

    pub fn get_as_mut_slice(self, len: usize) -> AxResult<&'static mut [T]> {
        let layout = Layout::array::<T>(len).map_err(|_| AxError::BadAddress)?;
        if len == 0 {
            return Ok(unsafe { slice::from_raw_parts_mut(NonNull::<T>::dangling().as_ptr(), 0) });
        }
        check_region(self.address(), layout, Self::ACCESS_FLAGS)?;
        Ok(unsafe { slice::from_raw_parts_mut(self.0, len) })
    }

    pub fn get_as_mut_null_terminated(self) -> AxResult<&'static mut [T]>
    where
        T: PartialEq + Default,
    {
        let len = check_null_terminated::<T>(self.address(), Self::ACCESS_FLAGS)?;
        Ok(unsafe { slice::from_raw_parts_mut(self.0, len) })
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
    const ACCESS_FLAGS: MappingFlags = MappingFlags::READ;

    pub fn address(&self) -> VirtAddr {
        VirtAddr::from_ptr_of(self.0)
    }

    pub fn cast<U>(self) -> UserConstPtr<U> {
        UserConstPtr(self.0 as *const U)
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    pub fn get_as_ref(self) -> AxResult<&'static T> {
        check_region(self.address(), Layout::new::<T>(), Self::ACCESS_FLAGS)?;
        Ok(unsafe { &*self.0 })
    }

    pub fn get_as_slice(self, len: usize) -> AxResult<&'static [T]> {
        let layout = Layout::array::<T>(len).map_err(|_| AxError::BadAddress)?;
        if len == 0 {
            return Ok(unsafe { slice::from_raw_parts(NonNull::<T>::dangling().as_ptr(), 0) });
        }
        check_region(self.address(), layout, Self::ACCESS_FLAGS)?;
        Ok(unsafe { slice::from_raw_parts(self.0, len) })
    }

    pub fn get_as_null_terminated(self) -> AxResult<&'static [T]>
    where
        T: PartialEq + Default,
    {
        let len = check_null_terminated::<T>(self.address(), Self::ACCESS_FLAGS)?;
        Ok(unsafe { slice::from_raw_parts(self.0, len) })
    }
}

impl UserConstPtr<c_char> {
    /// Get the pointer as `&str`, validating the memory region.
    pub fn get_as_str(self) -> AxResult<&'static str> {
        let slice = self.get_as_null_terminated()?;
        // SAFETY: c_char is u8
        let slice = unsafe { transmute::<&[c_char], &[u8]>(slice) };

        str::from_utf8(slice).map_err(|_| AxError::IllegalBytes)
    }
}

macro_rules! nullable {
    ($ptr:ident.$func:ident($($arg:expr),*)) => {
        if $ptr.is_null() {
            Ok(None)
        } else {
            Some($ptr.$func($($arg),*)).transpose()
        }
    };
}

pub(crate) use nullable;

#[register_trap_handler(PAGE_FAULT)]
fn handle_page_fault(vaddr: VirtAddr, access_flags: MappingFlags) -> bool {
    if !PAGE_FAULT_THREAD_CONTEXT_READY.load(Ordering::Acquire) {
        return false;
    }

    let Some(curr) = current_may_uninit() else {
        return false;
    };
    let Some(thr) = curr.try_as_thread() else {
        return false;
    };

    if unlikely(!thr.is_accessing_user_memory()) {
        return false;
    }

    debug!("Page fault at {vaddr:#x}, access_flags: {access_flags:#x?}");

    let aspace_handle = thr.proc_data.aspace();
    aspace_handle.lock().handle_page_fault(vaddr, access_flags)
}

pub fn vm_load_string(ptr: *const c_char) -> AxResult<String> {
    #[allow(clippy::unnecessary_cast)]
    let bytes = vm_load_until_nul(ptr as *const u8)?;
    String::from_utf8(bytes).map_err(|_| AxError::IllegalBytes)
}

/// Loads one NUL-terminated UTF-8 string within an explicit byte budget.
///
/// `scan_bytes` includes the terminating NUL, matching Linux's field-size
/// limits for syscall strings.
pub fn vm_load_string_bounded(ptr: *const c_char, scan_bytes: usize) -> AxResult<String> {
    #[allow(clippy::unnecessary_cast)]
    let bytes = vm_load_until_nul_bounded(ptr as *const u8, scan_bytes)?;
    String::from_utf8(bytes).map_err(|_| AxError::IllegalBytes)
}

#[allow(dead_code)]
struct Vm;

/// Bound the duration of a single IRQ-masked user-copy operation.
const USER_COPY_CHUNK: usize = 16 * 1024;

fn copy_user_bytes(mut dst: *mut u8, mut src: *const u8, mut len: usize) -> VmResult {
    while len != 0 {
        let chunk = min(len, USER_COPY_CHUNK);
        let failed_at = {
            let _irq = IrqSave::new();
            access_user_memory(|| unsafe { user_copy(dst, src, chunk) })
        };
        if unlikely(failed_at != 0) {
            return Err(VmError::AccessDenied);
        }

        // SAFETY: `chunk <= len`, and the caller validated the entire range up
        // front before entering this helper.
        unsafe {
            dst = dst.add(chunk);
            src = src.add(chunk);
        }
        len -= chunk;
    }

    Ok(())
}

/// Briefly checks if the given memory region is valid user memory.
pub fn check_access(start: usize, len: usize) -> VmResult {
    const USER_SPACE_END: usize = USER_SPACE_BASE + USER_SPACE_SIZE;
    let ok = (USER_SPACE_BASE..USER_SPACE_END).contains(&start) && (USER_SPACE_END - start) >= len;
    if unlikely(!ok) {
        Err(VmError::AccessDenied)
    } else {
        Ok(())
    }
}

fn populate_user_range(start: usize, len: usize, access_flags: MappingFlags) -> VmResult {
    check_access(start, len)?;
    if len == 0 {
        return Ok(());
    }

    let Some(curr) = current_may_uninit() else {
        return Err(VmError::AccessDenied);
    };
    let Some(thr) = curr.try_as_thread() else {
        return Err(VmError::AccessDenied);
    };

    let start = VirtAddr::from(start);
    let page_start = start.align_down_4k();
    let end = start.checked_add(len).ok_or(VmError::AccessDenied)?;
    let page_end =
        VirtAddr::from(super::checked_align_up_4k(end.as_usize()).ok_or(VmError::AccessDenied)?);
    let aspace_handle = thr.proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    if !aspace.can_access_range(start, len, access_flags) {
        return Err(VmError::AccessDenied);
    }
    aspace
        .populate_area(page_start, page_end - page_start, access_flags)
        .map_err(|_| VmError::AccessDenied)
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

#[derive(Debug, Clone, Copy, Default)]
pub struct UserIoPinSegment {
    pub paddr: usize,
    pub len: usize,
}

#[derive(Debug, Clone)]
pub struct UserIoPinSegments {
    segments: [UserIoPinSegment; MAX_USER_IO_PIN_SEGMENTS],
    len: usize,
    bytes: usize,
}

impl UserIoPinSegments {
    const fn new() -> Self {
        Self {
            segments: [UserIoPinSegment { paddr: 0, len: 0 }; MAX_USER_IO_PIN_SEGMENTS],
            len: 0,
            bytes: 0,
        }
    }

    fn push_or_merge(&mut self, paddr: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        if let Some(prev) = self.len.checked_sub(1).map(|idx| &mut self.segments[idx])
            && prev.paddr.checked_add(prev.len) == Some(paddr)
        {
            prev.len += len;
            self.bytes += len;
            return true;
        }
        if self.len == self.segments.len() {
            return false;
        }
        self.segments[self.len] = UserIoPinSegment { paddr, len };
        self.len += 1;
        self.bytes += len;
        true
    }

    pub fn as_slice(&self) -> &[UserIoPinSegment] {
        &self.segments[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
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
        self.aspace.lock().end_user_io_pin(self.token);
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
    charged_pages: usize,
    frame_pages_admitted: usize,
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
            charged_pages: page_count,
            frame_pages_admitted: 0,
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
            self.aspace.lock().cancel_user_io_pin(reservation);
        }
        drop(self.system_charge.take());
    }
}

struct PreparedUserIoPin {
    segments: UserIoPinSegments,
    frame_pins: UserIoFramePins,
    page_cache_pins: UserIoPageCachePins,
    range_pin: UserIoRangePin,
}

fn prepare_user_io_pin(
    start: usize,
    len: usize,
    access_flags: MappingFlags,
    require_contiguous: bool,
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

    let Some(curr) = current_may_uninit() else {
        reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
        return None;
    };
    let Some(thr) = curr.try_as_thread() else {
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
    let aspace_handle = thr.proc_data.aspace();
    let admission = {
        let mut aspace = aspace_handle.lock();
        record_user_io_pin_counter(&USER_IO_PIN_VM_RANGE_PIN_ATTEMPTS, 1);
        let request = PinRequest::new(
            UserRange::new(start, len).ok()?,
            if access_flags.contains(MappingFlags::WRITE) {
                PinAccess::Write
            } else {
                PinAccess::Read
            },
            PinDuration::AsyncIo,
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

    // The initial lock scope only installs the full-range Reserved barrier and
    // its system charge. Build mapping expectations afterwards in bounded
    // windows, using storage reserved outside every address-space lock. The
    // Reserved record blocks overlapping topology/access mutations across the
    // gaps between windows.
    let mut expectation_cursor = page_start;
    while expectation_cursor < page_end {
        let chunk_end = user_io_pin_scan_chunk_end(expectation_cursor, page_end);
        let expectation_result = {
            let aspace = aspace_handle.lock();
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
    let needs_frame_registry = preparation
        .expectations()
        .iter()
        .any(UserIoMappingExpectation::needs_frame_registry);
    if needs_frame_registry && prepare_physical_pin_registry().is_err() {
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
    let mut segments = UserIoPinSegments::new();
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
        let frame_capacity = if needs_frame_registry { chunk_pages } else { 0 };
        let mut frame_preparation = match PreparedPhysicalFramePins::try_new(frame_capacity) {
            Ok(preparation) => preparation,
            Err(_) => {
                reject_user_io_pin(&USER_IO_PIN_VM_RANGE_PIN_REJECTS);
                return None;
            }
        };

        let chunk_pin = {
            let aspace = aspace_handle.lock();
            (|| {
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
                    if backend.supports_user_io_frame_pin() {
                        match backend {
                            Backend::Cow(_) => cow_frame_pages += 1,
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
            let mut aspace = aspace_handle.lock();
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
        let mut aspace = aspace_handle.lock();
        aspace.commit_user_io_pin(preparation.reservation())
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
        if delay_ms != 0 && user_io_pin_counters_enabled() {
            if sleep(Duration::from_millis(delay_ms)).is_err() {
                reject_user_io_pin(&USER_IO_PIN_REJECT_ACCESS);
                return None;
            }
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

pub fn try_pin_user_slice_from_user(ptr: *const u8, len: usize) -> Option<PinnedUserSlice> {
    record_user_io_pin_counter(&USER_IO_PIN_FROM_USER_ATTEMPTS, 1);
    let prepared = prepare_user_io_pin(ptr as usize, len, MappingFlags::READ, true)?;
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

pub fn try_pin_user_slice_to_user(ptr: *mut u8, len: usize) -> Option<PinnedUserSliceMut> {
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_ATTEMPTS, 1);
    let prepared = prepare_user_io_pin(ptr as usize, len, MappingFlags::WRITE, true)?;
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
    _frame_pins: UserIoFramePins,
    _page_cache_pins: UserIoPageCachePins,
    _range_pin: UserIoRangePin,
}

#[allow(dead_code)]
impl PinnedUserSegmentsMut {
    pub fn segments(&self) -> &[UserIoPinSegment] {
        self.segments.as_slice()
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

#[cfg(test)]
mod tests {
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::{
        USER_IO_PIN_SCAN_CHUNK_PAGES, UserIoPinSegment, UserIoPinSegments,
        physical_pin_segments_are_disjoint, user_io_pin_scan_chunk_end,
    };

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
        let mut aliases = UserIoPinSegments::new();
        assert!(aliases.push_or_merge(0x1000, 0x800));
        assert!(aliases.push_or_merge(0x1000, 0x800));
        assert!(!aliases.physical_ranges_are_disjoint());

        let mut disjoint = UserIoPinSegments::new();
        assert!(disjoint.push_or_merge(0x1000, 0x800));
        assert!(disjoint.push_or_merge(0x2000, 0x800));
        assert!(disjoint.physical_ranges_are_disjoint());
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
}

#[allow(dead_code)]
pub fn try_pin_user_segments_from_user(ptr: *const u8, len: usize) -> Option<PinnedUserSegments> {
    record_user_io_pin_counter(&USER_IO_PIN_FROM_USER_ATTEMPTS, 1);
    let prepared = prepare_user_io_pin(ptr as usize, len, MappingFlags::READ, false)?;
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
pub fn try_pin_user_segments_to_user(ptr: *mut u8, len: usize) -> Option<PinnedUserSegmentsMut> {
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_ATTEMPTS, 1);
    let prepared = prepare_user_io_pin(ptr as usize, len, MappingFlags::WRITE, false)?;
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_HITS, 1);
    record_user_io_pin_counter(&USER_IO_PIN_TO_USER_BYTES, len as u64);
    record_user_io_pin_segments(&prepared.segments);
    Some(PinnedUserSegmentsMut {
        _ptr: ptr,
        segments: prepared.segments,
        _frame_pins: prepared.frame_pins,
        _page_cache_pins: prepared.page_cache_pins,
        _range_pin: prepared.range_pin,
    })
}

pub fn check_user_readable(start: usize, len: usize) -> VmResult {
    populate_user_range(start, len, MappingFlags::READ)
}

pub fn check_user_writable(start: usize, len: usize) -> VmResult {
    populate_user_range(start, len, MappingFlags::WRITE)
}

#[extern_trait]
unsafe impl VmIo for Vm {
    fn new() -> Self {
        Self
    }

    fn read(&mut self, start: usize, buf: &mut [MaybeUninit<u8>]) -> VmResult {
        populate_user_range(start, buf.len(), MappingFlags::READ)?;
        copy_user_bytes(buf.as_mut_ptr() as *mut _, start as _, buf.len())
    }

    fn write(&mut self, start: usize, buf: &[u8]) -> VmResult {
        populate_user_range(start, buf.len(), MappingFlags::WRITE)?;
        copy_user_bytes(start as _, buf.as_ptr() as *const _, buf.len())
    }
}

/// A read-only buffer in the VM's memory.
///
/// It implements the `axio::Read` trait, allowing it to be used with other I/O
/// operations.
pub struct VmBytes {
    /// The pointer to the start of the buffer in the VM's memory.
    pub ptr: *const u8,
    /// The length of the buffer.
    pub len: usize,
}

impl VmBytes {
    /// Creates a new `VmBytes` from a raw pointer and a length.
    pub fn new(ptr: *const u8, len: usize) -> Self {
        Self { ptr, len }
    }

    /// Casts the `VmBytes` to a mutable `VmBytesMut`.
    pub fn cast_mut(&self) -> VmBytesMut {
        VmBytesMut::new(self.ptr as *mut u8, self.len)
    }
}

impl Read for VmBytes {
    /// Reads bytes from the VM's memory into the provided buffer.
    fn read(&mut self, buf: &mut [u8]) -> axio::Result<usize> {
        let len = self.len.min(buf.len());
        vm_read_slice(self.ptr, unsafe {
            transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut buf[..len])
        })?;
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
    /// The pointer to the start of the buffer in the VM's memory.
    pub ptr: *mut u8,
    /// The length of the buffer.
    pub len: usize,
}

impl VmBytesMut {
    /// Creates a new `VmBytesMut` from a raw pointer and a length.
    pub fn new(ptr: *mut u8, len: usize) -> Self {
        Self { ptr, len }
    }

    /// Casts the `VmBytesMut` to a read-only `VmBytes`.
    pub fn cast_const(&self) -> VmBytes {
        VmBytes::new(self.ptr, self.len)
    }
}

impl Write for VmBytesMut {
    /// Writes bytes from the provided buffer into the VM's memory.
    fn write(&mut self, buf: &[u8]) -> axio::Result<usize> {
        let len = self.len.min(buf.len());
        vm_write_slice(self.ptr, &buf[..len])?;
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
