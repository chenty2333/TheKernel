// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation.
// Source-order translation of Linux 7.2.3 drivers/gpu/drm/i915/i915_vma_resource.c.
//
// The pending-unbind set is the source's interval tree keyed by VMA_RES_START().
// Iterators walk the same red-black tree in key order and keep nodes that overlap
// the query range, which yields the same nodes in the same order as the
// augmented interval-tree iterators.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::offset_of,
    ptr,
};

use crate::{
    i915_gem_clflush_upstream::{dma_fence_get, dma_fence_init, dma_fence_put, dma_fence_signal, dma_fence_wait},
    i915_request_upstream::DmaFenceOps,
    i915_sw_fence_upstream::{
        i915_sw_fence_await_dma_fence, i915_sw_fence_commit, i915_sw_fence_fini, i915_sw_fence_init,
    },
    i915_vma_resource_types_upstream::{I915RefctSgt, I915VmaResource},
    intel_context_types_upstream::{I915SwFence, I915SwFenceNotify},
    intel_context_upstream::{DmaFence, I915AddressSpace, Kref, RcuHead, SgTable},
    intel_engine_cs_upstream::{RbNode, RbRootCached, WorkStruct},
    intel_gtt_api_upstream::{I915_GTT_PAGE_SIZE, I915VmaOpsLayout, i915_vm_has_cache_coloring},
    intel_runtime_pm_upstream::intel_runtime_pm_get_if_in_use,
    linux::{
        locks::spin_lock_init,
        memory::{kfree, kzalloc_obj, refcount_dec_and_test, refcount_inc_not_zero, refcount_set},
        mutex::{mutex_lock, mutex_unlock},
        pm::intel_runtime_pm_put,
        rbtree::{RB_EMPTY_NODE, RB_CLEAR_NODE, rb_erase_cached, rb_insert_color, rb_link_node, rb_next},
        workqueue::{INIT_WORK_C, queue_work, system_dfl_wq},
    },
    linux_config::ENOMEM,
};

const NOTIFY_DONE: c_int = 0;
const GFP_KERNEL: u32 = 0x0cc0;
const U64_MAX: u64 = u64::MAX;

// upstream: i915_vma_resource.c get_driver_name()
unsafe extern "C" fn get_driver_name(_fence: *mut DmaFence) -> *const c_char {
    c"vma unbind fence".as_ptr()
}

// upstream: i915_vma_resource.c get_timeline_name()
unsafe extern "C" fn get_timeline_name(_fence: *mut DmaFence) -> *const c_char {
    c"unbound".as_ptr()
}

// container_of(unbind_fence) -> i915_vma_resource
unsafe fn vma_from_unbind_fence(fence: *mut DmaFence) -> *mut I915VmaResource {
    fence
        .cast::<u8>()
        .sub(offset_of!(I915VmaResource, unbind_fence))
        .cast()
}

// container_of(chain) -> i915_vma_resource
unsafe fn vma_from_chain(fence: *mut I915SwFence) -> *mut I915VmaResource {
    fence.cast::<u8>().sub(offset_of!(I915VmaResource, chain)).cast()
}

// container_of(work) -> i915_vma_resource
unsafe fn vma_from_work(work: *mut WorkStruct) -> *mut I915VmaResource {
    work.cast::<u8>().sub(offset_of!(I915VmaResource, work)).cast()
}

// upstream: i915_vma_resource.c i915_vma_resource_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_resource_alloc() -> *mut I915VmaResource {
    let vma_res: *mut I915VmaResource = kzalloc_obj();
    if vma_res.is_null() {
        // ERR_PTR(-ENOMEM)
        (-(ENOMEM as isize)) as usize as *mut I915VmaResource
    } else {
        vma_res
    }
}

// C bitfields `allocated`, `immediate_unbind`, `needs_wakeref`, `skip_pte_rewrite`.
const STATE_IMMEDIATE_UNBIND: u8 = 1 << 1;
const STATE_NEEDS_WAKEREF: u8 = 1 << 2;
const STATE_SKIP_PTE_REWRITE: u8 = 1 << 3;

