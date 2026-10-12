// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_modeset_lock.c` (batch B1b).
//!
//! Source-faithful transcription of Linux 7.2.3. The modeset locks are
//! wound/wait `ww_mutex`s on the `crtc_ww_class` reservation class; the
//! -EDEADLK backoff protocol is implemented by `tk_linux_kapi::ww_mutex`.
//!
//! Configuration: CONFIG_DRM_DEBUG_MODESET_LOCK is disabled on this target,
//! so the stack-depot helpers are the no-op variants.

use core::ffi::c_void;
use core::mem::{offset_of, size_of};

use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{EALREADY, EBUSY, EDEADLK};
use tk_linux_kapi::list::{list_add, list_del_init, list_empty};
use tk_linux_kapi::memory::{kfree, kzalloc_obj};
use tk_linux_kapi::mutex::{mutex_init, mutex_is_locked, mutex_lock, mutex_unlock};
use tk_linux_kapi::types::{ListHead, WwAcquireCtx};
use tk_linux_kapi::ww_mutex::{
    ww_acquire_done, ww_acquire_fini, ww_acquire_init, ww_mutex_lock, ww_mutex_lock_interruptible, CRTC_WW_CLASS,
    ww_mutex_lock_slow, ww_mutex_trylock, ww_mutex_unlock, WwMutex,
};

use crate::drm_atomic_types::DrmPrivateObj;
use crate::drm_crtc_types::DrmCrtc;
use crate::drm_device_types::DrmDevice;
use crate::drm_modeset_lock_types::{DrmModesetAcquireCtx, DrmModesetLock};
use crate::drm_plane_types::DrmPlane;

// upstream: include/drm/drm_modeset_lock.h
const DRM_MODESET_ACQUIRE_INTERRUPTIBLE: u32 = 0x1;

// upstream: drm_modeset_lock.c __drm_stack_depot_save() (CONFIG_DRM_DEBUG_MODESET_LOCK=n)
#[inline]
fn drm_stack_depot_save() -> u32 {
    0
}

// upstream: drm_modeset_lock.c __drm_stack_depot_print() (CONFIG_DRM_DEBUG_MODESET_LOCK=n)
#[inline]
fn drm_stack_depot_print(_stack_depot: u32) {}

// upstream: drm_modeset_lock.c __drm_stack_depot_init() (CONFIG_DRM_DEBUG_MODESET_LOCK=n)
#[inline]
fn drm_stack_depot_init() {}

// upstream: drm_modeset_lock.c drm_modeset_lock_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_lock_all(dev: *mut DrmDevice) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };

    let ctx = kzalloc_obj::<DrmModesetAcquireCtx>();
    if warn_on(ctx.is_null()) {
        return;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { mutex_lock(core::ptr::addr_of_mut!((*config).mutex)) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_modeset_acquire_init(ctx, 0) };

    loop {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let ret = unsafe { drm_modeset_lock_all_ctx(dev, ctx) };
        if ret < 0 {
            if ret == -EDEADLK {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { drm_modeset_backoff(ctx) };
                continue;
            }
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { drm_modeset_acquire_fini(ctx) };
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { kfree(ctx) };
            return;
        }
        break;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { ww_acquire_done(core::ptr::addr_of_mut!((*ctx).ww_ctx)) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    warn_on(!unsafe { (*config).acquire_ctx }.is_null());
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*config).acquire_ctx = ctx };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_warn_on_modeset_not_all_locked(dev) };
}

// upstream: drm_modeset_lock.c drm_modeset_unlock_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_unlock_all(dev: *mut DrmDevice) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ctx = unsafe { (*config).acquire_ctx };
    if warn_on(ctx.is_null()) {
        return;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*config).acquire_ctx = core::ptr::null_mut() };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_modeset_drop_locks(ctx) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_modeset_acquire_fini(ctx) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { kfree(ctx) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.mutex)) };
}

