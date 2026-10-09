// SPDX-License-Identifier: MIT
// Copyright © 2014-2018 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_gt_buffer_pool.c.
// `__list_del_many()` follows i915_list_util.h (Copyright © 2025 Intel Corporation).
// Full MIT grants are retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{
    ffi::{c_int, c_long, c_void},
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    i915_active_upstream::{__i915_active_init, i915_active_acquire, i915_active_fini},
    i915_gem_object_header_upstream::{
        __i915_gem_object_pin_pages, assert_object_held, i915_gem_object_set_readonly,
        i915_gem_object_unpin_pages,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, intel_bo_to_drm_bo},
    i915_gem_shrinker_upstream::{
        i915_gem_object_make_purgeable, i915_gem_object_make_unshrinkable,
    },
    intel_context_types_upstream::I915Active,
    intel_context_upstream::RcuHead,
    intel_engine_cs_upstream::{DelayedWork, ListHead, WorkStruct},
    intel_gt_buffer_pool_types_upstream::{I915MapType, IntelGtBufferPool, IntelGtBufferPoolNode},
    intel_gt_types_upstream::IntelGt,
    intel_ring_upstream::i915_gem_object_create_internal,
    linux::{
        bits::cmpxchg,
        list::{list_add_rcu, list_del_rcu, list_empty, list_is_last},
        locks::{
            spin_lock_init, spin_lock_irq, spin_trylock_irqsave, spin_unlock_irq,
            spin_unlock_irqrestore,
        },
        memory::{kfree, kmalloc_obj},
        primitives::{jiffies, page_align, round_jiffies_up_relative},
        rcu::call_rcu,
        workqueue::{INIT_DELAYED_WORK, cancel_delayed_work_sync, queue_delayed_work},
    },
    linux_config::{__GFP_NOWARN, GFP_KERNEL, HZ},
};

const __GFP_RETRY_MAYFAIL: u32 = 1 << 14;

/// `obj->base.base.size` header view without implicitly borrowing through the
/// C object/base unions.
unsafe fn object_size(obj: *mut DrmI915GemObject) -> u64 {
    let drm_obj = unsafe { intel_bo_to_drm_bo(obj) };
    unsafe { ptr::read_volatile(ptr::addr_of!((*drm_obj).size)) }
}

/// The source helper atomically detaches the nodes between `head` and `first`.
/// Its ordering and single-copy publication match i915_list_util.h.
// upstream: i915_list_util.h __list_del_many()
#[inline]
unsafe fn __list_del_many(head: *mut ListHead, first: *mut ListHead) {
    unsafe {
        (*first).prev = head;
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*head).next), first);
    }
}

// Linux `kfree_rcu(node, rcu)` adapter for this node's embedded RCU head.
unsafe extern "C" fn node_free_rcu(head: *mut RcuHead) {
    let node = container_of!(head, IntelGtBufferPoolNode, rcu_or_pool.rcu);
    unsafe { kfree(node) };
}

unsafe fn kfree_rcu_node(node: *mut IntelGtBufferPoolNode) {
    let head = unsafe { ptr::addr_of_mut!((*node).rcu_or_pool.rcu).cast::<RcuHead>() };
    call_rcu(head, node_free_rcu);
}

// upstream: intel_gt_buffer_pool.c bucket_for_size()
unsafe fn bucket_for_size(pool: *mut IntelGtBufferPool, sz: usize) -> *mut ListHead {
    let pages = sz >> crate::linux_config::PAGE_SHIFT;
    let mut n = if pages == 0 {
        unsafe { (*pool).cache_list.len() }
    } else {
        (usize::BITS - 1 - pages.leading_zeros()) as usize
    };
    if n >= unsafe { (*pool).cache_list.len() } {
        n = unsafe { (*pool).cache_list.len() - 1 };
    }
    unsafe { ptr::addr_of_mut!((*pool).cache_list[n]) }
}

// upstream: intel_gt_buffer_pool.c node_free()
unsafe fn node_free(node: *mut IntelGtBufferPoolNode) {
    unsafe {
        crate::i915_gem_object_api_upstream::i915_gem_object_put((*node).obj);
        i915_active_fini(ptr::addr_of_mut!((*node).active));
        kfree_rcu_node(node);
    }
}