unsafe fn vma_res_bit(vma_res: *const I915VmaResource, bit: u8) -> bool {
    unsafe { (*vma_res).state_bits & bit != 0 }
}

unsafe fn vma_res_set_bit(vma_res: *mut I915VmaResource, bit: u8, value: bool) {
    unsafe {
        if value {
            (*vma_res).state_bits |= bit;
        } else {
            (*vma_res).state_bits &= !bit;
        }
    }
}

// upstream: i915_vma_resource.c i915_vma_resource_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_resource_free(vma_res: *mut I915VmaResource) {
    if !vma_res.is_null() {
        unsafe { kfree(vma_res) };
    }
}

// upstream: i915_vma_resource.c unbind_fence_free_rcu()
unsafe extern "C" fn unbind_fence_free_rcu(head: *mut RcuHead) {
    // container_of(head, typeof(*vma_res), unbind_fence.rcu)
    let fence = head.cast::<u8>().sub(offset_of!(DmaFence, timestamp_union)).cast::<DmaFence>();
    unsafe { i915_vma_resource_free(vma_from_unbind_fence(fence)) };
}

// upstream: i915_vma_resource.c unbind_fence_release()
unsafe extern "C" fn unbind_fence_release(fence: *mut DmaFence) {
    let vma_res = unsafe { vma_from_unbind_fence(fence) };
    unsafe {
        i915_sw_fence_fini(ptr::addr_of_mut!((*vma_res).chain));
        crate::linux::rcu::call_rcu(
            ptr::addr_of_mut!((*fence).timestamp_union.rcu).cast::<RcuHead>(),
            unbind_fence_free_rcu,
        );
    }
}

// upstream: i915_vma_resource.c unbind_fence_ops
static UNBIND_FENCE_OPS: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(get_driver_name),
    get_timeline_name: Some(get_timeline_name),
    enable_signaling: None,
    signaled: None,
    wait: None,
    release: Some(unbind_fence_release),
    set_deadline: None,
};

// upstream: i915_vma_resource.c __i915_vma_resource_unhold()
unsafe fn __i915_vma_resource_unhold(vma_res: *mut I915VmaResource) {
    if !unsafe { refcount_dec_and_test(&mut (*vma_res).hold_count) } {
        return;
    }

    unsafe { dma_fence_signal(ptr::addr_of_mut!((*vma_res).unbind_fence)) };

    let vm = unsafe { (*vma_res).vm };
    let wakeref = unsafe { (*vma_res).wakeref };
    if !wakeref.is_null() {
        unsafe { intel_runtime_pm_put(ptr::addr_of_mut!((*(*vm).i915).runtime_pm), wakeref) };
    }

    unsafe { (*vma_res).vm = ptr::null_mut() };

    if !unsafe { RB_EMPTY_NODE(ptr::addr_of_mut!((*vma_res).rb)) } {
        unsafe {
            mutex_lock(ptr::addr_of_mut!((*vm).mutex));
            vma_res_itree_remove(vma_res, ptr::addr_of_mut!((*vm).pending_unbind));
            mutex_unlock(ptr::addr_of_mut!((*vm).mutex));
        }
    }

    let rsgt = unsafe { (*vma_res).bi.pages_rsgt };
    if !rsgt.is_null() {
        unsafe { i915_refct_sgt_put(rsgt) };
    }
}

// upstream: include/i915_scatterlist.h i915_refct_sgt_put()
#[repr(C)]
struct I915RefctSgtLayout {
    kref: Kref,
    table: SgTable,
    size: usize,
    ops: *const I915RefctSgtOps,
}

#[repr(C)]
struct I915RefctSgtOps {
    release: unsafe extern "C" fn(*mut Kref),
}

