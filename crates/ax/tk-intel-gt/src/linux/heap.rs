// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Small Linux kmem_cache subset for the imported GT object lifecycle. Objects
// come from the crate's global allocator and are reused on free until cache
// destruction. Caches with a constructor keep the free-list link outside the
// constructed object (as SLUB does), so a reused object keeps the state its
// constructor gave it.

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use core::{
    cell::Cell,
    ffi::{c_char, c_void},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

pub const SLAB_HWCACHE_ALIGN: u32 = 1 << 4;
pub const SLAB_RECLAIM_ACCOUNT: u32 = 0x0002_0000;
pub const SLAB_TYPESAFE_BY_RCU: u32 = 0x0008_0000;
const L1_CACHE_BYTES: usize = 64;
/// Linux `___GFP_DIRECT_RECLAIM_BIT` in the wt-dev configuration. Only a
/// sleepable request carries it; `GFP_ATOMIC` shares other bits with
/// `GFP_KERNEL`, so the atomic test must be on this bit.
const GFP_DIRECT_RECLAIM: u32 = 1 << 10;
const LINK_SIZE: usize = core::mem::size_of::<*mut u8>();

#[repr(C)]
pub struct KmCache {
    /// Bytes of each object that the caller owns (zeroed or constructed).
    object_size: usize,
    /// Bytes per allocation, including the free-list link.
    stride: usize,
    object_align: usize,
    /// Offset of the free-list link inside each object's allocation.
    link_offset: usize,
    /// Object base of the first free object, or NULL. Guarded by `lock`.
    free_list: Cell<*mut u8>,
    lock: AtomicBool,
    ctor: Option<unsafe extern "C" fn(*mut c_void)>,
}

impl KmCache {
    fn lock(&self) {
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    fn unlock(&self) {
        self.lock.store(false, Ordering::Release);
    }

    fn layout(&self) -> Layout {
        Layout::from_size_align(self.stride, self.object_align)
            .expect("validated kmem_cache layout")
    }

    /// The free-list link stored inside a free object.
    unsafe fn link(&self, object: *mut u8) -> *mut *mut u8 {
        unsafe { object.add(self.link_offset).cast() }
    }

    /// Take one free object, or null when the list is empty.
    fn pop_free(&self) -> *mut u8 {
        self.lock();
        let object = self.free_list.get();
        if !object.is_null() {
            self.free_list.set(unsafe { *self.link(object) });
        }
        self.unlock();
        object
    }

    fn push_free(&self, object: *mut u8) {
        self.lock();
        unsafe { *self.link(object) = self.free_list.get() };
        self.free_list.set(object);
        self.unlock();
    }
}

unsafe impl Send for KmCache {}
unsafe impl Sync for KmCache {}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// Build a cache for `size`-byte objects. `link_offset` is 0 for plain caches
/// and lies after the object for constructor caches.
unsafe fn new_cache(
    size: usize,
    align: usize,
    ctor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> *mut KmCache {
    let (link_offset, stride) = if ctor.is_some() {
        let link_offset = align_up(size, LINK_SIZE);
        (link_offset, link_offset + LINK_SIZE)
    } else {
        (0, size.max(LINK_SIZE))
    };
    let Ok(layout) = Layout::from_size_align(stride, align) else {
        return ptr::null_mut();
    };
    let cache = unsafe { alloc_zeroed(Layout::new::<KmCache>()) }.cast::<KmCache>();
    if cache.is_null() {
        return cache;
    }
    unsafe {
        ptr::write(
            cache,
            KmCache {
                object_size: size,
                stride,
                object_align: layout.align(),
                link_offset,
                free_list: Cell::new(ptr::null_mut()),
                lock: AtomicBool::new(false),
                ctor,
            },
        );
    }
    cache
}

pub unsafe fn kmem_cache_create<T>(flags: u32) -> *mut KmCache {
    let mut align = core::mem::align_of::<T>();
    if flags & SLAB_HWCACHE_ALIGN != 0 {
        align = align.max(L1_CACHE_BYTES);
    }
    unsafe { new_cache(core::mem::size_of::<T>(), align, None) }
}

/// Allocate one object: a reused free object, or a fresh zeroed allocation
/// that the constructor (if any) initializes. Non-sleepable requests return
/// null, because the global allocator cannot honour them.
unsafe fn take_object(cache: *mut KmCache, flags: u32) -> *mut u8 {
    if cache.is_null() || flags & GFP_DIRECT_RECLAIM == 0 {
        return ptr::null_mut();
    }
    let reused = unsafe { (*cache).pop_free() };
    if !reused.is_null() {
        return reused;
    }
    let fresh = unsafe { alloc_zeroed((*cache).layout()) };
    if fresh.is_null() {
        return fresh;
    }
    if let Some(ctor) = unsafe { (*cache).ctor } {
        unsafe { ctor(fresh.cast()) };
    }
    fresh
}

/// Zeroed allocation of one `T` from `cache`, as the Rust callers use it.
///
/// # Safety
/// `cache` must come from [`kmem_cache_create`] and not be destroyed.
pub unsafe fn kmem_cache_zalloc<T>(cache: *mut KmCache, flags: u32) -> *mut T {
    let object = unsafe { take_object(cache, flags) };
    if !object.is_null() {
        unsafe { ptr::write_bytes(object, 0, (*cache).object_size) };
    }
    object.cast()
}

pub unsafe fn kmem_cache_free(cache: *mut KmCache, object: *mut c_void) {
    if cache.is_null() || object.is_null() {
        return;
    }
    unsafe { (*cache).push_free(object.cast()) };
}

pub unsafe fn kmem_cache_destroy(cache: *mut KmCache) {
    if cache.is_null() {
        return;
    }
    let layout = unsafe { (*cache).layout() };
    loop {
        let object = unsafe { (*cache).pop_free() };
        if object.is_null() {
            break;
        }
        unsafe { dealloc(object, layout) };
    }
    unsafe { dealloc(cache.cast::<u8>(), Layout::new::<KmCache>()) };
}

/// Linux `kmem_cache_create(name, size, align, flags, ctor)`. `name` is a
/// diagnostic label only. Returns NULL for a zero size or an invalid layout,
/// as Linux does. `ctor` runs once for each object the cache constructs from
/// the allocator; reused objects keep their state.
///
/// # Safety
/// `name` must be NULL or a valid C string; `ctor` must be safe to call on
/// any object of `size` bytes returned by this cache.
#[unsafe(export_name = "kmem_cache_create")]
pub unsafe extern "C" fn kmem_cache_create_c(
    _name: *const c_char,
    size: usize,
    align: usize,
    flags: u32,
    ctor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> *mut KmCache {
    if size == 0 {
        return ptr::null_mut();
    }
    let mut align = align.max(core::mem::align_of::<usize>());
    if flags & SLAB_HWCACHE_ALIGN != 0 {
        align = align.max(L1_CACHE_BYTES);
    }
    unsafe { new_cache(size, align, ctor) }
}

/// Linux `kmem_cache_alloc(cache, flags)`. Fresh objects are zeroed and
/// constructed by `ctor`; reused objects are returned unchanged, as in Linux.
///
/// # Safety
/// `cache` must come from [`kmem_cache_create_c`] and not be destroyed.
#[unsafe(export_name = "kmem_cache_alloc")]
pub unsafe extern "C" fn kmem_cache_alloc_c(cache: *mut KmCache, flags: u32) -> *mut c_void {
    unsafe { take_object(cache, flags) }.cast()
}

/// Linux `kmem_cache_destroy(cache)`: releases the cache and its free objects.
///
/// # Safety
/// `cache` must come from [`kmem_cache_create_c`] and have no live objects.
#[unsafe(export_name = "kmem_cache_destroy")]
pub unsafe extern "C" fn kmem_cache_destroy_c(cache: *mut KmCache) {
    unsafe { kmem_cache_destroy(cache) };
}

macro_rules! KMEM_CACHE {
    ($object:ty, $flags:expr) => {{ unsafe { $crate::linux_heap::kmem_cache_create::<$object>($flags).cast() } }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kmem_cache_zeroes_and_reuses_objects() {
        unsafe {
            let cache = kmem_cache_create::<u64>(SLAB_HWCACHE_ALIGN);
            assert!(!cache.is_null());
            let first = kmem_cache_zalloc::<u64>(cache, crate::linux_config::GFP_KERNEL);
            assert!(!first.is_null());
            assert_eq!(*first, 0);
            *first = 0x1234;
            kmem_cache_free(cache, first.cast());

            let second = kmem_cache_zalloc::<u64>(cache, crate::linux_config::GFP_KERNEL);
            assert_eq!(second, first);
            assert_eq!(*second, 0);
            kmem_cache_free(cache, second.cast());
            kmem_cache_destroy(cache);
        }
    }

    #[test]
    fn non_sleepable_requests_fail_closed() {
        unsafe {
            let cache = kmem_cache_create::<u64>(0);
            assert!(!cache.is_null());
            assert!(kmem_cache_zalloc::<u64>(cache, crate::linux_config::GFP_ATOMIC).is_null());
            assert!(kmem_cache_zalloc::<u64>(cache, 0).is_null());
            assert!(!kmem_cache_zalloc::<u64>(cache, crate::linux_config::GFP_KERNEL).is_null());
            kmem_cache_destroy(cache);
        }
    }
}

#[cfg(test)]
mod ctor_tests {
    use core::sync::atomic::AtomicUsize;

    use super::*;

    static CONSTRUCTED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn count_construction(object: *mut c_void) {
        CONSTRUCTED.fetch_add(1, Ordering::SeqCst);
        unsafe { object.cast::<u64>().write(0xfeed) };
    }

    #[test]
    fn ctor_runs_for_fresh_objects_and_reuse_keeps_state() {
        unsafe {
            let cache =
                kmem_cache_create_c(c"ctor-test".as_ptr(), 16, 8, 0, Some(count_construction));
            assert!(!cache.is_null());
            let before = CONSTRUCTED.load(Ordering::SeqCst);
            let first = kmem_cache_alloc_c(cache, crate::linux_config::GFP_KERNEL).cast::<u64>();
            assert!(!first.is_null());
            assert_eq!(*first, 0xfeed);
            assert_eq!(CONSTRUCTED.load(Ordering::SeqCst), before + 1);

            *first = 0x1234;
            kmem_cache_free(cache, first.cast());
            let second = kmem_cache_alloc_c(cache, crate::linux_config::GFP_KERNEL).cast::<u64>();
            assert_eq!(second, first);
            assert_eq!(
                *second, 0x1234,
                "reused objects keep their constructed state"
            );
            assert_eq!(CONSTRUCTED.load(Ordering::SeqCst), before + 1);
            kmem_cache_free(cache, second.cast());
            kmem_cache_destroy_c(cache);
        }
    }

    #[test]
    fn invalid_cache_layouts_are_refused() {
        unsafe {
            assert!(kmem_cache_create_c(ptr::null(), 0, 8, 0, None).is_null());
            assert!(kmem_cache_alloc_c(ptr::null_mut(), crate::linux_config::GFP_KERNEL).is_null());
        }
    }
}
