// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation. Full MIT grant: LICENSE-MIT.
// Source-faithful Rust translation of Linux 7.2.3
// drivers/gpu/drm/i915/i915_active.c. The active-fence retirement, timeline
// tree, barrier ownership, callback and wait ordering are retained below.
// Kernel framework calls remain explicit LinuxKPI/upstream integration calls.
#![allow(
    non_snake_case,
    non_camel_case_types,
    unsafe_op_in_unsafe_fn,
    unexpected_cfgs
)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::offset_of,
    ptr,
};

use crate::{
    i915_request_types_upstream::{I915Request, i915_request_timeline},
    intel_context_types_upstream::{ActiveNode, I915Active, I915SwFence},
    intel_context_upstream::{
        DmaFence, DmaFenceCb, I915ActiveFence, Kref, WaitQueueEntry, WaitQueueHead,
    },
    intel_engine_cs_upstream::{
        IntelEngineCs, ListHead, LlistHead, LlistNode, RbNode, RbRoot, Spinlock, WorkStruct,
    },
    intel_gt_types_upstream::IntelGt,
    linux::{bits::*, locks::*, memory::*, rbtree::*, rcu::*, wait::*, workqueue::*},
    linux_config::*,
    linux_heap::{
        KmCache, SLAB_HWCACHE_ALIGN, kmem_cache_create, kmem_cache_destroy, kmem_cache_free,
        kmem_cache_zalloc,
    },
    linux_list::*,
};

// Canonical source-owned layout: private `active_node` record defined in this C file.

static mut SLAB_CACHE: *mut KmCache = ptr::null_mut();
const I915_ACTIVE_RETIRE_SLEEPS: c_ulong = 1 << 0;
const I915_ACTIVE_AWAIT_EXCL: u32 = 1 << 0;
const I915_ACTIVE_AWAIT_ACTIVE: u32 = 1 << 1;
const I915_ACTIVE_AWAIT_BARRIER: u32 = 1 << 2;

#[repr(C)]
#[cfg(feature = "i915-debug-gem")]
struct DebugObjDescr {
    name: *const u8,
    debug_hint: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    is_static_object: Option<unsafe extern "C" fn(*mut c_void) -> bool>,
    fixup_init: Option<unsafe extern "C" fn(*mut c_void, c_int) -> bool>,
    fixup_activate: Option<unsafe extern "C" fn(*mut c_void, c_int) -> bool>,
    fixup_destroy: Option<unsafe extern "C" fn(*mut c_void, c_int) -> bool>,
    fixup_free: Option<unsafe extern "C" fn(*mut c_void, c_int) -> bool>,
    fixup_assert_init: Option<unsafe extern "C" fn(*mut c_void, c_int) -> bool>,
}
#[cfg(feature = "i915-debug-gem")]
unsafe impl Sync for DebugObjDescr {}
#[cfg(feature = "i915-debug-gem")]
static ACTIVE_DEBUG_DESC: DebugObjDescr = DebugObjDescr {
    name: b"i915_active\0".as_ptr(),
    debug_hint: Some(active_debug_hint),
    is_static_object: None,
    fixup_init: None,
    fixup_activate: None,
    fixup_destroy: None,
    fixup_free: None,
    fixup_assert_init: None,
};

unsafe extern "C" {
    fn i915_request_await_dma_fence(rq: *mut I915Request, fence: *mut DmaFence) -> c_int;
    fn i915_sw_fence_await_dma_fence(
        sw: *mut I915SwFence,
        fence: *mut DmaFence,
        timeout: core::ffi::c_ulong,
        gfp: u32,
    ) -> c_int;
    fn i915_sw_fence_await(sw: *mut I915SwFence) -> bool;
    fn i915_sw_fence_complete(sw: *mut I915SwFence);
    fn dma_fence_enable_sw_signaling(fence: *mut DmaFence);
    fn dma_fence_put(fence: *mut DmaFence);
    fn dma_fence_lock_irqsave(fence: *mut DmaFence, flags: *mut c_ulong);
    fn dma_fence_unlock_irqrestore(fence: *mut DmaFence, flags: c_ulong);
    fn dma_fence_spinlock(fence: *mut DmaFence) -> *mut Spinlock;
    fn intel_engine_flush_barriers(engine: *mut IntelEngineCs) -> c_int;
    fn intel_engine_pm_get(engine: *mut IntelEngineCs);
    fn intel_engine_pm_put(engine: *mut IntelEngineCs);
    fn intel_engine_pm_put_delay(engine: *mut IntelEngineCs, delay: u64);
    fn intel_engine_pm_is_awake(engine: *mut IntelEngineCs) -> bool;
    fn intel_engine_is_virtual(engine: *mut IntelEngineCs) -> bool;
    fn intel_context_is_barrier(
        context: *mut crate::intel_context_types_upstream::IntelContext,
    ) -> bool;
    fn add_wait_queue(q: *mut WaitQueueHead, entry: *mut WaitQueueEntry);
    fn __var_waitqueue(addr: *mut c_void) -> *mut WaitQueueHead;
    fn wake_up_var(addr: *mut c_void);
    fn flush_work(work: *mut WorkStruct) -> bool;
    fn queue_work(wq: *mut c_void, work: *mut WorkStruct) -> bool;
    // Linux wait-var macro integration: sleeps on this address until the
    // caller's active-count predicate is true or the task is interrupted.
    fn linux_wait_var_event_interruptible(addr: *mut c_void, state: c_int) -> c_int;
    static system_dfl_wq: *mut c_void;
    fn mutex_lock_interruptible(lock: *mut crate::intel_engine_cs_upstream::Mutex) -> c_int;
    fn mutex_unlock(lock: *mut crate::intel_engine_cs_upstream::Mutex);
    fn mutex_destroy(lock: *mut crate::intel_engine_cs_upstream::Mutex);
    fn __mutex_init(
        lock: *mut crate::intel_engine_cs_upstream::Mutex,
        name: *const u8,
        key: *mut c_void,
    );
    fn spin_lock_irq(lock: *mut Spinlock);
    fn spin_unlock_irq(lock: *mut Spinlock);
    fn spin_lock_irqsave(lock: *mut Spinlock, flags: *mut c_ulong);
    fn spin_lock_irqsave_nested(lock: *mut Spinlock, flags: *mut c_ulong, subclass: u32);
    fn spin_unlock_irqrestore(lock: *mut Spinlock, flags: c_ulong);
    fn spin_lock_nested(lock: *mut Spinlock, subclass: u32);
    fn spin_unlock(lock: *mut Spinlock);
    fn debug_object_init(addr: *mut c_void, desc: *const c_void);
    fn debug_object_activate(addr: *mut c_void, desc: *const c_void);
    fn debug_object_deactivate(addr: *mut c_void, desc: *const c_void);
    fn debug_object_free(addr: *mut c_void, desc: *const c_void);
    fn debug_object_assert_init(addr: *mut c_void, desc: *const c_void);
    fn rb_erase(node: *mut RbNode, root: *mut RbRoot);
    fn rb_insert_color(node: *mut RbNode, root: *mut RbRoot);
    fn rb_link_node(node: *mut RbNode, parent: *mut RbNode, link: *mut *mut RbNode);
    fn kmalloc(size: usize, flags: u32) -> *mut c_void;
    fn kfree(ptr: *mut c_void);
    fn kref_get(kref: *mut Kref);
    fn kref_put(kref: *mut Kref, release: Option<unsafe extern "C" fn(*mut Kref)>) -> bool;
}

