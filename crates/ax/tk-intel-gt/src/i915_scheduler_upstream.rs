// SPDX-License-Identifier: MIT
// Copyright © 2018 Intel Corporation.
// Additive Rust translation of Linux v7.2.3 drivers/gpu/drm/i915/i915_scheduler.c.
//
// The field-bearing scheduler/request records come from the canonical upstream
// type modules. Framework-specific allocation, IRQ/spinlock, and RCU operations
// are routed through this crate's LinuxKPI adapters; noted limitations below
// are not replaced with successful no-op implementations.

#![allow(non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::offset_of,
    ptr,
};

use crate::{
    i915_request_types_upstream::I915Request,
    i915_scheduler_types_upstream::{
        I915_DEPENDENCY_ALLOC, I915Dependency, I915Priolist, I915SchedAttr, I915SchedEngine,
        I915SchedNode,
    },
    intel_context_upstream::Kref,
    intel_engine_cs_upstream::{RbNode, RbRoot, RbRootCached, Spinlock},
    linux::{
        bits::*, fields::DrmPrinter, locks::*, memory::*, rbtree::*, rcu::*, registers::*,
        requests::*, tasklet::*,
    },
    linux_config::{GFP_ATOMIC, GFP_KERNEL, *},
    linux_heap::{
        KmCache, SLAB_HWCACHE_ALIGN, kmem_cache_create, kmem_cache_destroy, kmem_cache_free,
        kmem_cache_zalloc,
    },
    linux_list::*,
};

static mut SLAB_DEPENDENCIES: *mut KmCache = ptr::null_mut();
static mut SLAB_PRIORITIES: *mut KmCache = ptr::null_mut();
// DEFINE_SPINLOCK() is a zero-initialized Linux raw spinlock in this non-RT
// target configuration. The LinuxKPI lock implementation treats zero as free.
static mut SCHEDULE_LOCK: Spinlock = unsafe { core::mem::zeroed() };

/// `rb_first()` is an inline Linux rbtree helper; it is kept local rather than
/// incorrectly declaring a C symbol for an inline function.
#[inline]
unsafe fn rb_first(root: *const RbRoot) -> *mut RbNode {
    let mut node = unsafe { (*root).node };
    if node.is_null() {
        return node;
    }
    while !unsafe { (*node).left }.is_null() {
        node = unsafe { (*node).left };
    }
    node
}

/// `rb_link_node()` is the Linux inline insertion primitive.
#[inline]
unsafe fn rb_link_node(node: *mut RbNode, parent: *mut RbNode, link: *mut *mut RbNode) {
    unsafe {
        (*node).parent_color = parent as usize;
        (*node).left = ptr::null_mut();
        (*node).right = ptr::null_mut();
        *link = node;
    }
}

unsafe extern "C" {
    // Out-of-line lib/rbtree.c balancing operation used by the inline cached
    // insertion helper.
    fn rb_insert_color(node: *mut RbNode, root: *mut RbRoot);
}

#[inline]
unsafe fn request_from_node(node: *const I915SchedNode) -> *const I915Request {
    unsafe {
        node.cast::<u8>()
            .sub(offset_of!(I915Request, sched))
            .cast::<I915Request>()
    }
}

#[inline]
unsafe fn priolist_from_rb(rb: *mut RbNode) -> *mut I915Priolist {
    unsafe {
        rb.cast::<u8>()
            .sub(offset_of!(I915Priolist, node))
            .cast::<I915Priolist>()
    }
}

#[inline]
unsafe fn dependency_from_link(
    link: *mut crate::intel_engine_cs_upstream::ListHead,
    offset: usize,
) -> *mut I915Dependency {
    unsafe { link.cast::<u8>().sub(offset).cast::<I915Dependency>() }
}

// upstream: i915_scheduler.c node_to_request()
unsafe fn node_to_request(node: *const I915SchedNode) -> *const I915Request {
    unsafe { request_from_node(node) }
}

// upstream: i915_scheduler.c node_started()
unsafe fn node_started(node: *const I915SchedNode) -> bool {
    i915_request_started(unsafe { &*node_to_request(node) })
}

// upstream: i915_scheduler.c node_signaled()
unsafe fn node_signaled(node: *const I915SchedNode) -> bool {
    i915_request_completed(unsafe { &*node_to_request(node) })
}