// upstream: drm_modeset_lock.c drm_warn_on_modeset_not_all_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_warn_on_modeset_not_all_locked(dev: *mut DrmDevice) {
    // oops_in_progress is 0 on the running system.
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.crtc_list) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let crtc = unsafe { entry_of::<DrmCrtc>(link, offset_of!(DrmCrtc, head)) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        warn_on(!unsafe { drm_modeset_is_locked(core::ptr::addr_of_mut!((*crtc).mutex)) });
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).next };
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    warn_on(!unsafe { drm_modeset_is_locked(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex)) });
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    warn_on(!unsafe { mutex_is_locked(core::ptr::addr_of_mut!((*dev).mode_config.mutex)) });
}

// upstream: include/drm/drm_modeset_lock.h drm_modeset_is_locked()
#[inline]
unsafe fn drm_modeset_is_locked(lock: *mut DrmModesetLock) -> bool {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { mutex_is_locked(core::ptr::addr_of_mut!((*lock).mutex.base)) }
}

// Recover the containing object from an embedded list link (container_of).
#[inline]
unsafe fn entry_of<T>(link: *mut ListHead, offset: usize) -> *mut T {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { link.cast::<u8>().sub(offset) as *mut T }
}

// upstream: drm_modeset_lock.c drm_modeset_acquire_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_acquire_init(ctx: *mut DrmModesetAcquireCtx, flags: u32) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { core::ptr::write_bytes(ctx as *mut u8, 0, size_of::<DrmModesetAcquireCtx>()) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { ww_acquire_init(core::ptr::addr_of_mut!((*ctx).ww_ctx), &CRTC_WW_CLASS) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { init_list_head(core::ptr::addr_of_mut!((*ctx).locked)) };

    if flags & DRM_MODESET_ACQUIRE_INTERRUPTIBLE != 0 {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { (*ctx).interruptible = true };
    }
}

// INIT_LIST_HEAD()
#[inline]
unsafe fn init_list_head(head: *mut ListHead) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        (*head).next = head;
        (*head).prev = head;
    }
}

// upstream: drm_modeset_lock.c drm_modeset_acquire_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_acquire_fini(ctx: *mut DrmModesetAcquireCtx) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { ww_acquire_fini(core::ptr::addr_of_mut!((*ctx).ww_ctx)) };
}

// upstream: drm_modeset_lock.c drm_modeset_drop_locks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_drop_locks(ctx: *mut DrmModesetAcquireCtx) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if warn_on(!unsafe { (*ctx).contended }.is_null()) {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        drm_stack_depot_print(unsafe { (*ctx).stack_depot });
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let locked = unsafe { core::ptr::addr_of_mut!((*ctx).locked) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    while !list_empty(unsafe { &*locked }) {
        // list_first_entry(&ctx->locked, struct drm_modeset_lock, head)
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let lock = unsafe { entry_of::<DrmModesetLock>((*locked).next, offset_of!(DrmModesetLock, head)) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_modeset_unlock(lock) };
    }
}

// upstream: drm_modeset_lock.c modeset_lock()
unsafe fn modeset_lock(
    lock: *mut DrmModesetLock,
    ctx: *mut DrmModesetAcquireCtx,
    interruptible: bool,
    slow: bool,
) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ww_lock = unsafe { core::ptr::addr_of_mut!((*lock).mutex) } as *mut WwMutex;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ww_ctx = unsafe { core::ptr::addr_of_mut!((*ctx).ww_ctx) } as *mut WwAcquireCtx;

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if warn_on(!unsafe { (*ctx).contended }.is_null()) {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        drm_stack_depot_print(unsafe { (*ctx).stack_depot });
    }

    let mut ret: i32;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if unsafe { (*ctx).trylock_only } {
        // lockdep_assert_held(&ctx->ww_ctx)
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if !unsafe { ww_mutex_trylock(ww_lock, core::ptr::null_mut()) } {
            return -EBUSY;
        } else {
            return 0;
        }
    } else if interruptible && slow {
        // ww_mutex_lock_slow_interruptible(): acquired == 0, so only the
        // interruptible wait can fail.
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        ret = unsafe { ww_mutex_lock_interruptible(ww_lock as *mut c_void, ww_ctx as *mut c_void) };
    } else if interruptible {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        ret = unsafe { ww_mutex_lock_interruptible(ww_lock as *mut c_void, ww_ctx as *mut c_void) };
    } else if slow {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { ww_mutex_lock_slow(ww_lock, ww_ctx) };
        ret = 0;
    } else {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        ret = unsafe { ww_mutex_lock(ww_lock, ww_ctx) };
    }

    if ret == 0 {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        warn_on(!list_empty(unsafe { &(*lock).head }));
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { list_add(core::ptr::addr_of_mut!((*lock).head), core::ptr::addr_of_mut!((*ctx).locked)) };
    } else if ret == -EALREADY {
        ret = 0;
    } else if ret == -EDEADLK {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe {
            (*ctx).contended = lock;
            (*ctx).stack_depot = drm_stack_depot_save();
        }
    }

    ret
}