// Header inline: hold RCU while obtaining a reference to the active fence.
#[inline]
pub unsafe fn i915_active_fence_get(active: *mut I915ActiveFence) -> *mut DmaFence {
    unsafe {
        rcu_read_lock();
        let fence = ptr::read_volatile(&(*active).fence);
        let held = if fence.is_null() || (fence as isize) < 0 {
            fence
        } else {
            crate::linux::requests::dma_fence_get_rcu(fence)
        };
        rcu_read_unlock();
        held
    }
}

// upstream: i915_active.h i915_active_init() (CONFIG_LOCKDEP=n expansion)
pub unsafe fn i915_active_init(
    active: *mut I915Active,
    on_active: unsafe extern "C" fn(*mut I915Active) -> c_int,
    on_retire: unsafe extern "C" fn(*mut I915Active),
    flags: c_ulong,
) {
    unsafe {
        __i915_active_init(
            active,
            Some(on_active),
            Some(on_retire),
            flags,
            ptr::null_mut(),
            ptr::null_mut(),
        );
    }
}
#[inline]
unsafe fn i915_active_fence_isset(active: *const I915ActiveFence) -> bool {
    unsafe { !ptr::read_volatile(&(*active).fence).is_null() }
}
#[inline]
unsafe fn __i915_active_fence_init(
    active: *mut I915ActiveFence,
    fence: *mut DmaFence,
    cb: Option<unsafe extern "C" fn(*mut DmaFence, *mut DmaFenceCb)>,
) {
    unsafe {
        (*active).fence = fence;
        (*active).cb.func = cb.or(Some(i915_active_noop));
    }
}
#[inline]
pub unsafe fn __i915_active_acquire(ref_: *mut I915Active) {
    unsafe {
        assert!(atomic_read(&(*ref_).count) != 0);
        atomic_inc(&mut (*ref_).count);
    }
}

// Linux's postorder iterator permits deleting the current entry after first
// capturing the next node, matching the safe iteration contract in this file.
unsafe fn rb_first_postorder(root: *mut RbRoot) -> *mut RbNode {
    let mut node = unsafe { (*root).node };
    if node.is_null() {
        return node;
    }
    loop {
        unsafe {
            if !(*node).left.is_null() {
                node = (*node).left;
            } else if !(*node).right.is_null() {
                node = (*node).right;
            } else {
                return node;
            }
        }
    }
}
unsafe fn rb_next_postorder(node: *mut RbNode) -> *mut RbNode {
    if node.is_null() {
        return node;
    }
    unsafe {
        let parent = ((*node).parent_color & !3) as *mut RbNode;
        if parent.is_null() || ptr::eq(node, (*parent).right) || (*parent).right.is_null() {
            return parent;
        }
        let mut next = (*parent).right;
        loop {
            if !(*next).left.is_null() {
                next = (*next).left;
            } else if !(*next).right.is_null() {
                next = (*next).right;
            } else {
                return next;
            }
        }
    }
}

// upstream: i915_active.c node_from_active()
#[inline]
unsafe fn node_from_active(active: *mut I915ActiveFence) -> *mut ActiveNode {
    unsafe { active.cast::<u8>().sub(offset_of!(ActiveNode, base)).cast() }
}
#[inline]
unsafe fn fetch_node(node: *mut RbNode) -> *mut ActiveNode {
    if node.is_null() {
        ptr::null_mut()
    } else {
        unsafe { node.cast::<u8>().sub(offset_of!(ActiveNode, node)).cast() }
    }
}
// upstream: i915_active.c is_barrier()
#[inline]
unsafe fn is_barrier(active: *const I915ActiveFence) -> bool {
    unsafe { ((*(active.cast_mut())).fence as isize) < 0 }
}
// upstream: i915_active.c barrier_to_ll()
#[inline]
unsafe fn barrier_to_ll(node: *mut ActiveNode) -> *mut LlistNode {
    unsafe { &mut (*node).base.cb.node as *mut ListHead as *mut LlistNode }
}
// upstream: i915_active.c __barrier_to_engine()
#[inline]
unsafe fn __barrier_to_engine(node: *mut ActiveNode) -> *mut IntelEngineCs {
    unsafe { (*node).base.cb.node.prev.cast() }
}
// upstream: i915_active.c barrier_to_engine()
#[inline]
unsafe fn barrier_to_engine(node: *mut ActiveNode) -> *mut IntelEngineCs {
    unsafe {
        assert!(is_barrier(&(*node).base));
        __barrier_to_engine(node)
    }
}
// upstream: i915_active.c barrier_from_ll()
#[inline]
unsafe fn barrier_from_ll(ll: *mut LlistNode) -> *mut ActiveNode {
    unsafe {
        ll.cast::<u8>()
            .sub(
                offset_of!(ActiveNode, base)
                    + offset_of!(I915ActiveFence, cb)
                    + offset_of!(DmaFenceCb, node),
            )
            .cast()
    }
}