// upstream: i915_scheduler.c to_priolist()
unsafe fn to_priolist(rb: *mut RbNode) -> *mut I915Priolist {
    unsafe { priolist_from_rb(rb) }
}

// upstream: i915_scheduler.c assert_priolists()
unsafe fn assert_priolists(sched_engine: *const I915SchedEngine) {
    if !crate::linux_config::CONFIG_DRM_I915_DEBUG_GEM {
        return;
    }

    let queue = unsafe { &(*sched_engine).queue };
    GEM_BUG_ON!(rb_first_cached(queue) != unsafe { rb_first(ptr::addr_of!(queue.root)) });

    let mut last_prio = i32::MAX;
    let mut rb = unsafe { rb_first(ptr::addr_of!(queue.root)) };
    while !rb.is_null() {
        let priolist = unsafe { to_priolist(rb) };
        GEM_BUG_ON!(unsafe { (*priolist).priority > last_prio });
        last_prio = unsafe { (*priolist).priority };
        rb = unsafe { rb_next(rb) };
    }
}

// upstream: i915_scheduler.c i915_sched_lookup_priolist()
pub unsafe fn i915_sched_lookup_priolist(
    sched_engine: *mut I915SchedEngine,
    mut prio: i32,
) -> *mut crate::intel_engine_cs_upstream::ListHead {
    let mut first = true;
    lockdep_assert_held!(unsafe { &(*sched_engine).lock });
    unsafe { assert_priolists(sched_engine) };

    if unsafe { (*sched_engine).no_priolist } {
        prio = I915_PRIORITY_NORMAL;
    }

    'find_priolist: loop {
        let mut rb = ptr::null_mut();
        let mut parent = unsafe { ptr::addr_of_mut!((*sched_engine).queue.root.node) };
        while !unsafe { (*parent).is_null() } {
            rb = unsafe { *parent };
            let priolist = unsafe { to_priolist(rb) };
            let current = unsafe { (*priolist).priority };
            if prio > current {
                parent = unsafe { ptr::addr_of_mut!((*rb).left) };
            } else if prio < current {
                parent = unsafe { ptr::addr_of_mut!((*rb).right) };
                first = false;
            } else {
                return unsafe { ptr::addr_of_mut!((*priolist).requests) };
            }
        }

        let priolist = if prio == I915_PRIORITY_NORMAL {
            unsafe { ptr::addr_of_mut!((*sched_engine).default_priolist) }
        } else {
            let allocated =
                unsafe { kmem_cache_zalloc::<I915Priolist>(SLAB_PRIORITIES, GFP_ATOMIC) };
            if allocated.is_null() {
                prio = I915_PRIORITY_NORMAL;
                // Allocation failure takes upstream's FIFO fallback. The
                // current LinuxKPI allocator deliberately rejects GFP_ATOMIC,
                // so this boundary can force the fallback until a nonblocking
                // slab allocator is supplied.
                unsafe { (*sched_engine).no_priolist = true };
                continue 'find_priolist;
            }
            allocated
        };

        unsafe {
            (*priolist).priority = prio;
            INIT_LIST_HEAD!(ptr::addr_of_mut!((*priolist).requests));
            rb_link_node(ptr::addr_of_mut!((*priolist).node), rb, parent);
            if first {
                (*sched_engine).queue.leftmost = ptr::addr_of_mut!((*priolist).node);
            }
            rb_insert_color(
                ptr::addr_of_mut!((*priolist).node),
                ptr::addr_of_mut!((*sched_engine).queue.root),
            );
            return ptr::addr_of_mut!((*priolist).requests);
        }
    }
}

// upstream: i915_scheduler.c __i915_priolist_free()
pub unsafe fn __i915_priolist_free(priolist: *mut I915Priolist) {
    unsafe { kmem_cache_free(SLAB_PRIORITIES, priolist.cast::<c_void>()) };
}

#[repr(C)]
struct SchedCache {
    priolist: *mut crate::intel_engine_cs_upstream::ListHead,
}