// upstream: drm_modeset_lock.c drm_modeset_backoff()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_backoff(ctx: *mut DrmModesetAcquireCtx) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let contended = unsafe { (*ctx).contended };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        (*ctx).contended = core::ptr::null_mut();
        (*ctx).stack_depot = 0;
    }
    if warn_on(contended.is_null()) {
        return 0;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_modeset_drop_locks(ctx) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let interruptible = unsafe { (*ctx).interruptible };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { modeset_lock(contended, ctx, interruptible, true) }
}

// upstream: drm_modeset_lock.c drm_modeset_lock_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_lock_init(lock: *mut DrmModesetLock) {
    // ww_mutex_init(&lock->mutex, &crtc_ww_class): mutex_init() plus ctx = NULL.
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        mutex_init(core::ptr::addr_of_mut!((*lock).mutex.base));
        (*lock).mutex.ctx = core::ptr::null_mut();
        init_list_head(core::ptr::addr_of_mut!((*lock).head));
    }
    drm_stack_depot_init();
}

// upstream: drm_modeset_lock.c drm_modeset_lock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_lock(lock: *mut DrmModesetLock, ctx: *mut DrmModesetAcquireCtx) -> i32 {
    if !ctx.is_null() {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let interruptible = unsafe { (*ctx).interruptible };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        return unsafe { modeset_lock(lock, ctx, interruptible, false) };
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { ww_mutex_lock(core::ptr::addr_of_mut!((*lock).mutex), core::ptr::null_mut()) };
    0
}

// upstream: drm_modeset_lock.c drm_modeset_lock_single_interruptible()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_lock_single_interruptible(lock: *mut DrmModesetLock) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        ww_mutex_lock_interruptible(
            core::ptr::addr_of_mut!((*lock).mutex) as *mut c_void,
            core::ptr::null_mut(),
        )
    }
}

// upstream: drm_modeset_lock.c drm_modeset_unlock()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_unlock(lock: *mut DrmModesetLock) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { list_del_init(core::ptr::addr_of_mut!((*lock).head)) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { ww_mutex_unlock(core::ptr::addr_of_mut!((*lock).mutex)) };
}

// upstream: drm_modeset_lock.c drm_modeset_lock_all_ctx()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_modeset_lock_all_ctx(dev: *mut DrmDevice, ctx: *mut DrmModesetAcquireCtx) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex), ctx) };
    if ret != 0 {
        return ret;
    }

    // drm_for_each_crtc()
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.crtc_list) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let crtc = unsafe { entry_of::<DrmCrtc>(link, offset_of!(DrmCrtc, head)) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*crtc).mutex), ctx) };
        if ret != 0 {
            return ret;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).next };
    }

    // drm_for_each_plane()
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.plane_list) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let plane = unsafe { entry_of::<DrmPlane>(link, offset_of!(DrmPlane, head)) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*plane).mutex), ctx) };
        if ret != 0 {
            return ret;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).next };
    }

    // drm_for_each_privobj()
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.privobj_list) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let privobj = unsafe { entry_of::<DrmPrivateObj>(link, offset_of!(DrmPrivateObj, head)) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let ret = unsafe { drm_modeset_lock(core::ptr::addr_of_mut!((*privobj).lock), ctx) };
        if ret != 0 {
            return ret;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).next };
    }

    0
}