// upstream: i915_active.c active_debug_hint()
#[cfg(feature = "i915-debug-gem")]
unsafe extern "C" fn active_debug_hint(addr: *mut c_void) -> *mut c_void {
    let r = addr.cast::<I915Active>();
    unsafe {
        if let Some(f) = (*r).active {
            f as *const () as *mut c_void
        } else if let Some(f) = (*r).retire {
            f as *const () as *mut c_void
        } else {
            r.cast()
        }
    }
}
// upstream: i915_active.c debug_active_init()
#[cfg(feature = "i915-debug-gem")]
unsafe fn debug_active_init(ref_: *mut I915Active) {
    unsafe {
        debug_object_init(
            ref_.cast(),
            (&ACTIVE_DEBUG_DESC as *const DebugObjDescr).cast(),
        )
    }
}
// upstream: i915_active.c debug_active_activate()
#[cfg(feature = "i915-debug-gem")]
unsafe fn debug_active_activate(ref_: *mut I915Active) {
    unsafe {
        debug_object_activate(
            ref_.cast(),
            (&ACTIVE_DEBUG_DESC as *const DebugObjDescr).cast(),
        )
    }
}
// upstream: i915_active.c debug_active_deactivate()
#[cfg(feature = "i915-debug-gem")]
unsafe fn debug_active_deactivate(ref_: *mut I915Active) {
    unsafe {
        debug_object_deactivate(
            ref_.cast(),
            (&ACTIVE_DEBUG_DESC as *const DebugObjDescr).cast(),
        )
    }
}
// upstream: i915_active.c debug_active_fini()
#[cfg(feature = "i915-debug-gem")]
unsafe fn debug_active_fini(ref_: *mut I915Active) {
    unsafe {
        debug_object_free(
            ref_.cast(),
            (&ACTIVE_DEBUG_DESC as *const DebugObjDescr).cast(),
        )
    }
}
// upstream: i915_active.c debug_active_assert()
#[cfg(feature = "i915-debug-gem")]
unsafe fn debug_active_assert(ref_: *mut I915Active) {
    unsafe {
        debug_object_assert_init(
            ref_.cast(),
            (&ACTIVE_DEBUG_DESC as *const DebugObjDescr).cast(),
        )
    }
}

// These are the source's #else implementations when either debug config is
// disabled. They intentionally have no side effects, exactly as upstream.
// upstream: i915_active.c debug_active_init()
#[cfg(not(feature = "i915-debug-gem"))]
unsafe fn debug_active_init(_ref_: *mut I915Active) {}
// upstream: i915_active.c debug_active_activate()
#[cfg(not(feature = "i915-debug-gem"))]
unsafe fn debug_active_activate(_ref_: *mut I915Active) {}
// upstream: i915_active.c debug_active_deactivate()
#[cfg(not(feature = "i915-debug-gem"))]
unsafe fn debug_active_deactivate(_ref_: *mut I915Active) {}
// upstream: i915_active.c debug_active_fini()
#[cfg(not(feature = "i915-debug-gem"))]
unsafe fn debug_active_fini(_ref_: *mut I915Active) {}
// upstream: i915_active.c debug_active_assert()
#[cfg(not(feature = "i915-debug-gem"))]
unsafe fn debug_active_assert(_ref_: *mut I915Active) {}

// upstream: i915_active.c __active_retire()
unsafe fn __active_retire(ref_: *mut I915Active) {
    let mut root = RbRoot {
        node: ptr::null_mut(),
    };
    let mut flags = 0;
    unsafe {
        assert!(atomic_read(&(*ref_).count) != 0);
        if !atomic_dec_and_lock_irqsave(&mut (*ref_).count, &mut (*ref_).tree_lock, &mut flags) {
            return;
        }
        assert!((*ref_).excl.fence.is_null());
        debug_active_deactivate(ref_);
        if (*ref_).cache.is_null() {
            (*ref_).cache = fetch_node((*ref_).tree.node);
        }
        if !(*ref_).cache.is_null() {
            let cache = (*ref_).cache;
            rb_erase(&mut (*cache).node, &mut (*ref_).tree);
            root = (*ref_).tree;
            rb_link_node(&mut (*cache).node, ptr::null_mut(), &mut (*ref_).tree.node);
            rb_insert_color(&mut (*cache).node, &mut (*ref_).tree);
            (*cache).timeline = 0;
        }
        spin_unlock_irqrestore(&mut (*ref_).tree_lock, flags);
        if let Some(retire) = (*ref_).retire {
            retire(ref_);
        }
        wake_up_var(ref_.cast());
        let mut it = rb_first_postorder(&mut root);
        while !it.is_null() {
            let next = rb_next_postorder(it);
            let node = fetch_node(it);
            assert!(!i915_active_fence_isset(&(*node).base));
            kmem_cache_free(SLAB_CACHE, node.cast());
            it = next;
        }
    }
}
// upstream: i915_active.c active_work()
unsafe extern "C" fn active_work(wrk: *mut WorkStruct) {
    let ref_ = unsafe {
        wrk.cast::<u8>()
            .sub(offset_of!(I915Active, work))
            .cast::<I915Active>()
    };
    unsafe {
        assert!(atomic_read(&(*ref_).count) != 0);
        if !atomic_add_unless(&mut (*ref_).count, -1, 1) {
            __active_retire(ref_);
        }
    }
}
// upstream: i915_active.c active_retire()
unsafe fn active_retire(ref_: *mut I915Active) {
    unsafe {
        assert!(atomic_read(&(*ref_).count) != 0);
        if atomic_add_unless(&mut (*ref_).count, -1, 1) {
            return;
        }
        if (*ref_).flags & I915_ACTIVE_RETIRE_SLEEPS != 0 {
            queue_work(system_dfl_wq, &mut (*ref_).work);
            return;
        }
        __active_retire(ref_);
    }
}
// upstream: i915_active.c __active_fence_slot()
#[inline]
unsafe fn __active_fence_slot(active: *mut I915ActiveFence) -> *mut *mut DmaFence {
    unsafe { &mut (*active).fence }
}
// upstream: i915_active.c active_fence_cb()
unsafe fn active_fence_cb(fence: *mut DmaFence, cb: *mut DmaFenceCb) -> bool {
    let active = unsafe {
        cb.cast::<u8>()
            .sub(offset_of!(I915ActiveFence, cb))
            .cast::<I915ActiveFence>()
    };
    unsafe { cmpxchg(__active_fence_slot(active), fence, ptr::null_mut()) == fence }
}
// upstream: i915_active.c node_retire()
unsafe extern "C" fn node_retire(fence: *mut DmaFence, cb: *mut DmaFenceCb) {
    if unsafe { active_fence_cb(fence, cb) } {
        let node = unsafe {
            cb.cast::<u8>()
                .sub(offset_of!(ActiveNode, base) + offset_of!(I915ActiveFence, cb))
                .cast::<ActiveNode>()
        };
        unsafe { active_retire((*node).ref_) };
    }
}
// upstream: i915_active.c excl_retire()
unsafe extern "C" fn excl_retire(fence: *mut DmaFence, cb: *mut DmaFenceCb) {
    if unsafe { active_fence_cb(fence, cb) } {
        let ref_ = unsafe {
            cb.cast::<u8>()
                .sub(offset_of!(I915Active, excl) + offset_of!(I915ActiveFence, cb))
                .cast::<I915Active>()
        };
        unsafe { active_retire(ref_) };
    }
}