// upstream: i915_scheduler.c lock_sched_engine()
unsafe fn lock_sched_engine(
    node: *mut I915SchedNode,
    mut locked: *mut I915SchedEngine,
    cache: *mut SchedCache,
) -> *mut I915SchedEngine {
    GEM_BUG_ON!(locked.is_null());
    let request = unsafe { node_to_request(node) };

    // A virtual-engine request may change engines while locks are transferred;
    // lock the currently published engine and recheck the owner under lock.
    loop {
        let engine = unsafe { READ_ONCE!((*request).engine) };
        let sched_engine = unsafe { (*engine).sched_engine };
        if locked == sched_engine {
            break;
        }
        unsafe {
            spin_unlock_raw(ptr::addr_of_mut!((*locked).lock));
            *cache = SchedCache {
                priolist: ptr::null_mut(),
            };
            spin_lock_raw(ptr::addr_of_mut!((*sched_engine).lock));
        }
        locked = sched_engine;
    }

    GEM_BUG_ON!(locked != unsafe { (*READ_ONCE!((*request).engine)).sched_engine });
    locked
}

// upstream: i915_scheduler.c __i915_schedule()
unsafe fn __i915_schedule(node: *mut I915SchedNode, attr: *const I915SchedAttr) {
    let prio = unsafe { (*attr).priority.max((*node).attr.priority) };
    let mut sched_engine: *mut I915SchedEngine;
    let mut cache = SchedCache {
        priolist: ptr::null_mut(),
    };
    let mut stack = unsafe { core::mem::zeroed::<I915Dependency>() };
    let mut dfs = unsafe { core::mem::zeroed::<crate::intel_engine_cs_upstream::ListHead>() };
    unsafe { INIT_LIST_HEAD!(ptr::addr_of_mut!(dfs)) };

    lockdep_assert_held!(unsafe { &*ptr::addr_of!(SCHEDULE_LOCK) });
    GEM_BUG_ON!(prio == I915_PRIORITY_INVALID);

    if unsafe { node_signaled(node) } {
        return;
    }

    // The embedded field is a raw pointer, so initialize the stack record
    // explicitly before publishing its DFS link.
    unsafe {
        stack.signaler = node;
        (*ptr::addr_of_mut!(stack.dfs_link)).next = ptr::null_mut();
        (*ptr::addr_of_mut!(stack.dfs_link)).prev = ptr::null_mut();
        list_add(ptr::addr_of_mut!(stack.dfs_link), ptr::addr_of_mut!(dfs));
    }

    // Flatten the dependency graph, moving each priority-inheriting signaler
    // to the tail so the reverse walk below visits dependencies first.
    let dfs_offset = offset_of!(I915Dependency, dfs_link);
    let signal_offset = offset_of!(I915Dependency, signal_link);
    let mut cursor = unsafe { (*ptr::addr_of_mut!(dfs)).next };
    while !ptr::eq(cursor, ptr::addr_of_mut!(dfs)) {
        let dep = unsafe { dependency_from_link(cursor, dfs_offset) };
        let signaler = unsafe { (*dep).signaler };
        if !unsafe { node_started(signaler) } {
            let head = unsafe { ptr::addr_of_mut!((*signaler).signalers_list) };
            let mut signal_link = unsafe { (*head).next };
            while !ptr::eq(signal_link, head) {
                let p = unsafe { dependency_from_link(signal_link, signal_offset) };
                let next = unsafe { (*signal_link).next };
                GEM_BUG_ON!(p == dep);
                let p_signaler = unsafe { (*p).signaler };
                if !unsafe { node_signaled(p_signaler) }
                    && prio > unsafe { READ_ONCE!((*p_signaler).attr.priority) }
                {
                    unsafe {
                        list_move_tail(ptr::addr_of_mut!((*p).dfs_link), ptr::addr_of_mut!(dfs))
                    };
                }
                signal_link = next;
            }
        }
        cursor = unsafe { (*dep).dfs_link.next };
    }

    // For a new request with no dependency chain, update only its own attr.
    if unsafe { (*node).attr.priority == I915_PRIORITY_INVALID } {
        GEM_BUG_ON!(!list_empty(unsafe { &(*node).link }));
        unsafe { (*node).attr = *attr };
        if unsafe { stack.dfs_link.next == stack.dfs_link.prev } {
            return;
        }
        unsafe { list_del(ptr::addr_of_mut!(stack.dfs_link)) };
    }

    let request = unsafe { node_to_request(node) };
    sched_engine = unsafe { (*(*request).engine).sched_engine };
    unsafe { spin_lock_raw(ptr::addr_of_mut!((*sched_engine).lock)) };
    sched_engine = unsafe { lock_sched_engine(node, sched_engine, ptr::addr_of_mut!(cache)) };

    let mut cursor = unsafe { (*ptr::addr_of_mut!(dfs)).prev };
    while !ptr::eq(cursor, ptr::addr_of_mut!(dfs)) {
        let dep = unsafe { dependency_from_link(cursor, dfs_offset) };
        let previous = unsafe { (*cursor).prev };
        let from = unsafe { node_to_request((*dep).signaler) }.cast_mut();

        unsafe { INIT_LIST_HEAD!(ptr::addr_of_mut!((*dep).dfs_link)) };
        node = unsafe { (*dep).signaler };
        sched_engine = unsafe { lock_sched_engine(node, sched_engine, ptr::addr_of_mut!(cache)) };

        if prio <= unsafe { (*node).attr.priority } || unsafe { node_signaled(node) } {
            cursor = previous;
            continue;
        }

        GEM_BUG_ON!(unsafe { (*(*node_to_request(node)).engine).sched_engine != sched_engine });
        if let Some(bump) = unsafe { (*sched_engine).bump_inflight_request_prio } {
            unsafe { bump(from, prio) };
        }
        unsafe { WRITE_ONCE!((*node).attr.priority, prio) };

        if list_empty(unsafe { &(*node).link }) {
            cursor = previous;
            continue;
        }
        if i915_request_in_priority_queue(unsafe { &*node_to_request(node) }) {
            if cache.priolist.is_null() {
                cache.priolist = unsafe { i915_sched_lookup_priolist(sched_engine, prio) };
            }
            unsafe { list_move_tail(ptr::addr_of_mut!((*node).link), cache.priolist) };
        }
        if let Some(kick) = unsafe { (*sched_engine).kick_backend } {
            unsafe { kick(node_to_request(node), prio) };
        }
        cursor = previous;
    }

    unsafe { spin_unlock_raw(ptr::addr_of_mut!((*sched_engine).lock)) };
}

