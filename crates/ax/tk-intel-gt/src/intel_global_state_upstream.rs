// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of Linux v7.2.3 drivers/gpu/drm/i915/display/intel_global_state.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::offset_of, ptr};

use tk_drm_upstream::{
    drm_atomic_types::{DrmAtomicCommit, DrmCrtcsState},
    drm_modeset_lock_types::{DrmModesetAcquireCtx, DrmModesetLock},
};
use tk_linux_kapi::{
    completion::Completion,
    config::GFP_KERNEL,
    list,
    memory::{kfree, krealloc, kref_get, kref_init, kref_put, kref_read, kzalloc_obj},
    types::{Kref, ListHead},
};

use crate::{
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::{IntelAtomicState, IntelCrtc},
    intel_global_state_types_upstream::{IntelGlobalObj, IntelGlobalState, IntelGlobalStateFuncs},
};

const ENOMEM: i32 = 12;
const ETIMEDOUT: i32 = 110;

#[repr(C)]
struct IntelGlobalObjsState {
    ptr: *mut IntelGlobalObj,
    state: *mut IntelGlobalState,
    old_state: *mut IntelGlobalState,
    new_state: *mut IntelGlobalState,
}

#[repr(C, align(8))]
struct IntelGlobalCommit {
    ref_: Kref,
    done: Completion,
}

#[inline]
fn err_ptr<T>(err: i32) -> *mut T {
    err as isize as *mut T
}

#[inline]
fn is_err<T>(ptr: *const T) -> bool {
    (ptr as usize) >= (!4095usize)
}

#[inline]
unsafe fn display_from_state(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() {
        return ptr::null_mut();
    }
    let drm = unsafe { (*state).base.dev };
    unsafe { __drm_to_display(drm) }
}

#[inline]
unsafe fn crtc_at(display: *mut IntelDisplay, entry: *mut ListHead) -> *mut IntelCrtc {
    unsafe {
        entry
            .cast::<u8>()
            .sub(offset_of!(IntelCrtc, pipe_head))
            .cast()
    }
}

unsafe fn for_each_crtc(display: *mut IntelDisplay, mut f: impl FnMut(*mut IntelCrtc)) {
    if display.is_null() {
        return;
    }
    let head = unsafe { ptr::addr_of_mut!((*display).pipe_list) };
    let mut pos = unsafe { (*head).next };
    while !ptr::eq(pos, head) {
        let next = unsafe { (*pos).next };
        f(unsafe { crtc_at(display, pos) });
        pos = next;
    }
}

unsafe fn modeset_lock_is_held(ctx: *mut DrmModesetAcquireCtx, lock: *mut DrmModesetLock) -> bool {
    if ctx.is_null() || lock.is_null() {
        return false;
    }
    let head = unsafe { ptr::addr_of_mut!((*ctx).locked) };
    let mut pos = unsafe { (*head).next };
    while !ptr::eq(pos, head) {
        if ptr::eq(pos, unsafe { ptr::addr_of_mut!((*lock).head) }) {
            return true;
        }
        pos = unsafe { (*pos).next };
    }
    false
}

unsafe fn assert_global_state_read_locked(state: *mut IntelAtomicState) {
    let display = unsafe { display_from_state(state) };
    let ctx = unsafe { (*state).base.acquire_ctx };
    let mut any_locked = false;
    unsafe {
        for_each_crtc(display, |crtc| {
            let lock = ptr::addr_of_mut!((*crtc).base.mutex);
            any_locked |= modeset_lock_is_held(ctx, lock);
        });
    }
    if !any_locked {
        axlog::warn!("i915: global state accessed without a held CRTC modeset lock");
    }
}

unsafe fn assert_global_state_write_locked(
    display: *mut IntelDisplay,
    ctx: *mut DrmModesetAcquireCtx,
) {
    let mut all_locked = true;
    unsafe {
        for_each_crtc(display, |crtc| {
            let lock = ptr::addr_of_mut!((*crtc).base.mutex);
            all_locked &= modeset_lock_is_held(ctx, lock);
        });
    }
    if !all_locked {
        axlog::warn!("i915: global state write without all CRTC modeset locks held");
    }
}

unsafe extern "C" fn commit_free(ref_: *mut Kref) {
    let commit = unsafe {
        ref_.cast::<u8>()
            .sub(offset_of!(IntelGlobalCommit, ref_))
            .cast::<IntelGlobalCommit>()
    };
    unsafe { kfree(commit) };
}