// upstream: i915_active.c __active_lookup()
unsafe fn __active_lookup(ref_: *mut I915Active, idx: u64) -> *mut ActiveNode {
    unsafe {
        assert!(idx != 0);
        let cache = (*ref_).cache;
        if !cache.is_null() {
            let cached = ptr::read_volatile(&(*cache).timeline);
            if cached == idx {
                return cache;
            }
            if cached == 0 && cmpxchg64(&mut (*cache).timeline, 0, idx) == 0 {
                return cache;
            }
        }
        assert!(atomic_read(&(*ref_).count) != 0);
        let mut it = fetch_node((*ref_).tree.node);
        while !it.is_null() {
            if (*it).timeline < idx {
                it = fetch_node((*it).node.right);
            } else if (*it).timeline > idx {
                it = fetch_node((*it).node.left);
            } else {
                ptr::write_volatile(&mut (*ref_).cache, it);
                break;
            }
        }
        it
    }
}
// upstream: i915_active.c active_instance()
unsafe fn active_instance(ref_: *mut I915Active, idx: u64) -> *mut I915ActiveFence {
    unsafe {
        let mut node = __active_lookup(ref_, idx);
        if !node.is_null() {
            return &mut (*node).base;
        }
        spin_lock_irq(&mut (*ref_).tree_lock);
        assert!(atomic_read(&(*ref_).count) != 0);
        let mut parent = ptr::null_mut();
        let mut link = &mut (*ref_).tree.node as *mut *mut RbNode;
        while !(*link).is_null() {
            parent = *link;
            node = fetch_node(parent);
            if (*node).timeline == idx {
                ptr::write_volatile(&mut (*ref_).cache, node);
                spin_unlock_irq(&mut (*ref_).tree_lock);
                return &mut (*node).base;
            }
            link = if (*node).timeline < idx {
                &mut (*parent).right
            } else {
                &mut (*parent).left
            };
        }
        node = kmem_cache_zalloc::<ActiveNode>(SLAB_CACHE, GFP_ATOMIC);
        if node.is_null() {
            spin_unlock_irq(&mut (*ref_).tree_lock);
            return ptr::null_mut();
        }
        __i915_active_fence_init(&mut (*node).base, ptr::null_mut(), Some(node_retire));
        (*node).ref_ = ref_;
        (*node).timeline = idx;
        rb_link_node(&mut (*node).node, parent, link);
        rb_insert_color(&mut (*node).node, &mut (*ref_).tree);
        ptr::write_volatile(&mut (*ref_).cache, node);
        spin_unlock_irq(&mut (*ref_).tree_lock);
        &mut (*node).base
    }
}
// upstream: i915_active.c __i915_active_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_active_init(
    ref_: *mut I915Active,
    active: Option<unsafe extern "C" fn(*mut I915Active) -> i32>,
    retire: Option<unsafe extern "C" fn(*mut I915Active)>,
    flags: c_ulong,
    mkey: *mut c_void,
    wkey: *mut c_void,
) {
    unsafe {
        debug_active_init(ref_);
        (*ref_).flags = flags;
        (*ref_).active = active;
        (*ref_).retire = retire;
        spin_lock_init(&mut (*ref_).tree_lock);
        (*ref_).tree.node = ptr::null_mut();
        (*ref_).cache = ptr::null_mut();
        init_llist_head(&mut (*ref_).preallocated_barriers);
        atomic_set(&mut (*ref_).count, 0);
        __mutex_init(&mut (*ref_).mutex, b"i915_active\0".as_ptr(), mkey);
        __i915_active_fence_init(&mut (*ref_).excl, ptr::null_mut(), Some(excl_retire));
        INIT_WORK_C(&mut (*ref_).work, active_work);
        let _ = wkey;
    }
}
// upstream: i915_active.c ____active_del_barrier()
unsafe fn ____active_del_barrier(
    ref_: *mut I915Active,
    node: *mut ActiveNode,
    engine: *mut IntelEngineCs,
) -> bool {
    unsafe {
        assert_eq!(
            (*node).timeline,
            (*(*(*engine).kernel_context).timeline).fence_context
        );
        let mut head: *mut LlistNode = ptr::null_mut();
        let mut tail: *mut LlistNode = ptr::null_mut();
        let mut found = false;
        let mut pos = llist_del_all(&mut (*engine).barrier_tasks);
        while !pos.is_null() {
            let next = (*pos).next;
            if node == barrier_from_ll(pos) {
                found = true;
            } else {
                (*pos).next = head;
                head = pos;
                if tail.is_null() {
                    tail = pos;
                }
            }
            pos = next;
        }
        if !head.is_null() {
            llist_add_batch(head, tail, &mut (*engine).barrier_tasks);
        }
        let _ = ref_;
        found
    }
}
// upstream: i915_active.c __active_del_barrier()
unsafe fn __active_del_barrier(ref_: *mut I915Active, node: *mut ActiveNode) -> bool {
    unsafe { ____active_del_barrier(ref_, node, barrier_to_engine(node)) }
}
// upstream: i915_active.c replace_barrier()
unsafe fn replace_barrier(ref_: *mut I915Active, active: *mut I915ActiveFence) -> bool {
    unsafe {
        if !is_barrier(active) {
            return false;
        }
        __active_del_barrier(ref_, node_from_active(active))
    }
}
// upstream: i915_active.c i915_active_add_request()
pub unsafe fn i915_active_add_request(ref_: *mut I915Active, rq: *mut I915Request) -> c_int {
    unsafe {
        let idx = (*i915_request_timeline(rq)).fence_context;
        let fence = &mut (*rq).fence as *mut DmaFence;
        let err = i915_active_acquire(ref_);
        if err != 0 {
            return err;
        }
        let mut result = 0;
        loop {
            let active = active_instance(ref_, idx);
            if active.is_null() {
                result = -12;
                break;
            }
            if replace_barrier(ref_, active) {
                (*active).fence = ptr::null_mut();
                atomic_dec(&mut (*ref_).count);
            }
            if !is_barrier(active) {
                let old = __i915_active_fence_set(active, fence);
                if old.is_null() {
                    __i915_active_acquire(ref_);
                } else {
                    dma_fence_put(old);
                }
                break;
            }
        }
        i915_active_release(ref_);
        result
    }
}
// upstream: i915_active.c __i915_active_set_fence()
unsafe fn __i915_active_set_fence(
    ref_: *mut I915Active,
    active: *mut I915ActiveFence,
    fence: *mut DmaFence,
) -> *mut DmaFence {
    unsafe {
        if replace_barrier(ref_, active) {
            (*active).fence = fence;
            return ptr::null_mut();
        }
        let prev = __i915_active_fence_set(active, fence);
        if prev.is_null() {
            __i915_active_acquire(ref_);
        }
        prev
    }
}
// upstream: i915_active.c i915_active_set_exclusive()
pub unsafe fn i915_active_set_exclusive(ref_: *mut I915Active, f: *mut DmaFence) -> *mut DmaFence {
    unsafe { __i915_active_set_fence(ref_, &mut (*ref_).excl, f) }
}
// upstream: i915_active.c i915_active_acquire_if_busy()
pub unsafe fn i915_active_acquire_if_busy(ref_: *mut I915Active) -> bool {
    unsafe {
        debug_active_assert(ref_);
        atomic_add_unless(&mut (*ref_).count, 1, 0)
    }
}
// upstream: i915_active.c __i915_active_activate()
unsafe fn __i915_active_activate(ref_: *mut I915Active) {
    unsafe {
        spin_lock_irq(&mut (*ref_).tree_lock);
        if atomic_fetch_inc(&mut (*ref_).count) == 0 {
            debug_active_activate(ref_);
        }
        spin_unlock_irq(&mut (*ref_).tree_lock);
    }
}
// upstream: i915_active.c i915_active_acquire()
pub unsafe fn i915_active_acquire(ref_: *mut I915Active) -> c_int {
    unsafe {
        if i915_active_acquire_if_busy(ref_) {
            return 0;
        }
        if (*ref_).active.is_none() {
            __i915_active_activate(ref_);
            return 0;
        }
        let err = mutex_lock_interruptible(&mut (*ref_).mutex);
        if err != 0 {
            return err;
        }
        let mut result = 0;
        if !i915_active_acquire_if_busy(ref_) {
            if let Some(active) = (*ref_).active {
                result = active(ref_);
                if result == 0 {
                    __i915_active_activate(ref_);
                }
            }
        }
        mutex_unlock(&mut (*ref_).mutex);
        result
    }
}
// upstream: i915_active.c i915_active_release()
pub unsafe fn i915_active_release(ref_: *mut I915Active) {
    unsafe {
        debug_active_assert(ref_);
        active_retire(ref_);
    }
}