// upstream: i915_scheduler.c i915_schedule()
pub unsafe fn i915_schedule(request: *mut I915Request, attr: *const I915SchedAttr) {
    unsafe {
        spin_lock_irq_raw(ptr::addr_of_mut!(SCHEDULE_LOCK));
        __i915_schedule(ptr::addr_of_mut!((*request).sched), attr);
        spin_unlock_irq_raw(ptr::addr_of_mut!(SCHEDULE_LOCK));
    }
}

// upstream: i915_scheduler.c i915_sched_node_init()
pub unsafe fn i915_sched_node_init(node: *mut I915SchedNode) {
    unsafe {
        INIT_LIST_HEAD!(ptr::addr_of_mut!((*node).signalers_list));
        INIT_LIST_HEAD!(ptr::addr_of_mut!((*node).waiters_list));
        INIT_LIST_HEAD!(ptr::addr_of_mut!((*node).link));
        i915_sched_node_reinit(node);
    }
}

// upstream: i915_scheduler.c i915_sched_node_reinit()
pub unsafe fn i915_sched_node_reinit(node: *mut I915SchedNode) {
    unsafe {
        (*node).attr.priority = I915_PRIORITY_INVALID;
        (*node).semaphores = 0;
        (*node).flags = 0;
    }
    GEM_BUG_ON!(!list_empty(unsafe { &(*node).signalers_list }));
    GEM_BUG_ON!(!list_empty(unsafe { &(*node).waiters_list }));
    GEM_BUG_ON!(!list_empty(unsafe { &(*node).link }));
}

// upstream: i915_scheduler.c i915_dependency_alloc()
unsafe fn i915_dependency_alloc() -> *mut I915Dependency {
    // LinuxKPI currently exposes zeroing cache allocation only; the source
    // writes all dependency members before publication, so zero-init preserves
    // the observable initialized state.
    unsafe { kmem_cache_zalloc::<I915Dependency>(SLAB_DEPENDENCIES, GFP_KERNEL) }
}

// upstream: i915_scheduler.c i915_dependency_free()
unsafe fn i915_dependency_free(dependency: *mut I915Dependency) {
    // The source cache is SLAB_TYPESAFE_BY_RCU. The current LinuxKPI heap has no
    // RCU-delayed cache-reuse mode; this boundary must be supplied before RCU
    // walkers and dependency reclamation are considered concurrency-equivalent.
    unsafe { kmem_cache_free(SLAB_DEPENDENCIES, dependency.cast::<c_void>()) };
}