unsafe fn commit_get(commit: *mut IntelGlobalCommit) -> *mut IntelGlobalCommit {
    if !commit.is_null() {
        unsafe { kref_get(ptr::addr_of_mut!((*commit).ref_)) };
    }
    commit
}

unsafe fn commit_put(commit: *mut IntelGlobalCommit) {
    if !commit.is_null() {
        unsafe { kref_put(ptr::addr_of_mut!((*commit).ref_), commit_free) };
    }
}

unsafe fn commit_new() -> *mut IntelGlobalCommit {
    let commit = kzalloc_obj::<IntelGlobalCommit>();
    if commit.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        (*commit).done = Completion::new();
        kref_init(ptr::addr_of_mut!((*commit).ref_));
    }
    commit
}

unsafe extern "C" fn global_state_free(ref_: *mut Kref) {
    let state = unsafe {
        ref_.cast::<u8>()
            .sub(offset_of!(IntelGlobalState, r#ref))
            .cast::<IntelGlobalState>()
    };
    let obj = unsafe { (*state).obj };
    unsafe { commit_put((*state).commit.cast()) };
    if !obj.is_null() {
        let funcs = unsafe { (*obj).funcs };
        if !funcs.is_null() {
            if let Some(destroy) = unsafe { (*funcs).atomic_destroy_state } {
                unsafe { destroy(obj, state) };
            }
        }
    }
}

unsafe fn global_state_get(state: *mut IntelGlobalState) -> *mut IntelGlobalState {
    if !state.is_null() {
        unsafe { kref_get(ptr::addr_of_mut!((*state).r#ref)) };
    }
    state
}

unsafe fn global_state_put(state: *mut IntelGlobalState) {
    if !state.is_null() {
        unsafe { kref_put(ptr::addr_of_mut!((*state).r#ref), global_state_free) };
    }
}

// upstream: intel_global_state.c intel_atomic_global_obj_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_global_obj_init(
    display: *mut IntelDisplay,
    obj: *mut IntelGlobalObj,
    state: *mut IntelGlobalState,
    funcs: *const IntelGlobalStateFuncs,
) {
    if display.is_null() || obj.is_null() || state.is_null() || funcs.is_null() {
        return;
    }
    unsafe {
        ptr::write_bytes(obj.cast::<u8>(), 0, core::mem::size_of::<IntelGlobalObj>());
        (*state).obj = obj;
        kref_init(ptr::addr_of_mut!((*state).r#ref));
        (*obj).state = state;
        (*obj).funcs = funcs.cast_mut();
        list::list_add_tail(
            ptr::addr_of_mut!((*obj).head),
            ptr::addr_of_mut!((*display).global.obj_list),
        );
    }
}

// upstream: intel_global_state.c intel_atomic_global_obj_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_global_obj_cleanup(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let head = unsafe { ptr::addr_of_mut!((*display).global.obj_list) };
    let mut pos = unsafe { (*head).next };
    while !ptr::eq(pos, head) {
        let next = unsafe { (*pos).next };
        let obj = unsafe {
            pos.cast::<u8>()
                .sub(offset_of!(IntelGlobalObj, head))
                .cast::<IntelGlobalObj>()
        };
        unsafe {
            list::list_del(pos);
            if !(*obj).state.is_null() {
                let refs = kref_read(&(*(*obj).state).r#ref);
                if refs != 1 {
                    axlog::warn!("i915: global object cleanup with {} state references", refs);
                }
                global_state_put((*obj).state);
            }
        }
        pos = next;
    }
}

// upstream: intel_global_state.c intel_atomic_get_global_obj_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_global_obj_state(
    state: *mut IntelAtomicState,
    obj: *mut IntelGlobalObj,
) -> *mut IntelGlobalState {
    if state.is_null() || obj.is_null() {
        return err_ptr(-ENOMEM);
    }
    let count = unsafe { (*state).num_global_objs };
    if count < 0 {
        return err_ptr(-ENOMEM);
    }
    let entries = unsafe { (*state).global_objs.cast::<IntelGlobalObjsState>() };
    for i in 0..count as usize {
        let entry = unsafe { &*entries.add(i) };
        if entry.ptr == obj {
            return entry.state;
        }
    }

    unsafe { assert_global_state_read_locked(state) };
    let num_objs = count as usize + 1;
    let bytes = match core::mem::size_of::<IntelGlobalObjsState>().checked_mul(num_objs) {
        Some(bytes) => bytes,
        None => return err_ptr(-ENOMEM),
    };
    let expanded =
        unsafe { krealloc((*state).global_objs, bytes, GFP_KERNEL) }.cast::<IntelGlobalObjsState>();
    if expanded.is_null() {
        return err_ptr(-ENOMEM);
    }
    unsafe { (*state).global_objs = expanded.cast() };
    let entry = unsafe { expanded.add(count as usize) };
    unsafe {
        ptr::write_bytes(
            entry.cast::<u8>(),
            0,
            core::mem::size_of::<IntelGlobalObjsState>(),
        )
    };

    let funcs = unsafe { (*obj).funcs };
    let duplicate = if funcs.is_null() {
        None
    } else {
        unsafe { (*funcs).atomic_duplicate_state }
    };
    let obj_state = match duplicate {
        Some(duplicate) => unsafe { duplicate(obj) },
        None => ptr::null_mut(),
    };
    if obj_state.is_null() {
        return err_ptr(-ENOMEM);
    }

    unsafe {
        (*obj_state).obj = obj;
        (*obj_state).changed = false;
        (*obj_state).serialized = false;
        (*obj_state).commit = ptr::null_mut();
        kref_init(ptr::addr_of_mut!((*obj_state).r#ref));
        (*entry).state = obj_state;
        (*entry).old_state = global_state_get((*obj).state);
        (*entry).new_state = obj_state;
        (*entry).ptr = obj;
        (*obj_state).state = state;
        (*state).num_global_objs = num_objs as i32;
    }
    obj_state
}

// upstream: intel_global_state.c intel_atomic_get_old_global_obj_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_old_global_obj_state(
    state: *mut IntelAtomicState,
    obj: *mut IntelGlobalObj,
) -> *mut IntelGlobalState {
    if state.is_null() || obj.is_null() {
        return ptr::null_mut();
    }
    let count = unsafe { (*state).num_global_objs.max(0) as usize };
    let entries = unsafe { (*state).global_objs.cast::<IntelGlobalObjsState>() };
    for i in 0..count {
        let entry = unsafe { &*entries.add(i) };
        if entry.ptr == obj {
            return entry.old_state;
        }
    }
    ptr::null_mut()
}

// upstream: intel_global_state.c intel_atomic_get_new_global_obj_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_new_global_obj_state(
    state: *mut IntelAtomicState,
    obj: *mut IntelGlobalObj,
) -> *mut IntelGlobalState {
    if state.is_null() || obj.is_null() {
        return ptr::null_mut();
    }
    let count = unsafe { (*state).num_global_objs.max(0) as usize };
    let entries = unsafe { (*state).global_objs.cast::<IntelGlobalObjsState>() };
    for i in 0..count {
        let entry = unsafe { &*entries.add(i) };
        if entry.ptr == obj {
            return entry.new_state;
        }
    }
    ptr::null_mut()
}

// upstream: intel_global_state.c intel_atomic_swap_global_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_swap_global_state(state: *mut IntelAtomicState) {
    if state.is_null() {
        return;
    }
    let display = unsafe { display_from_state(state) };
    let count = unsafe { (*state).num_global_objs.max(0) as usize };
    let entries = unsafe { (*state).global_objs.cast::<IntelGlobalObjsState>() };
    for i in 0..count {
        let entry = unsafe { &mut *entries.add(i) };
        let obj = entry.ptr;
        let old_state = entry.old_state;
        let new_state = entry.new_state;
        if obj.is_null() || old_state.is_null() || new_state.is_null() {
            continue;
        }
        if unsafe { (*obj).state != old_state } {
            axlog::warn!("i915: global object state changed outside the atomic transaction");
        }
        if !unsafe { (*new_state).changed } {
            continue;
        }
        unsafe { assert_global_state_write_locked(display, (*state).base.acquire_ctx) };
        unsafe {
            (*old_state).state = state;
            (*new_state).state = ptr::null_mut();
            entry.state = old_state;
            global_state_put((*obj).state);
            (*obj).state = global_state_get(new_state);
        }
    }
}

// upstream: intel_global_state.c intel_atomic_clear_global_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_clear_global_state(state: *mut IntelAtomicState) {
    if state.is_null() {
        return;
    }
    let count = unsafe { (*state).num_global_objs.max(0) as usize };
    let entries = unsafe { (*state).global_objs.cast::<IntelGlobalObjsState>() };
    for i in 0..count {
        let entry = unsafe { &mut *entries.add(i) };
        unsafe {
            global_state_put(entry.old_state);
            global_state_put(entry.new_state);
            entry.ptr = ptr::null_mut();
            entry.state = ptr::null_mut();
            entry.old_state = ptr::null_mut();
            entry.new_state = ptr::null_mut();
        }
    }
    unsafe { (*state).num_global_objs = 0 };
}

// upstream: intel_global_state.c intel_atomic_lock_global_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_lock_global_state(obj_state: *mut IntelGlobalState) -> i32 {
    if obj_state.is_null() || unsafe { (*obj_state).state.is_null() } {
        return -ENOMEM;
    }
    let state = unsafe { (*obj_state).state };
    let display = unsafe { display_from_state(state) };
    let ctx = unsafe { (*state).base.acquire_ctx };
    if ctx.is_null() {
        return -ENOMEM;
    }
    let mut error = 0;
    unsafe {
        for_each_crtc(display, |crtc| {
            if error == 0 {
                error = tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_lock(
                    ptr::addr_of_mut!((*crtc).base.mutex),
                    ctx,
                );
            }
        });
    }
    if error != 0 {
        return error;
    }
    unsafe { (*obj_state).changed = true };
    0
}

// upstream: intel_global_state.c intel_atomic_serialize_global_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_serialize_global_state(
    obj_state: *mut IntelGlobalState,
) -> i32 {
    let ret = unsafe { intel_atomic_lock_global_state(obj_state) };
    if ret != 0 {
        return ret;
    }
    unsafe { (*obj_state).serialized = true };
    0
}

// upstream: intel_global_state.c intel_atomic_global_state_is_serialized()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_global_state_is_serialized(
    state: *mut IntelAtomicState,
) -> bool {
    if state.is_null() {
        return false;
    }
    let display = unsafe { display_from_state(state) };
    let mut all_present = true;
    unsafe {
        for_each_crtc(display, |crtc| {
            let found = new_crtc_state_present(state, crtc);
            all_present &= found;
        });
    }
    all_present
}

unsafe fn new_crtc_state_present(state: *mut IntelAtomicState, crtc: *mut IntelCrtc) -> bool {
    let drm = unsafe { (*state).base.dev };
    if drm.is_null() {
        return false;
    }
    let entries = unsafe { (*state).base.crtcs };
    let count = unsafe { (*drm).num_crtcs as usize };
    if entries.is_null() {
        return false;
    }
    for i in 0..count {
        let entry = unsafe { &*entries.add(i) };
        if entry.ptr == ptr::addr_of_mut!((*crtc).base) {
            return !entry.new_state.is_null();
        }
    }
    false
}

// upstream: intel_global_state.c intel_atomic_global_state_setup_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_global_state_setup_commit(
    state: *mut IntelAtomicState,
) -> i32 {
    if state.is_null() {
        return -ENOMEM;
    }
    let count = unsafe { (*state).num_global_objs.max(0) as usize };
    let entries = unsafe { (*state).global_objs.cast::<IntelGlobalObjsState>() };
    for i in 0..count {
        let entry = unsafe { &mut *entries.add(i) };
        if entry.new_state.is_null() || entry.old_state.is_null() {
            continue;
        }
        let new_state = entry.new_state;
        let commit = if unsafe { (*new_state).serialized } {
            let commit = unsafe { commit_new() };
            if commit.is_null() {
                return -ENOMEM;
            }
            commit
        } else if unsafe { (*new_state).changed } {
            let old_commit = unsafe { (*entry.old_state).commit.cast::<IntelGlobalCommit>() };
            unsafe { commit_get(old_commit) }
        } else {
            ptr::null_mut()
        };
        unsafe { (*new_state).commit = commit.cast() };
    }
    0
}

// upstream: intel_global_state.c intel_atomic_global_state_wait_for_dependencies()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_global_state_wait_for_dependencies(
    state: *mut IntelAtomicState,
) -> i32 {
    if state.is_null() {
        return -ENOMEM;
    }
    let count = unsafe { (*state).num_global_objs.max(0) as usize };
    let entries = unsafe { (*state).global_objs.cast::<IntelGlobalObjsState>() };
    let timeout = 10 * crate::linux_config::HZ as i64;
    for i in 0..count {
        let entry = unsafe { &*entries.add(i) };
        if entry.old_state.is_null() {
            continue;
        }
        let commit = unsafe { (*entry.old_state).commit.cast::<IntelGlobalCommit>() };
        if commit.is_null() {
            continue;
        }
        if unsafe { (*commit).done.wait_timeout(timeout) } == 0 {
            return -ETIMEDOUT;
        }
    }
    0
}

// upstream: intel_global_state.c intel_atomic_global_state_commit_done()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_global_state_commit_done(state: *mut IntelAtomicState) {
    if state.is_null() {
        return;
    }
    let count = unsafe { (*state).num_global_objs.max(0) as usize };
    let entries = unsafe { (*state).global_objs.cast::<IntelGlobalObjsState>() };
    for i in 0..count {
        let entry = unsafe { &*entries.add(i) };
        let new_state = entry.new_state;
        if new_state.is_null() || !unsafe { (*new_state).serialized } {
            continue;
        }
        let commit = unsafe { (*new_state).commit.cast::<IntelGlobalCommit>() };
        if !commit.is_null() {
            unsafe { (*commit).done.complete_all() };
        }
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use tk_linux_kapi::{
        completion::Completion,
        list::{INIT_LIST_HEAD, list_empty},
        memory::kref_read,
        types::{AtomicT, Kref, RefcountT},
    };

    use super::{
        IntelGlobalCommit, IntelGlobalObjsState, err_ptr, intel_atomic_global_obj_cleanup,
        intel_atomic_global_obj_init, is_err,
    };
    use crate::{
        intel_display_core_types_upstream::IntelDisplay,
        intel_global_state_types_upstream::{
            IntelGlobalObj, IntelGlobalState, IntelGlobalStateFuncs,
        },
    };

    static DESTROYED_STATES: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn test_destroy_state(
        _obj: *mut IntelGlobalObj,
        _state: *mut IntelGlobalState,
    ) {
        DESTROYED_STATES.fetch_add(1, Ordering::Relaxed);
    }

    static TEST_GLOBAL_STATE_FUNCS: IntelGlobalStateFuncs = IntelGlobalStateFuncs {
        atomic_duplicate_state: None,
        atomic_destroy_state: Some(test_destroy_state),
    };

    #[test]
    fn error_pointer_encoding_matches_linux_err_ptr() {
        let ptr: *mut IntelGlobalObjsState = err_ptr(-12);
        assert!(is_err(ptr));
        assert_eq!(ptr as isize, -12);
    }

    #[test]
    fn commit_completion_is_sized_for_kapi_completion() {
        let commit = IntelGlobalCommit {
            ref_: Kref {
                refcount: RefcountT {
                    refs: AtomicT { counter: 0 },
                },
            },
            done: Completion::new(),
        };
        assert_eq!(core::mem::size_of_val(&commit), 40);
        assert!(!commit.done.is_done());
        commit.done.complete_all();
        assert!(commit.done.is_done());
    }

    #[test]
    fn global_object_init_and_cleanup_hold_and_release_the_owner_reference() {
        DESTROYED_STATES.store(0, Ordering::Relaxed);
        let mut display: IntelDisplay = unsafe { core::mem::zeroed() };
        let mut obj: IntelGlobalObj = unsafe { core::mem::zeroed() };
        let mut state: IntelGlobalState = unsafe { core::mem::zeroed() };
        unsafe { INIT_LIST_HEAD(core::ptr::addr_of_mut!(display.global.obj_list)) };

        unsafe {
            intel_atomic_global_obj_init(
                &mut display,
                &mut obj,
                &mut state,
                &TEST_GLOBAL_STATE_FUNCS,
            )
        };
        assert_eq!(kref_read(&state.r#ref), 1);
        assert!(!list_empty(&display.global.obj_list));

        unsafe { intel_atomic_global_obj_cleanup(&mut display) };

        assert!(list_empty(&display.global.obj_list));
        assert_eq!(DESTROYED_STATES.load(Ordering::Relaxed), 1);
    }
}