// upstream: i915_active.c enable_signaling()
unsafe fn enable_signaling(active: *mut I915ActiveFence) {
    unsafe {
        if is_barrier(active) {
            return;
        }
        let fence = i915_active_fence_get(active);
        if fence.is_null() {
            return;
        }
        dma_fence_enable_sw_signaling(fence);
        dma_fence_put(fence);
    }
}
// upstream: i915_active.c flush_barrier()
unsafe fn flush_barrier(it: *mut ActiveNode) -> c_int {
    unsafe {
        if !is_barrier(&(*it).base) {
            return 0;
        }
        let engine = barrier_to_engine(it);
        smp_rmb();
        if !is_barrier(&(*it).base) {
            return 0;
        }
        intel_engine_flush_barriers(engine)
    }
}
// upstream: i915_active.c flush_lazy_signals()
unsafe fn flush_lazy_signals(ref_: *mut I915Active) -> c_int {
    unsafe {
        enable_signaling(&mut (*ref_).excl);
        let mut rb = rb_first_postorder(&mut (*ref_).tree);
        while !rb.is_null() {
            let node = fetch_node(rb);
            let err = flush_barrier(node);
            if err != 0 {
                return err;
            }
            enable_signaling(&mut (*node).base);
            rb = rb_next_postorder(rb);
        }
        0
    }
}
// upstream: i915_active.c __i915_active_wait()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_active_wait(ref_: *mut I915Active, state: c_int) -> c_int {
    unsafe {
        if i915_active_acquire_if_busy(ref_) {
            let err = flush_lazy_signals(ref_);
            i915_active_release(ref_);
            if err != 0 {
                return err;
            }
            if linux_wait_var_event_interruptible(ref_.cast(), state) != 0 {
                return -4;
            }
        }
        flush_work(&mut (*ref_).work);
        0
    }
}
// upstream: i915_active.c __await_active()
unsafe fn __await_active(
    active: *mut I915ActiveFence,
    fn_: unsafe extern "C" fn(*mut c_void, *mut DmaFence) -> c_int,
    arg: *mut c_void,
) -> c_int {
    unsafe {
        if is_barrier(active) {
            return 0;
        }
        let fence = i915_active_fence_get(active);
        if fence.is_null() {
            return 0;
        }
        let err = fn_(arg, fence);
        dma_fence_put(fence);
        if err < 0 { err } else { 0 }
    }
}
#[repr(C)]
struct WaitBarrier {
    base: WaitQueueEntry,
    ref_: *mut I915Active,
}
// upstream: i915_active.c barrier_wake()
unsafe extern "C" fn barrier_wake(
    wq: *mut WaitQueueEntry,
    _mode: u32,
    _flags: c_int,
    _key: *mut c_void,
) -> c_int {
    unsafe {
        let wb = wq
            .cast::<u8>()
            .sub(offset_of!(WaitBarrier, base))
            .cast::<WaitBarrier>();
        if crate::linux::requests::i915_active_is_idle((*wb).ref_) {
            list_del(&mut (*wq).entry);
            i915_sw_fence_complete((*wq).private.cast());
            kfree(wq.cast());
        }
        0
    }
}
// upstream: i915_active.c __await_barrier()
unsafe fn __await_barrier(ref_: *mut I915Active, fence: *mut I915SwFence) -> c_int {
    unsafe {
        let wb = kmalloc(core::mem::size_of::<WaitBarrier>(), GFP_KERNEL).cast::<WaitBarrier>();
        if wb.is_null() {
            return -12;
        }
        assert!(!crate::linux::requests::i915_active_is_idle(ref_));
        if !i915_sw_fence_await(fence) {
            kfree(wb.cast());
            return -22;
        }
        (*wb).base.flags = 0;
        (*wb).base.func = Some(barrier_wake);
        (*wb).base.private = fence.cast();
        (*wb).ref_ = ref_;
        add_wait_queue(__var_waitqueue(ref_.cast()), &mut (*wb).base);
        0
    }
}
// upstream: i915_active.c await_active()
unsafe fn await_active(
    ref_: *mut I915Active,
    flags: u32,
    fn_: unsafe extern "C" fn(*mut c_void, *mut DmaFence) -> c_int,
    arg: *mut c_void,
    barrier: *mut I915SwFence,
) -> c_int {
    unsafe {
        if !i915_active_acquire_if_busy(ref_) {
            return 0;
        }
        let mut err = 0;
        if flags & I915_ACTIVE_AWAIT_EXCL != 0 && !(*ref_).excl.fence.is_null() {
            err = __await_active(&mut (*ref_).excl, fn_, arg);
            if err != 0 {
                i915_active_release(ref_);
                return err;
            }
        }
        if flags & I915_ACTIVE_AWAIT_ACTIVE != 0 {
            let mut rb = rb_first_postorder(&mut (*ref_).tree);
            while !rb.is_null() {
                let n = rb_next_postorder(rb);
                let node = fetch_node(rb);
                err = __await_active(&mut (*node).base, fn_, arg);
                if err != 0 {
                    i915_active_release(ref_);
                    return err;
                }
                rb = n;
            }
        }
        if flags & I915_ACTIVE_AWAIT_BARRIER != 0 {
            err = flush_lazy_signals(ref_);
            if err == 0 {
                err = __await_barrier(ref_, barrier);
            }
        }
        i915_active_release(ref_);
        err
    }
}
// upstream: i915_active.c rq_await_fence()
unsafe extern "C" fn rq_await_fence(arg: *mut c_void, fence: *mut DmaFence) -> c_int {
    unsafe { i915_request_await_dma_fence(arg.cast(), fence) }
}
// upstream: i915_active.c i915_request_await_active()
pub unsafe fn i915_request_await_active(
    rq: *mut I915Request,
    ref_: *mut I915Active,
    flags: u32,
) -> c_int {
    unsafe { await_active(ref_, flags, rq_await_fence, rq.cast(), &mut (*rq).submit) }
}
// upstream: i915_active.c sw_await_fence()
unsafe extern "C" fn sw_await_fence(arg: *mut c_void, fence: *mut DmaFence) -> c_int {
    unsafe { i915_sw_fence_await_dma_fence(arg.cast(), fence, 0, GFP_NOWAIT | __GFP_NOWARN) }
}
// upstream: i915_active.c i915_sw_fence_await_active()
pub unsafe fn i915_sw_fence_await_active(
    fence: *mut I915SwFence,
    ref_: *mut I915Active,
    flags: u32,
) -> c_int {
    unsafe { await_active(ref_, flags, sw_await_fence, fence.cast(), fence) }
}
// upstream: i915_active.c i915_active_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_active_fini(ref_: *mut I915Active) {
    unsafe {
        debug_active_fini(ref_);
        assert_eq!(atomic_read(&(*ref_).count), 0);
        assert!(!work_pending(&(*ref_).work));
        mutex_destroy(&mut (*ref_).mutex);
        if !(*ref_).cache.is_null() {
            kmem_cache_free(SLAB_CACHE, (*ref_).cache.cast());
        }
    }
}

