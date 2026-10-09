// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Small Linux kmem_cache subset for the imported GT object lifecycle. Objects
// come from the crate's global allocator, are zeroed on zalloc, and are reused
// on free until cache destruction.

use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

pub const SLAB_HWCACHE_ALIGN: u32 = 1 << 4;
pub const SLAB_RECLAIM_ACCOUNT: u32 = 0x0002_0000;
pub const SLAB_TYPESAFE_BY_RCU: u32 = 0x0008_0000;
const L1_CACHE_BYTES: usize = 64;

#[repr(C)]
struct FreeNode {
    next: *mut FreeNode,
}

#[repr(C)]
pub struct KmCache {
    object_size: usize,
    object_align: usize,
    free_list: *mut FreeNode,
    lock: AtomicBool,
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
        Layout::from_size_align(self.object_size, self.object_align)
            .expect("validated kmem_cache layout")
    }
}

unsafe impl Send for KmCache {}
unsafe impl Sync for KmCache {}

pub unsafe fn kmem_cache_create<T>(flags: u32) -> *mut KmCache {
    let mut align = core::mem::align_of::<T>();
    if flags & SLAB_HWCACHE_ALIGN != 0 {
        align = align.max(L1_CACHE_BYTES);
    }
    let object_size = core::mem::size_of::<T>().max(core::mem::size_of::<FreeNode>());
    let Ok(layout) = Layout::from_size_align(object_size, align) else {
        return ptr::null_mut();
    };
    let cache = alloc_zeroed(Layout::new::<KmCache>()).cast::<KmCache>();
    if cache.is_null() {
        return cache;
    }
    ptr::write(
        cache,
        KmCache {
            object_size,
            object_align: layout.align(),
            free_list: ptr::null_mut(),
            lock: AtomicBool::new(false),
        },
    );
    cache
}

pub unsafe fn kmem_cache_zalloc<T>(cache: *mut KmCache, flags: u32) -> *mut T {
    if cache.is_null() {
        return ptr::null_mut();
    }
    // The global allocator may block; refuse Linux GFP_ATOMIC rather than
    // silently weakening the source allocation-context contract.
    if flags & crate::linux_config::GFP_ATOMIC != 0 {
        return ptr::null_mut();
    }
    (*cache).lock();
    let object = (*cache).free_list;
    if !object.is_null() {
        (*cache).free_list = (*object).next;
    }
    (*cache).unlock();

    if object.is_null() {
        alloc_zeroed((*cache).layout()).cast::<T>()
    } else {
        ptr::write_bytes(object.cast::<u8>(), 0, (*cache).object_size);
        object.cast::<T>()
    }
}

pub unsafe fn kmem_cache_free(cache: *mut KmCache, object: *mut c_void) {
    if cache.is_null() || object.is_null() {
        return;
    }
    let node = object.cast::<FreeNode>();
    (*cache).lock();
    (*node).next = (*cache).free_list;
    (*cache).free_list = node;
    (*cache).unlock();
}

pub unsafe fn kmem_cache_destroy(cache: *mut KmCache) {
    if cache.is_null() {
        return;
    }
    let layout = (*cache).layout();
    let cache_layout = Layout::new::<KmCache>();
    (*cache).lock();
    let mut node = (*cache).free_list;
    (*cache).free_list = ptr::null_mut();
    (*cache).unlock();
    while !node.is_null() {
        let next = (*node).next;
        dealloc(node.cast::<u8>(), layout);
        node = next;
    }
    dealloc(cache.cast::<u8>(), cache_layout);
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
            let first = kmem_cache_zalloc::<u64>(cache, 0);
            assert!(!first.is_null());
            assert_eq!(*first, 0);
            *first = 0x1234;
            kmem_cache_free(cache, first.cast());

            let second = kmem_cache_zalloc::<u64>(cache, 0);
            assert_eq!(second, first);
            assert_eq!(*second, 0);
            kmem_cache_free(cache, second.cast());
            kmem_cache_destroy(cache);
        }
    }
}