// upstream: i915_scheduler.c __i915_sched_node_add_dependency()
pub unsafe fn __i915_sched_node_add_dependency(
    node: *mut I915SchedNode,
    signal: *mut I915SchedNode,
    dependency: *mut I915Dependency,
    flags: c_ulong,
) -> bool {
    let mut ret = false;
    unsafe { spin_lock_irq_raw(ptr::addr_of_mut!(SCHEDULE_LOCK)) };

    if !unsafe { node_signaled(signal) } {
        unsafe {
            INIT_LIST_HEAD!(ptr::addr_of_mut!((*dependency).dfs_link));
            (*dependency).signaler = signal;
            (*dependency).waiter = node;
            (*dependency).flags = flags;
            list_add_rcu(
                ptr::addr_of_mut!((*dependency).signal_link),
                ptr::addr_of_mut!((*node).signalers_list),
            );
            list_add_rcu(
                ptr::addr_of_mut!((*dependency).wait_link),
                ptr::addr_of_mut!((*signal).waiters_list),
            );
            (*node).flags |= (*signal).flags;
        }
        ret = true;
    }

    unsafe { spin_unlock_irq_raw(ptr::addr_of_mut!(SCHEDULE_LOCK)) };
    ret
}

// upstream: i915_scheduler.c i915_sched_node_add_dependency()
pub unsafe fn i915_sched_node_add_dependency(
    node: *mut I915SchedNode,
    signal: *mut I915SchedNode,
    flags: c_ulong,
) -> c_int {
    let dependency = unsafe { i915_dependency_alloc() };
    if dependency.is_null() {
        return -ENOMEM;
    }

    if !unsafe {
        __i915_sched_node_add_dependency(node, signal, dependency, flags | I915_DEPENDENCY_ALLOC)
    } {
        unsafe { i915_dependency_free(dependency) };
    }

    0
}

// upstream: i915_scheduler.c i915_sched_node_fini()
pub unsafe fn i915_sched_node_fini(node: *mut I915SchedNode) {
    let signal_link_offset = offset_of!(I915Dependency, signal_link);
    let wait_link_offset = offset_of!(I915Dependency, wait_link);
    unsafe { spin_lock_irq_raw(ptr::addr_of_mut!(SCHEDULE_LOCK)) };

    let signalers = unsafe { ptr::addr_of_mut!((*node).signalers_list) };
    let mut cursor = unsafe { (*signalers).next };
    while !ptr::eq(cursor, signalers) {
        let dependency = unsafe { dependency_from_link(cursor, signal_link_offset) };
        let next = unsafe { (*cursor).next };
        GEM_BUG_ON!(!list_empty(unsafe { &(*dependency).dfs_link }));
        unsafe { list_del_rcu(ptr::addr_of_mut!((*dependency).wait_link)) };
        if unsafe { (*dependency).flags & I915_DEPENDENCY_ALLOC != 0 } {
            unsafe { i915_dependency_free(dependency) };
        }
        cursor = next;
    }
    unsafe { INIT_LIST_HEAD!(ptr::addr_of_mut!((*node).signalers_list)) };

    let waiters = unsafe { ptr::addr_of_mut!((*node).waiters_list) };
    let mut cursor = unsafe { (*waiters).next };
    while !ptr::eq(cursor, waiters) {
        let dependency = unsafe { dependency_from_link(cursor, wait_link_offset) };
        let next = unsafe { (*cursor).next };
        GEM_BUG_ON!(unsafe { (*dependency).signaler != node });
        GEM_BUG_ON!(!list_empty(unsafe { &(*dependency).dfs_link }));
        unsafe { list_del_rcu(ptr::addr_of_mut!((*dependency).signal_link)) };
        if unsafe { (*dependency).flags & I915_DEPENDENCY_ALLOC != 0 } {
            unsafe { i915_dependency_free(dependency) };
        }
        cursor = next;
    }
    unsafe { INIT_LIST_HEAD!(ptr::addr_of_mut!((*node).waiters_list)) };

    unsafe { spin_unlock_irq_raw(ptr::addr_of_mut!(SCHEDULE_LOCK)) };
}