// upstream: i915_active.c is_idle_barrier()
unsafe fn is_idle_barrier(node: *mut ActiveNode, idx: u64) -> bool {
    unsafe { (*node).timeline == idx && !i915_active_fence_isset(&(*node).base) }
}
// upstream: i915_active.c reuse_idle_barrier()
unsafe fn reuse_idle_barrier(ref_: *mut I915Active, idx: u64) -> *mut ActiveNode {
    unsafe {
        if (*ref_).tree.node.is_null() {
            return ptr::null_mut();
        }
        assert!(atomic_read(&(*ref_).count) != 0);
        let mut p = (*ref_).tree.node;
        let mut prev = ptr::null_mut();
        while !p.is_null() {
            let node = fetch_node(p);
            if is_idle_barrier(node, idx) {
                break;
            }
            prev = p;
            p = if (*node).timeline < idx {
                (*p).right
            } else {
                (*p).left
            };
        }
        if p.is_null() {
            p = prev;
            while !p.is_null() {
                let node = fetch_node(p);
                if (*node).timeline > idx {
                    break;
                }
                if (*node).timeline == idx {
                    if is_idle_barrier(node, idx) {
                        break;
                    }
                    let engine = barrier_to_engine(node);
                    smp_rmb();
                    if is_barrier(&(*node).base) && ____active_del_barrier(ref_, node, engine) {
                        break;
                    }
                }
                p = rb_next(p);
            }
        }
        if p.is_null() {
            return ptr::null_mut();
        }
        spin_lock_irq(&mut (*ref_).tree_lock);
        rb_erase(p, &mut (*ref_).tree);
        if !(*ref_).cache.is_null() && p == &mut (*(*ref_).cache).node {
            ptr::write_volatile(&mut (*ref_).cache, ptr::null_mut());
        }
        spin_unlock_irq(&mut (*ref_).tree_lock);
        fetch_node(p)
    }
}
// upstream: i915_active.c i915_active_acquire_preallocate_barrier()
pub unsafe fn i915_active_acquire_preallocate_barrier(
    ref_: *mut I915Active,
    engine: *mut IntelEngineCs,
) -> c_int {
    unsafe {
        assert!(atomic_read(&(*ref_).count) != 0);
        while !llist_empty(&(*ref_).preallocated_barriers) {
            cond_resched();
        }
        let mut first: *mut LlistNode = ptr::null_mut();
        let mut last: *mut LlistNode = ptr::null_mut();
        let mask = (*engine).mask;
        let gt = (*engine).gt;
        let mut iter = 0;
        while iter < crate::intel_engine_types_upstream::I915_NUM_ENGINES as u32 {
            let e = (*gt).engine[iter as usize];
            if !e.is_null() && (*e).mask & mask != 0 {
                let idx = (*(*(*e).kernel_context).timeline).fence_context;
                let mut node = reuse_idle_barrier(ref_, idx);
                if node.is_null() {
                    node = kmem_cache_zalloc::<ActiveNode>(SLAB_CACHE, GFP_KERNEL);
                    if node.is_null() {
                        let mut pos = first;
                        while !pos.is_null() {
                            let n = (*pos).next;
                            let old = barrier_from_ll(pos);
                            atomic_dec(&mut (*ref_).count);
                            intel_engine_pm_put(barrier_to_engine(old));
                            kmem_cache_free(SLAB_CACHE, old.cast());
                            pos = n;
                        }
                        return -12;
                    }
                    (*node).base.fence = ptr::null_mut();
                    (*node).base.cb.func = Some(node_retire);
                    (*node).timeline = idx;
                    (*node).ref_ = ref_;
                }
                if !i915_active_fence_isset(&(*node).base) {
                    (*node).base.fence = (-11isize) as *mut DmaFence;
                    (*node).base.cb.node.prev = e.cast();
                    __i915_active_acquire(ref_);
                }
                let ll = barrier_to_ll(node);
                (*ll).next = first;
                first = ll;
                if last.is_null() {
                    last = ll;
                }
                intel_engine_pm_get(e);
            }
            iter += 1;
        }
        if !first.is_null() {
            llist_add_batch(first, last, &mut (*ref_).preallocated_barriers);
        }
        0
    }
}