unsafe fn i915_refct_sgt_put(rsgt: *mut I915RefctSgt) {
    let view = rsgt.cast::<I915RefctSgtLayout>();
    unsafe {
        let release = (*(*view).ops).release;
        crate::linux::memory::kref_put(ptr::addr_of_mut!((*view).kref), release);
    }
}

// upstream: i915_vma_resource.c i915_vma_resource_unhold()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_resource_unhold(vma_res: *mut I915VmaResource, _lockdep_cookie: bool) {
    // CONFIG_LOCKDEP is off: dma_fence_end_signalling() and the PROVE_LOCKING
    // spinlock round trip are no-ops in this configuration.
    unsafe { __i915_vma_resource_unhold(vma_res) };
}

// upstream: i915_vma_resource.c i915_vma_resource_hold()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_resource_hold(vma_res: *mut I915VmaResource, lockdep_cookie: *mut bool) -> bool {
    let held = unsafe { refcount_inc_not_zero(&mut (*vma_res).hold_count) };
    if held {
        // dma_fence_begin_signalling() is `true` without CONFIG_LOCKDEP.
        unsafe { *lockdep_cookie = true };
    }
    held
}

// upstream: i915_vma_resource.c i915_vma_resource_unbind_work()
unsafe extern "C" fn i915_vma_resource_unbind_work(work: *mut WorkStruct) {
    let vma_res = unsafe { vma_from_work(work) };
    let vm = unsafe { (*vma_res).vm };
    if !unsafe { vma_res_bit(vma_res, STATE_SKIP_PTE_REWRITE) } {
        let ops = unsafe { (*vma_res).ops.cast::<I915VmaOpsLayout>() };
        if let Some(unbind) = unsafe { (*ops).unbind_vma } {
            unsafe { unbind(vm, vma_res) };
        }
    }
    unsafe {
        __i915_vma_resource_unhold(vma_res);
        i915_vma_resource_put(vma_res);
    }
}

unsafe fn i915_vma_resource_put(vma_res: *mut I915VmaResource) {
    unsafe { dma_fence_put(ptr::addr_of_mut!((*vma_res).unbind_fence)) };
}

// upstream: i915_vma_resource.c i915_vma_resource_fence_notify()
unsafe extern "C" fn i915_vma_resource_fence_notify(
    fence: *mut I915SwFence,
    state: I915SwFenceNotify,
) -> c_int {
    let vma_res = unsafe { vma_from_chain(fence) };
    let unbind_fence = unsafe { ptr::addr_of_mut!((*vma_res).unbind_fence) };
    match state {
        I915SwFenceNotify::FenceComplete => {
            unsafe { dma_fence_get(unbind_fence) };
            if unsafe { vma_res_bit(vma_res, STATE_IMMEDIATE_UNBIND) } {
                unsafe { i915_vma_resource_unbind_work(ptr::addr_of_mut!((*vma_res).work)) };
            } else {
                unsafe {
                    INIT_WORK_C(&mut (*vma_res).work, i915_vma_resource_unbind_work);
                    queue_work(system_dfl_wq, ptr::addr_of_mut!((*vma_res).work));
                }
            }
        }
        I915SwFenceNotify::FenceFree => unsafe { i915_vma_resource_put(vma_res) },
    }
    NOTIFY_DONE
}

// upstream: i915_vma_resource.c i915_vma_resource_unbind()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_resource_unbind(vma_res: *mut I915VmaResource, tlb: *mut u32) -> *mut DmaFence {
    let vm = unsafe { (*vma_res).vm };
    unsafe {
        (*vma_res).tlb = tlb;
        dma_fence_get(ptr::addr_of_mut!((*vma_res).unbind_fence));
    }

    if unsafe { vma_res_bit(vma_res, STATE_NEEDS_WAKEREF) } {
        let wakeref = unsafe {
            intel_runtime_pm_get_if_in_use(ptr::addr_of_mut!((*(*vm).i915).runtime_pm).cast())
        };
        unsafe { (*vma_res).wakeref = wakeref };
    }

    let pending = unsafe { crate::linux::memory::atomic_read(&(*vma_res).chain.pending) };
    if pending <= 1 {
        unsafe {
            RB_CLEAR_NODE(ptr::addr_of_mut!((*vma_res).rb));
            vma_res_set_bit(vma_res, STATE_IMMEDIATE_UNBIND, true);
        }
    } else {
        unsafe { vma_res_itree_insert(vma_res, ptr::addr_of_mut!((*vm).pending_unbind)) };
    }

    unsafe { i915_sw_fence_commit(ptr::addr_of_mut!((*vma_res).chain)) };
    unsafe { ptr::addr_of_mut!((*vma_res).unbind_fence) }
}