// upstream: i915_scheduler.c i915_request_show_with_schedule()
pub unsafe fn i915_request_show_with_schedule(
    printer: *mut DrmPrinter,
    request: *const I915Request,
    prefix: *const c_char,
    indent: c_int,
) {
    unsafe {
        crate::i915_request_types_upstream::i915_request_show(printer, request, prefix, indent)
    };
    if i915_request_completed(unsafe { &*request }) {
        return;
    }

    let sched = unsafe { ptr::addr_of!((*request).sched) } as *const I915SchedNode;
    let link_offset = offset_of!(I915Dependency, signal_link);
    rcu_read_lock();
    let head = unsafe {
        ptr::addr_of!((*sched).signalers_list) as *mut crate::intel_engine_cs_upstream::ListHead
    };
    let mut cursor = unsafe { (*head).next };
    while !ptr::eq(cursor, head) {
        let dependency = unsafe { dependency_from_link(cursor, link_offset) };
        let next = unsafe { (*cursor).next };
        let signaler = unsafe { node_to_request((*dependency).signaler) };
        if unsafe { (*signaler).timeline == (*request).timeline }
            || unsafe { __i915_request_is_complete(signaler) }
        {
            cursor = next;
            continue;
        }
        unsafe {
            crate::i915_request_types_upstream::i915_request_show(
                printer,
                signaler,
                prefix,
                indent + 2,
            )
        };
        cursor = next;
    }
    rcu_read_unlock();
}

// upstream: i915_scheduler.c default_destroy()
unsafe extern "C" fn default_destroy(kref: *mut Kref) {
    let sched_engine = unsafe {
        kref.cast::<u8>()
            .sub(offset_of!(I915SchedEngine, r#ref))
            .cast::<I915SchedEngine>()
    };
    unsafe { tasklet_kill(ptr::addr_of_mut!((*sched_engine).tasklet)) };
    unsafe { kfree(sched_engine) };
}

// upstream: i915_scheduler.c default_disabled()
unsafe extern "C" fn default_disabled(_sched_engine: *mut I915SchedEngine) -> bool {
    false
}

// upstream: i915_scheduler.c i915_sched_engine_create()
pub unsafe fn i915_sched_engine_create(subclass: u32) -> *mut I915SchedEngine {
    let sched_engine = unsafe { kzalloc_obj_flags::<I915SchedEngine>(GFP_KERNEL) };
    if sched_engine.is_null() {
        return ptr::null_mut();
    }

    unsafe {
        kref_init(ptr::addr_of_mut!((*sched_engine).r#ref));
        (*sched_engine).queue = RbRootCached {
            root: RbRoot {
                node: ptr::null_mut(),
            },
            leftmost: ptr::null_mut(),
        };
        (*sched_engine).queue_priority_hint = i32::MIN;
        (*sched_engine).destroy = Some(default_destroy);
        (*sched_engine).disabled = Some(default_disabled);
        INIT_LIST_HEAD!(ptr::addr_of_mut!((*sched_engine).requests));
        INIT_LIST_HEAD!(ptr::addr_of_mut!((*sched_engine).hold));
        spin_lock_init(&mut (*sched_engine).lock);
    }

    // CONFIG_DEBUG_LOCK_ALLOC is absent in the current kernel configuration;
    // its lock subclass/debug-map side effects therefore are compiled out.
    let _ = subclass;
    sched_engine
}

// upstream: i915_scheduler.c i915_scheduler_module_exit()
pub unsafe fn i915_scheduler_module_exit() {
    unsafe {
        kmem_cache_destroy(SLAB_DEPENDENCIES);
        kmem_cache_destroy(SLAB_PRIORITIES);
        SLAB_DEPENDENCIES = ptr::null_mut();
        SLAB_PRIORITIES = ptr::null_mut();
    }
}

// upstream: i915_scheduler.c i915_scheduler_module_init()
pub unsafe fn i915_scheduler_module_init() -> c_int {
    // The LinuxKPI cache supports object-size/alignment plus reuse, but not
    // SLAB_TYPESAFE_BY_RCU reclamation; see i915_dependency_free().
    unsafe { SLAB_DEPENDENCIES = kmem_cache_create::<I915Dependency>(SLAB_HWCACHE_ALIGN) };
    if unsafe { SLAB_DEPENDENCIES.is_null() } {
        return -ENOMEM;
    }

    unsafe { SLAB_PRIORITIES = kmem_cache_create::<I915Priolist>(0) };
    if unsafe { SLAB_PRIORITIES.is_null() } {
        unsafe { kmem_cache_destroy(SLAB_DEPENDENCIES) };
        unsafe { SLAB_DEPENDENCIES = ptr::null_mut() };
        return -ENOMEM;
    }

    0
}