// upstream: intel_gt_buffer_pool.c pool_free_older_than()
unsafe fn pool_free_older_than(pool: *mut IntelGtBufferPool, keep: c_long) -> bool {
    let mut stale: *mut IntelGtBufferPoolNode = ptr::null_mut();
    let mut active = false;

    for n in 0..unsafe { (*pool).cache_list.len() } {
        let list = unsafe { ptr::addr_of_mut!((*pool).cache_list[n]) };
        if unsafe { list_empty(&*list) } {
            continue;
        }

        let mut flags = 0;
        if spin_trylock_irqsave(unsafe { &mut (*pool).lock }, &mut flags) {
            let mut pos = unsafe { (*list).prev };
            while !ptr::eq(pos, list) {
                let node = container_of!(pos, IntelGtBufferPoolNode, link);
                let age = unsafe { READ_ONCE!((*node).age) };
                if age == 0 || jiffies().wrapping_sub(age) < keep as u64 {
                    break;
                }

                let old_age = unsafe {
                    AtomicU64::from_ptr(ptr::addr_of_mut!((*node).age)).swap(0, Ordering::SeqCst)
                };
                if old_age == 0 {
                    break;
                }

                unsafe { (*node).rcu_or_pool.free = stale };
                stale = node;
                pos = unsafe { (*pos).prev };
            }

            if !unsafe { list_is_last(pos, list) } {
                unsafe { __list_del_many(pos, list) };
            }
            spin_unlock_irqrestore(unsafe { &mut (*pool).lock }, flags);
        }

        active |= !unsafe { list_empty(&*list) };
    }

    while !stale.is_null() {
        let node = stale;
        stale = unsafe { (*node).rcu_or_pool.free };
        unsafe { node_free(node) };
    }
    active
}

// upstream: intel_gt_buffer_pool.c pool_free_work()
unsafe fn pool_free_work(wrk: *mut WorkStruct) {
    let delayed = container_of!(wrk, DelayedWork, work);
    let pool = container_of!(delayed, IntelGtBufferPool, work);
    let gt = container_of!(pool, IntelGt, buffer_pool);

    if unsafe { pool_free_older_than(pool, HZ as c_long) } {
        unsafe {
            queue_delayed_work(
                (*(*gt).i915).unordered_wq,
                &mut (*pool).work,
                round_jiffies_up_relative(HZ as u64),
            );
        }
    }
}

// upstream: intel_gt_buffer_pool.c pool_retire()
unsafe extern "C" fn pool_retire(ref_: *mut I915Active) {
    let node = container_of!(ref_, IntelGtBufferPoolNode, active);
    let pool = unsafe { (*node).rcu_or_pool.pool };
    let gt = container_of!(pool, IntelGt, buffer_pool);
    let size = unsafe { object_size((*node).obj) as usize };
    let list = unsafe { bucket_for_size(pool, size) };

    if unsafe { (*node).pinned != 0 } {
        unsafe {
            i915_gem_object_unpin_pages((*node).obj);
            i915_gem_object_make_purgeable((*node).obj);
            (*node).pinned = 0;
        }
    }

    GEM_BUG_ON!(unsafe { (*node).age != 0 });
    spin_lock_irq(unsafe { &mut (*pool).lock });
    unsafe { list_add_rcu(ptr::addr_of_mut!((*node).link), list) };
    unsafe {
        (*node).age = jiffies().max(1);
    }
    spin_unlock_irq(unsafe { &mut (*pool).lock });

    unsafe {
        queue_delayed_work(
            (*(*gt).i915).unordered_wq,
            &mut (*pool).work,
            round_jiffies_up_relative(HZ as u64),
        );
    }
}

// upstream: intel_gt_buffer_pool.c intel_gt_buffer_pool_mark_used()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_buffer_pool_mark_used(node: *mut IntelGtBufferPoolNode) {
    unsafe { assert_object_held((*node).obj) };
    if unsafe { (*node).pinned != 0 } {
        return;
    }
    unsafe { __i915_gem_object_pin_pages((*node).obj) };
    unsafe {
        i915_gem_object_make_unshrinkable((*node).obj);
        (*node).pinned = 1;
    }
}