// upstream: i915_vma_resource.c __i915_vma_resource_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_vma_resource_init(vma_res: *mut I915VmaResource) {
    unsafe {
        spin_lock_init(&mut (*vma_res).lock);
        dma_fence_init(
            ptr::addr_of_mut!((*vma_res).unbind_fence),
            ptr::addr_of!(UNBIND_FENCE_OPS),
            ptr::addr_of_mut!((*vma_res).lock),
            0,
            0,
        );
        refcount_set(&mut (*vma_res).hold_count, 1);
        i915_sw_fence_init(
            ptr::addr_of_mut!((*vma_res).chain),
            Some(i915_vma_resource_fence_notify),
        );
    }
}

// upstream: i915_vma_resource.c i915_vma_resource_color_adjust_range()
unsafe fn i915_vma_resource_color_adjust_range(vm: *mut I915AddressSpace, start: *mut u64, end: *mut u64) {
    if unsafe { i915_vm_has_cache_coloring(vm) } {
        unsafe {
            if *start != 0 {
                *start -= I915_GTT_PAGE_SIZE;
            }
            *end += I915_GTT_PAGE_SIZE;
        }
    }
}

// upstream: i915_vma_resource.c vma_res_itree (VMA_RES_START/VMA_RES_LAST)
unsafe fn vma_res_start(node: *const I915VmaResource) -> u64 {
    unsafe { (*node).start.wrapping_sub((*node).guard as u64) }
}

unsafe fn vma_res_last(node: *const I915VmaResource) -> u64 {
    unsafe {
        (*node)
            .start
            .wrapping_add((*node).node_size)
            .wrapping_add((*node).guard as u64)
            .wrapping_sub(1)
    }
}

unsafe fn vma_from_rb(node: *mut RbNode) -> *mut I915VmaResource {
    node.cast::<u8>().sub(offset_of!(I915VmaResource, rb)).cast()
}

// Interval-tree insert keyed by VMA_RES_START(), the same ordering as
// interval_tree_insert(). Keeps the cached leftmost pointer.
unsafe fn vma_res_itree_insert(node: *mut I915VmaResource, root: *mut RbRootCached) {
    let key = unsafe { vma_res_start(node) };
    let mut link: *mut *mut RbNode = unsafe { ptr::addr_of_mut!((*root).root.node) };
    let mut parent: *mut RbNode = ptr::null_mut();
    let mut leftmost = true;
    unsafe {
        while !(*link).is_null() {
            parent = *link;
            let parent_vma = vma_from_rb(parent);
            if key <= vma_res_start(parent_vma) {
                link = ptr::addr_of_mut!((*parent).left);
            } else {
                leftmost = false;
                link = ptr::addr_of_mut!((*parent).right);
            }
        }
        let rb = ptr::addr_of_mut!((*node).rb);
        rb_link_node(rb, parent, link);
        rb_insert_color(rb, ptr::addr_of_mut!((*root).root));
        if leftmost {
            (*root).leftmost = rb;
        }
    }
}

unsafe fn vma_res_itree_remove(node: *mut I915VmaResource, root: *mut RbRootCached) {
    unsafe { rb_erase_cached(&mut (*node).rb, &mut *root) };
}

fn overlaps(node: *const I915VmaResource, first: u64, last: u64) -> bool {
    unsafe { vma_res_start(node) <= last && vma_res_last(node) >= first }
}