// upstream: i915_active.c i915_active_acquire_barrier()
pub unsafe fn i915_active_acquire_barrier(ref_: *mut I915Active) {
    unsafe {
        assert!(atomic_read(&(*ref_).count) != 0);
        let mut pos = llist_del_all(&mut (*ref_).preallocated_barriers);
        while !pos.is_null() {
            let next = (*pos).next;
            let node = barrier_from_ll(pos);
            let engine = barrier_to_engine(node);
            let mut flags = 0;
            spin_lock_irqsave_nested(&mut (*ref_).tree_lock, &mut flags, 1);
            let mut parent = ptr::null_mut();
            let mut link = &mut (*ref_).tree.node as *mut *mut RbNode;
            while !(*link).is_null() {
                parent = *link;
                let it = fetch_node(parent);
                link = if (*it).timeline < (*node).timeline {
                    &mut (*parent).right
                } else {
                    &mut (*parent).left
                };
            }
            rb_link_node(&mut (*node).node, parent, link);
            rb_insert_color(&mut (*node).node, &mut (*ref_).tree);
            spin_unlock_irqrestore(&mut (*ref_).tree_lock, flags);
            assert!(intel_engine_pm_is_awake(engine));
            llist_add(barrier_to_ll(node), &mut (*engine).barrier_tasks);
            intel_engine_pm_put_delay(engine, 2);
            pos = next;
        }
    }
}
// upstream: i915_active.c ll_to_fence_slot()
unsafe fn ll_to_fence_slot(node: *mut LlistNode) -> *mut *mut DmaFence {
    unsafe { __active_fence_slot(&mut (*barrier_from_ll(node)).base) }
}
// upstream: i915_active.c i915_request_add_active_barriers()
pub unsafe fn i915_request_add_active_barriers(rq: *mut I915Request) {
    unsafe {
        let engine = (*rq).engine;
        assert!(intel_context_is_barrier((*rq).context));
        assert!(!intel_engine_is_virtual(engine));
        assert_eq!(
            i915_request_timeline(rq),
            (*engine).kernel_context.as_mut().unwrap().timeline
        );
        let mut node = llist_del_all(&mut (*engine).barrier_tasks);
        if node.is_null() {
            return;
        }
        let mut flags = 0;
        spin_lock_irqsave(&mut (*rq).lock, &mut flags);
        while !node.is_null() {
            let next = (*node).next;
            ptr::write_volatile(ll_to_fence_slot(node), &mut (*rq).fence);
            list_add_tail(
                node.cast(),
                core::ptr::addr_of_mut!((*rq).fence.timestamp_union.cb_list).cast::<ListHead>(),
            );
            node = next;
        }
        spin_unlock_irqrestore(&mut (*rq).lock, flags);
    }
}
// upstream: i915_active.c __i915_active_fence_set()
pub unsafe fn __i915_active_fence_set(
    active: *mut I915ActiveFence,
    fence: *mut DmaFence,
) -> *mut DmaFence {
    unsafe {
        let mut prev = i915_active_fence_get(active);
        if fence == prev {
            return fence;
        }
        assert!((*fence).flags & (1 << 3) == 0);
        let mut flags = 0;
        dma_fence_lock_irqsave(fence, &mut flags);
        if !prev.is_null() {
            spin_lock_nested(dma_fence_spinlock(prev), 1);
        }
        loop {
            let old = cmpxchg(__active_fence_slot(active), prev, fence);
            if old == prev {
                break;
            }
            if !prev.is_null() {
                spin_unlock(dma_fence_spinlock(prev));
                dma_fence_put(prev);
            }
            dma_fence_unlock_irqrestore(fence, flags);
            prev = i915_active_fence_get(active);
            assert_ne!(prev, fence);
            dma_fence_lock_irqsave(fence, &mut flags);
            if !prev.is_null() {
                spin_lock_nested(dma_fence_spinlock(prev), 1);
            }
        }
        if !prev.is_null() {
            __list_del_entry(&mut (*active).cb.node);
            spin_unlock(dma_fence_spinlock(prev));
        }
        list_add_tail(
            core::ptr::addr_of_mut!((*active).cb.node).cast::<ListHead>(),
            core::ptr::addr_of_mut!((*fence).timestamp_union.cb_list).cast::<ListHead>(),
        );
        dma_fence_unlock_irqrestore(fence, flags);
        prev
    }
}
// upstream: i915_active.c i915_active_fence_set()
pub unsafe fn i915_active_fence_set(active: *mut I915ActiveFence, rq: *mut I915Request) -> c_int {
    unsafe {
        let fence = __i915_active_fence_set(active, &mut (*rq).fence);
        if fence.is_null() {
            0
        } else {
            let err = i915_request_await_dma_fence(rq, fence);
            dma_fence_put(fence);
            err
        }
    }
}
// upstream: i915_active.c i915_active_noop()
pub unsafe extern "C" fn i915_active_noop(fence: *mut DmaFence, cb: *mut DmaFenceCb) {
    let _ = unsafe { active_fence_cb(fence, cb) };
}
#[repr(C)]
struct AutoActive {
    base: I915Active,
    ref_: Kref,
}
// upstream: i915_active.c i915_active_get()
pub unsafe fn i915_active_get(ref_: *mut I915Active) -> *mut I915Active {
    unsafe {
        let aa = ref_
            .cast::<u8>()
            .sub(offset_of!(AutoActive, base))
            .cast::<AutoActive>();
        kref_get(&mut (*aa).ref_);
        &mut (*aa).base
    }
}
// upstream: i915_active.c auto_release()
unsafe extern "C" fn auto_release(ref_: *mut Kref) {
    unsafe {
        let aa = ref_
            .cast::<u8>()
            .sub(offset_of!(AutoActive, ref_))
            .cast::<AutoActive>();
        i915_active_fini(&mut (*aa).base);
        kfree(aa.cast());
    }
}
// upstream: i915_active.c i915_active_put()
pub unsafe fn i915_active_put(ref_: *mut I915Active) {
    unsafe {
        let aa = ref_
            .cast::<u8>()
            .sub(offset_of!(AutoActive, base))
            .cast::<AutoActive>();
        let _ = kref_put(&mut (*aa).ref_, Some(auto_release));
    }
}
// upstream: i915_active.c auto_active()
unsafe extern "C" fn auto_active(ref_: *mut I915Active) -> c_int {
    unsafe {
        i915_active_get(ref_);
        0
    }
}
// upstream: i915_active.c auto_retire()
unsafe extern "C" fn auto_retire(ref_: *mut I915Active) {
    unsafe { i915_active_put(ref_) }
}
// upstream: i915_active.c i915_active_create()
pub unsafe fn i915_active_create() -> *mut I915Active {
    unsafe {
        let aa = kmalloc(core::mem::size_of::<AutoActive>(), GFP_KERNEL).cast::<AutoActive>();
        if aa.is_null() {
            return ptr::null_mut();
        }
        kref_init(&mut (*aa).ref_);
        __i915_active_init(
            &mut (*aa).base,
            Some(auto_active),
            Some(auto_retire),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        );
        &mut (*aa).base
    }
}
// upstream: i915_active.c i915_active_module_exit()
pub unsafe fn i915_active_module_exit() {
    unsafe { kmem_cache_destroy(SLAB_CACHE) }
}
// upstream: i915_active.c i915_active_module_init()
pub unsafe fn i915_active_module_init() -> c_int {
    unsafe {
        SLAB_CACHE = kmem_cache_create::<ActiveNode>(SLAB_HWCACHE_ALIGN);
        if SLAB_CACHE.is_null() { -12 } else { 0 }
    }
}