// upstream: intel_gt_buffer_pool.c node_create()
unsafe fn node_create(
    pool: *mut IntelGtBufferPool,
    sz: usize,
    type_: I915MapType,
) -> *mut IntelGtBufferPoolNode {
    let gt = container_of!(pool, IntelGt, buffer_pool);
    let node = kmalloc_obj!(
        IntelGtBufferPoolNode,
        GFP_KERNEL | __GFP_RETRY_MAYFAIL | __GFP_NOWARN
    );
    if node.is_null() {
        return ERR_PTR!(-crate::linux_config::ENOMEM) as *mut IntelGtBufferPoolNode;
    }

    unsafe {
        (*node).age = 0;
        (*node).rcu_or_pool.pool = pool;
        (*node).pinned = 0;
        __i915_active_init(
            ptr::addr_of_mut!((*node).active),
            None,
            Some(pool_retire),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        );
    }

    let obj = unsafe { i915_gem_object_create_internal((*gt).i915, sz as u64) };
    if IS_ERR!(obj) {
        unsafe {
            i915_active_fini(ptr::addr_of_mut!((*node).active));
            kfree(node);
        }
        return ERR_PTR!(PTR_ERR!(obj)) as *mut IntelGtBufferPoolNode;
    }

    unsafe {
        i915_gem_object_set_readonly(obj);
        (*node).r#type = type_;
        (*node).obj = obj;
    }
    node
}

// upstream: intel_gt_buffer_pool.c intel_gt_get_buffer_pool()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_get_buffer_pool(
    gt: *mut IntelGt,
    size: usize,
    type_: I915MapType,
) -> *mut IntelGtBufferPoolNode {
    let pool = unsafe { ptr::addr_of_mut!((*gt).buffer_pool) };
    let size = page_align(size);
    let list = unsafe { bucket_for_size(pool, size) };
    let mut node = ptr::null_mut();

    crate::linux::rcu::rcu_read_lock();
    let mut pos = unsafe { (*list).next };
    let mut found = false;
    while !ptr::eq(pos, list) {
        node = container_of!(pos, IntelGtBufferPoolNode, link);
        let next = unsafe { (*pos).next };

        if unsafe { object_size((*node).obj) } < size as u64 || unsafe { (*node).r#type != type_ } {
            pos = next;
            continue;
        }

        let age = unsafe { READ_ONCE!((*node).age) };
        if age == 0 {
            pos = next;
            continue;
        }

        if unsafe { cmpxchg(ptr::addr_of_mut!((*node).age), age, 0) } == age {
            spin_lock_irq(unsafe { &mut (*pool).lock });
            unsafe { list_del_rcu(ptr::addr_of_mut!((*node).link)) };
            spin_unlock_irq(unsafe { &mut (*pool).lock });
            found = true;
            break;
        }
        pos = next;
    }
    crate::linux::rcu::rcu_read_unlock();

    if !found {
        node = unsafe { node_create(pool, size, type_) };
        if IS_ERR!(node) {
            return node;
        }
    }

    let ret = unsafe { i915_active_acquire(ptr::addr_of_mut!((*node).active)) };
    if ret != 0 {
        unsafe { node_free(node) };
        return ERR_PTR!(ret) as *mut IntelGtBufferPoolNode;
    }
    node
}

// upstream: intel_gt_buffer_pool.c intel_gt_init_buffer_pool()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_init_buffer_pool(gt: *mut IntelGt) {
    let pool = unsafe { ptr::addr_of_mut!((*gt).buffer_pool) };
    spin_lock_init(unsafe { &mut (*pool).lock });
    for n in 0..unsafe { (*pool).cache_list.len() } {
        unsafe { crate::linux_list::INIT_LIST_HEAD(ptr::addr_of_mut!((*pool).cache_list[n])) };
    }
    unsafe {
        INIT_DELAYED_WORK(&mut (*pool).work, |work| {
            pool_free_work(work as *mut WorkStruct)
        });
    }
}

// upstream: intel_gt_buffer_pool.c intel_gt_flush_buffer_pool()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_flush_buffer_pool(gt: *mut IntelGt) {
    let pool = unsafe { ptr::addr_of_mut!((*gt).buffer_pool) };
    loop {
        while unsafe { pool_free_older_than(pool, 0) } {}
        if !unsafe { cancel_delayed_work_sync(&mut (*pool).work) } {
            break;
        }
    }
}

// upstream: intel_gt_buffer_pool.c intel_gt_fini_buffer_pool()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_fini_buffer_pool(gt: *mut IntelGt) {
    let pool = unsafe { ptr::addr_of_mut!((*gt).buffer_pool) };
    for n in 0..unsafe { (*pool).cache_list.len() } {
        GEM_BUG_ON!(!unsafe { list_empty(&(*pool).cache_list[n]) });
    }
}