// First node in key order whose [START, LAST] overlaps [first, last].
unsafe fn vma_res_itree_iter_first(root: *mut RbRootCached, first: u64, last: u64) -> *mut I915VmaResource {
    let mut node = unsafe { (*root).leftmost };
    while !node.is_null() {
        let vma = unsafe { vma_from_rb(node) };
        if overlaps(vma, first, last) {
            return vma;
        }
        node = unsafe { rb_next(node) };
    }
    ptr::null_mut()
}

// Next overlapping node after `vma` in key order.
unsafe fn vma_res_itree_iter_next(vma: *mut I915VmaResource, first: u64, last: u64) -> *mut I915VmaResource {
    let mut node = unsafe { rb_next(ptr::addr_of_mut!((*vma).rb)) };
    while !node.is_null() {
        let next = unsafe { vma_from_rb(node) };
        if overlaps(next, first, last) {
            return next;
        }
        node = unsafe { rb_next(node) };
    }
    ptr::null_mut()
}

// upstream: i915_vma_resource.c i915_vma_resource_bind_dep_sync()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_resource_bind_dep_sync(
    vm: *mut I915AddressSpace,
    mut offset: u64,
    size: u64,
    intr: bool,
) -> c_int {
    let mut last = offset.wrapping_add(size).wrapping_sub(1);
    unsafe { i915_vma_resource_color_adjust_range(vm, &mut offset, &mut last) };
    let pending = unsafe { ptr::addr_of_mut!((*vm).pending_unbind) };
    let mut node = unsafe { vma_res_itree_iter_first(pending, offset, last) };
    while !node.is_null() {
        let ret = unsafe { dma_fence_wait(ptr::addr_of_mut!((*node).unbind_fence), intr) };
        if ret != 0 {
            return ret;
        }
        node = unsafe { vma_res_itree_iter_next(node, offset, last) };
    }
    0
}

// upstream: i915_vma_resource.c i915_vma_resource_bind_dep_sync_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_resource_bind_dep_sync_all(vm: *mut I915AddressSpace) {
    loop {
        let mut fence: *mut DmaFence = ptr::null_mut();
        let node;
        unsafe {
            mutex_lock(ptr::addr_of_mut!((*vm).mutex));
            node = vma_res_itree_iter_first(ptr::addr_of_mut!((*vm).pending_unbind), 0, U64_MAX);
            if !node.is_null() {
                fence = crate::linux::requests::dma_fence_get_rcu(ptr::addr_of_mut!((*node).unbind_fence));
            }
            mutex_unlock(ptr::addr_of_mut!((*vm).mutex));
            if !fence.is_null() {
                dma_fence_wait(fence, false);
                dma_fence_put(fence);
            }
        }
        if node.is_null() {
            break;
        }
    }
}

// upstream: i915_vma_resource.c i915_vma_resource_bind_dep_await()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_resource_bind_dep_await(
    vm: *mut I915AddressSpace,
    sw_fence: *mut I915SwFence,
    mut offset: u64,
    size: u64,
    intr: bool,
    gfp: u32,
) -> c_int {
    let mut last = offset.wrapping_add(size).wrapping_sub(1);
    unsafe { i915_vma_resource_color_adjust_range(vm, &mut offset, &mut last) };
    let pending = unsafe { ptr::addr_of_mut!((*vm).pending_unbind) };
    let mut node = unsafe { vma_res_itree_iter_first(pending, offset, last) };
    while !node.is_null() {
        let mut ret = unsafe {
            i915_sw_fence_await_dma_fence(sw_fence, ptr::addr_of_mut!((*node).unbind_fence), 0, gfp as c_ulong)
        };
        if ret < 0 {
            ret = unsafe { dma_fence_wait(ptr::addr_of_mut!((*node).unbind_fence), intr) };
            if ret != 0 {
                return ret;
            }
        }
        node = unsafe { vma_res_itree_iter_next(node, offset, last) };
    }
    0
}
