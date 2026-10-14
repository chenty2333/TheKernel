// SPDX-License-Identifier: MIT
//! Kernel-owned callbacks shared by the upstream i915 display code.
//!
//! Only services backed by the existing i915 GEM/GGTT owners are published
//! here. DPT and inherited-plane backing stay absent until their full owners
//! can validate and retain the firmware pages; the native modeset gate must
//! remain closed while those callbacks are missing.

use alloc::boxed::Box;
use core::{cell::UnsafeCell, ffi::c_void, ptr};

use intel_gt::{
    PIN_MAPPABLE, PIN_NOEVICT, PIN_OFFSET_GUARD, PIN_ZONE_4G,
    i915_gem_core_upstream::i915_gem_object_ggtt_pin,
    i915_gem_object_api_upstream::i915_gem_object_unpin_map,
    i915_gem_object_header_upstream::{
        i915_gem_object_is_protected, i915_gem_object_is_tiled, i915_gem_object_is_userptr,
        i915_gem_object_put,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, I915_MAP_WB},
    i915_gem_pages_upstream::{__i915_gem_object_flush_map, i915_gem_object_pin_map_unlocked},
    i915_gem_shmem_upstream::i915_gem_object_is_shmem,
    i915_vma_api_upstream::{i915_vma_offset, i915_vma_put, i915_vma_unpin, i915_vma_unpin_fence},
    i915_vma_types_upstream::I915Vma,
    intel_parent_upstream::{
        IntelDisplayBoInterface, IntelDisplayDsbInterface, IntelDisplayFbPinInterface,
        IntelDisplayIrqInterface, IntelDisplayParentInterface,
    },
    intel_plane_upstream::IntelFbPinParams,
    linux_i915_private::DrmI915Private,
};
use tk_drm_upstream::drm_device_types::DrmDevice;
use tk_linux_kapi::config::{IS_ERR, PTR_ERR};

const PAGE_SIZE: usize = 4096;
const EINVAL: i32 = 22;
const ENODEV: i32 = 19;
const ENOMEM: i32 = 12;
const EOPNOTSUPP: i32 = 95;
const ERANGE: i32 = 34;

#[repr(C)]
struct DsbBuffer {
    object: *mut DrmI915GemObject,
    vma: *mut I915Vma,
    mapping: *mut u8,
    size: usize,
    ggtt_offset: u32,
}

#[inline]
fn err_ptr(error: i32) -> *mut c_void {
    (error as isize) as *mut c_void
}

#[inline]
unsafe fn cleanup_dsb(buffer: *mut DsbBuffer) {
    if buffer.is_null() {
        return;
    }
    // SAFETY: `buffer` is verified non-null and was allocated via `Box::into_raw` in `dsb_create`.
    let state = unsafe { &*buffer };
    if !state.mapping.is_null() {
        // SAFETY: `state.mapping` is non-null, indicating CPU mapping was successfully pinned on `state.object`.
        unsafe { i915_gem_object_unpin_map(state.object) };
    }
    if !state.vma.is_null() {
        // SAFETY: `state.vma` is non-null and holds an active GGTT pin acquired during `dsb_create`.
        unsafe { i915_vma_unpin(state.vma) };
        // SAFETY: Releases the VMA reference held by `state.vma`.
        unsafe { i915_vma_put(state.vma) };
    }
    if !state.object.is_null() {
        // SAFETY: `state.object` is non-null and holds an active GEM reference from creation.
        unsafe { i915_gem_object_put(state.object) };
    }
    // SAFETY: `buffer` was allocated via `Box::try_new` and turned into raw pointer in `dsb_create`; reclaims ownership to drop.
    unsafe { drop(Box::from_raw(buffer)) };
}

/// Parent DSB storage: an i915-owned internal GEM object, pinned in its real
/// GGTT range and write-backed through the upstream object's WB map API.
unsafe extern "C" fn dsb_create(drm: *mut DrmDevice, requested: usize) -> *mut c_void {
    if drm.is_null() || requested == 0 {
        return err_ptr(-EINVAL);
    }
    let Some(size) = requested
        .checked_add(PAGE_SIZE - 1)
        .map(|size| size & !(PAGE_SIZE - 1))
        .filter(|size| *size != 0)
    else {
        return err_ptr(-ERANGE);
    };
    let Some(state) = Box::try_new(DsbBuffer {
        object: ptr::null_mut(),
        vma: ptr::null_mut(),
        mapping: ptr::null_mut(),
        size,
        ggtt_offset: 0,
    })
    .ok() else {
        return err_ptr(-ENOMEM);
    };
    let state = Box::into_raw(state);

    // SAFETY: `drm` was validated non-null above; dereferences `dev_private` to access the driver private structure.
    let i915 = unsafe { (*drm).dev_private.cast::<DrmI915Private>() };
    if i915.is_null() {
        // SAFETY: `state` was allocated with `Box::into_raw` above; cleans up partial allocation.
        unsafe { cleanup_dsb(state) };
        return err_ptr(-ENODEV);
    }
    // SAFETY: `i915` is a verified non-null pointer to `DrmI915Private`; creates internal GEM object of `size` bytes.
    let object = unsafe { intel_gt::i915_gem_object_create_internal(i915, size as u64) };
    if object.is_null() || IS_ERR(object) {
        let error = if object.is_null() {
            -ENOMEM
        } else {
            PTR_ERR(object)
        };
        // SAFETY: Cleans up `state` on GEM object creation failure.
        unsafe { cleanup_dsb(state) };
        return err_ptr(error);
    }
    // SAFETY: `state` is a valid unaliased pointer to `DsbBuffer`; assigns newly created `object`.
    unsafe { (*state).object = object };

    // DSB memory must be CPU-mappable and fixed until cleanup. The object and
    // VMA are both i915-owned; no shadow GGTT mapping is introduced here.
    // SAFETY: `object` is verified non-null and valid; pins the internal GEM object into the GGTT space.
    let vma = unsafe {
        i915_gem_object_ggtt_pin(
            object,
            ptr::null(),
            size as u64,
            PAGE_SIZE as u64,
            (PIN_MAPPABLE | PIN_NOEVICT) as u64,
        )
    };
    if vma.is_null() || IS_ERR(vma) {
        let error = if vma.is_null() {
            -ENODEV
        } else {
            PTR_ERR(vma)
        };
        // SAFETY: Cleans up `state` (and releases stored `object`) on GGTT pin failure.
        unsafe { cleanup_dsb(state) };
        return err_ptr(error);
    }
    // SAFETY: `state` is valid and unaliased; stores the pinned VMA pointer.
    unsafe { (*state).vma = vma };
    // SAFETY: `vma` is verified non-null and valid; queries its bound GGTT address offset.
    let offset = unsafe { i915_vma_offset(vma) };
    let Ok(offset) = u32::try_from(offset) else {
        // SAFETY: Cleans up `state` (unpinning VMA and putting object) on offset conversion failure.
        unsafe { cleanup_dsb(state) };
        return err_ptr(-ERANGE);
    };
    if offset == 0 {
        // SAFETY: Cleans up `state` on zero GGTT offset.
        unsafe { cleanup_dsb(state) };
        return err_ptr(-ERANGE);
    }
    // SAFETY: `state` is valid and unaliased; stores the verified 32-bit GGTT offset.
    unsafe { (*state).ggtt_offset = offset };

    // SAFETY: `object` is verified valid; creates and pins CPU write-back virtual address mapping.
    let mapping = unsafe { i915_gem_object_pin_map_unlocked(object, I915_MAP_WB) };
    if mapping.is_null() || IS_ERR(mapping) {
        let error = if mapping.is_null() {
            -ENOMEM
        } else {
            PTR_ERR(mapping)
        };
        // SAFETY: Cleans up `state` on pin map failure.
        unsafe { cleanup_dsb(state) };
        return err_ptr(error);
    }
    // SAFETY: `state` is valid and unaliased; stores the pinned CPU mapping address.
    unsafe { (*state).mapping = mapping.cast() };
    state.cast()
}

unsafe extern "C" fn dsb_cleanup(buffer: *mut c_void) {
    // SAFETY: `buffer` is the pointer previously returned by `dsb_create` and caller relinquishes ownership.
    unsafe { cleanup_dsb(buffer.cast()) };
}

unsafe extern "C" fn dsb_ggtt_offset(buffer: *mut c_void) -> u32 {
    if buffer.is_null() {
        return 0;
    }
    // SAFETY: `buffer` is verified non-null and points to a valid `DsbBuffer` returned by `dsb_create`.
    unsafe { (*buffer.cast::<DsbBuffer>()).ggtt_offset }
}

unsafe extern "C" fn dsb_write(buffer: *mut c_void, index: u32, value: u32) {
    if buffer.is_null() {
        return;
    }
    // SAFETY: `buffer` is verified non-null and points to a valid `DsbBuffer` returned by `dsb_create`.
    let state = unsafe { &*buffer.cast::<DsbBuffer>() };
    let Some(offset) = (index as usize).checked_mul(core::mem::size_of::<u32>()) else {
        return;
    };
    if offset.checked_add(4).is_none_or(|end| end > state.size) || state.mapping.is_null() {
        return;
    }
    // SAFETY: `offset + 4 <= state.size` and `state.mapping` is non-null, pointing to pinned CPU mapping.
    unsafe {
        state
            .mapping
            .add(offset)
            .cast::<u32>()
            .write_volatile(value)
    };
}

unsafe extern "C" fn dsb_read(buffer: *mut c_void, index: u32) -> u32 {
    if buffer.is_null() {
        return 0;
    }
    // SAFETY: `buffer` is verified non-null and points to a valid `DsbBuffer` returned by `dsb_create`.
    let state = unsafe { &*buffer.cast::<DsbBuffer>() };
    let Some(offset) = (index as usize).checked_mul(core::mem::size_of::<u32>()) else {
        return 0;
    };
    if offset.checked_add(4).is_none_or(|end| end > state.size) || state.mapping.is_null() {
        return 0;
    }
    // SAFETY: `offset + 4 <= state.size` and `state.mapping` is non-null; volatile read of 32-bit word.
    unsafe { state.mapping.add(offset).cast::<u32>().read_volatile() }
}

unsafe extern "C" fn dsb_fill(buffer: *mut c_void, index: u32, value: u32, bytes: usize) {
    if buffer.is_null() {
        return;
    }
    // SAFETY: `buffer` is verified non-null and points to a valid `DsbBuffer` returned by `dsb_create`.
    let state = unsafe { &*buffer.cast::<DsbBuffer>() };
    let Some(offset) = (index as usize).checked_mul(core::mem::size_of::<u32>()) else {
        return;
    };
    if offset.checked_add(bytes).is_none_or(|end| end > state.size) || state.mapping.is_null() {
        return;
    }
    let mut remaining = bytes;
    let mut cursor = offset;
    while remaining >= 4 {
        // SAFETY: `cursor + 4 <= offset + bytes <= state.size` and `state.mapping` is non-null.
        unsafe {
            state
                .mapping
                .add(cursor)
                .cast::<u32>()
                .write_volatile(value)
        };
        cursor += 4;
        remaining -= 4;
    }
    if remaining != 0 {
        let value_bytes = value.to_ne_bytes();
        for byte in 0..remaining {
            // SAFETY: `cursor + byte < offset + bytes <= state.size` and `state.mapping` is non-null.
            unsafe {
                state
                    .mapping
                    .add(cursor + byte)
                    .write_volatile(value_bytes[byte])
            };
        }
    }
}

unsafe extern "C" fn dsb_flush_map(buffer: *mut c_void) {
    if buffer.is_null() {
        return;
    }
    // SAFETY: `buffer` is verified non-null and points to a valid `DsbBuffer` returned by `dsb_create`.
    let state = unsafe { &*buffer.cast::<DsbBuffer>() };
    if !state.object.is_null() {
        // SAFETY: `state.object` is verified non-null and valid; flushes mapped range to memory.
        unsafe { __i915_gem_object_flush_map(state.object, 0, state.size as u64) };
    }
}

static DSB: IntelDisplayDsbInterface = IntelDisplayDsbInterface {
    ggtt_offset: Some(dsb_ggtt_offset),
    write: Some(dsb_write),
    read: Some(dsb_read),
    fill: Some(dsb_fill),
    create: Some(dsb_create),
    cleanup: Some(dsb_cleanup),
    flush_map: Some(dsb_flush_map),
};

fn ggtt_pin_flags(params: &IntelFbPinParams) -> Result<u64, i32> {
    // The generic GGTT pin path does not implement i915's physical-contiguity
    // and physical-alignment constraints. Reject such scanouts rather than
    // silently dropping those requirements.
    if params.needs_physical || params.phys_alignment != 0 {
        return Err(-EOPNOTSUPP);
    }
    if params.alignment != 0 && !params.alignment.is_power_of_two() {
        return Err(-EINVAL);
    }
    let mut flags = 0u64;
    if params.needs_cpu_lmem_access {
        flags |= PIN_MAPPABLE as u64;
    }
    if params.needs_low_address {
        flags |= PIN_ZONE_4G as u64;
    }
    if params.vtd_guard != 0 {
        flags |= PIN_OFFSET_GUARD as u64 | u64::from(params.vtd_guard);
    }
    Ok(flags)
}

unsafe extern "C" fn fb_ggtt_pin(
    object: *mut c_void,
    params: *const c_void,
    out_vma: *mut *mut c_void,
    out_offset: *mut u32,
    out_fence_id: *mut i32,
) -> i32 {
    if object.is_null() || params.is_null() || out_vma.is_null() || out_offset.is_null() {
        return -EINVAL;
    }
    // SAFETY: `out_vma` and `out_offset` are verified non-null; `out_fence_id` is null-checked before write.
    unsafe {
        *out_vma = ptr::null_mut();
        *out_offset = 0;
        if !out_fence_id.is_null() {
            *out_fence_id = -1;
        }
    }
    // SAFETY: `params` is verified non-null and points to a valid `IntelFbPinParams` structure.
    let params = unsafe { &*params.cast::<IntelFbPinParams>() };
    let flags = match ggtt_pin_flags(params) {
        Ok(flags) => flags,
        Err(error) => return error,
    };
    let object = object.cast::<DrmI915GemObject>();
    let view = params.view;
    let vma =
        // SAFETY: `object` is verified non-null and points to a valid `DrmI915GemObject`; pins it into GGTT.
        unsafe { i915_gem_object_ggtt_pin(object, view, 0, u64::from(params.alignment), flags) };
    if vma.is_null() || IS_ERR(vma) {
        return if vma.is_null() {
            -ENODEV
        } else {
            PTR_ERR(vma)
        };
    }

    let mut fence_id = -1;
    if !out_fence_id.is_null() && params.needs_fence {
        // SAFETY: `vma` is verified non-null holding active GGTT pin; pins an allocated fence register.
        let ret = unsafe { intel_gt::i915_vma_api_upstream::i915_vma_pin_fence(vma) };
        if ret != 0 {
            // SAFETY: Restores VMA state and reference count on fence pin failure.
            unsafe {
                i915_vma_unpin(vma);
                i915_vma_put(vma);
            }
            return ret;
        }
        // SAFETY: `vma` is valid and holding a pinned fence; reads its fence pointer.
        let fence = unsafe { (*vma).fence };
        if fence.is_null() {
            // SAFETY: Restores VMA state on missing fence structure.
            unsafe {
                i915_vma_unpin(vma);
                i915_vma_put(vma);
            }
            return -ENODEV;
        }
        // SAFETY: `fence` is verified non-null and points to the assigned `I915FenceReg`.
        fence_id = unsafe { (*fence).id };
    }
    // SAFETY: `vma` is valid and pinned in GGTT; queries bound offset.
    let offset = unsafe { i915_vma_offset(vma) };
    let Ok(offset) = u32::try_from(offset) else {
        if fence_id >= 0 {
            // SAFETY: Unpins the allocated fence register on offset conversion failure.
            unsafe { i915_vma_unpin_fence(vma) };
        }
        // SAFETY: Restores VMA pin and reference count on offset conversion failure.
        unsafe {
            i915_vma_unpin(vma);
            i915_vma_put(vma);
        }
        return -ERANGE;
    };
    // SAFETY: `out_vma`, `out_offset`, and conditionally `out_fence_id` are verified non-null output pointers.
    unsafe {
        *out_vma = vma.cast();
        *out_offset = offset;
        if !out_fence_id.is_null() {
            *out_fence_id = fence_id;
        }
    }
    0
}

unsafe extern "C" fn fb_ggtt_unpin(vma: *mut c_void, fence_id: i32) {
    if vma.is_null() {
        return;
    }
    let vma = vma.cast::<I915Vma>();
    if fence_id >= 0 {
        // SAFETY: `vma` is verified non-null and points to a valid `I915Vma`.
        let fence = unsafe { (*vma).fence };
        if !fence.is_null()
            // SAFETY: `fence` is verified non-null; reads assigned fence register ID.
            && unsafe { (*fence).id == fence_id }
        {
            // SAFETY: Unpins the fence register associated with `vma`.
            unsafe { i915_vma_unpin_fence(vma) };
        } else {
            axlog::error!("i915 display: framebuffer pin fence identity changed");
        }
    }
    // SAFETY: Unpins the GGTT allocation and releases caller's reference on `vma`.
    unsafe {
        i915_vma_unpin(vma);
        i915_vma_put(vma);
    }
}

static FB_PIN: IntelDisplayFbPinInterface = IntelDisplayFbPinInterface {
    ggtt_pin: Some(fb_ggtt_pin),
    ggtt_unpin: Some(fb_ggtt_unpin),
    // DPT needs its own page-table memory/object lifecycle; it must not be
    // emulated by pinning the framebuffer object directly in the GGTT.
    dpt_pin: None,
    dpt_unpin: None,
    reuse_vma: None,
    get_map: None,
};

unsafe extern "C" fn bo_is_tiled(object: *mut c_void) -> bool {
    !object.is_null()
        // SAFETY: `object` is verified non-null and points to a valid `DrmI915GemObject`.
        && unsafe { i915_gem_object_is_tiled(object.cast()) }
}

unsafe extern "C" fn bo_is_userptr(object: *mut c_void) -> bool {
    !object.is_null()
        // SAFETY: `object` is verified non-null and points to a valid `DrmI915GemObject`.
        && unsafe { i915_gem_object_is_userptr(object.cast()) }
}

unsafe extern "C" fn bo_is_shmem(object: *mut c_void) -> bool {
    !object.is_null()
        // SAFETY: `object` is verified non-null and points to a valid `DrmI915GemObject`.
        && unsafe { i915_gem_object_is_shmem(object.cast()) }
}

unsafe extern "C" fn bo_is_protected(object: *mut c_void) -> bool {
    !object.is_null()
        // SAFETY: `object` is verified non-null and points to a valid `DrmI915GemObject`.
        && unsafe { i915_gem_object_is_protected(object.cast()) }
}

unsafe extern "C" fn bo_key_check(object: *mut c_void) -> i32 {
    // SAFETY: `bo_is_protected` validates non-null; caller supplies GEM object pointer.
    if unsafe { bo_is_protected(object) } {
        -EOPNOTSUPP
    } else {
        0
    }
}

unsafe extern "C" fn bo_framebuffer_init(object: *mut c_void, mode_cmd: *mut c_void) -> i32 {
    if object.is_null() || mode_cmd.is_null() {
        return -EINVAL;
    }
    // The display framebuffer owns one object reference between init/fini.
    // Format, pitch, extent, modifier, and plane-count checks stay in the
    // source-faithful intel_framebuffer_init() immediately after this hook.
    // SAFETY: `object` is verified non-null and references a valid DRM GEM object;
    // increments its reference count for the lifetime of this display framebuffer.
    unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_get(object.cast()) };
    0
}

unsafe extern "C" fn bo_framebuffer_fini(object: *mut c_void) {
    if !object.is_null() {
        // SAFETY: `object` is verified non-null and holds the reference taken during
        // `bo_framebuffer_init`; releases the display framebuffer's GEM reference.
        unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(object.cast()) };
    }
}

unsafe extern "C" fn bo_framebuffer_lookup(
    _drm: *mut DrmDevice,
    file: *mut c_void,
    user_mode_cmd: *const c_void,
) -> *mut c_void {
    if file.is_null() || user_mode_cmd.is_null() {
        return ptr::null_mut();
    }
    let command =
        // SAFETY: `user_mode_cmd` is non-null, properly aligned, and points to a valid
        // `DrmModeFbCmd2` struct passed from userspace DRM mode commands.
        unsafe { &*user_mode_cmd.cast::<tk_drm_upstream::uapi_drm_mode_types::DrmModeFbCmd2>() };
    if command.handles[0] == 0 {
        return ptr::null_mut();
    }
    // SAFETY: `file` is verified non-null and references a valid DRM file private; looks up
    // the GEM object for the primary buffer handle, returning a referenced GEM pointer or null.
    unsafe {
        tk_drm_upstream::drm_gem_upstream::drm_gem_object_lookup(file.cast(), command.handles[0])
            .cast()
    }
}

static BO: IntelDisplayBoInterface = IntelDisplayBoInterface {
    is_tiled: Some(bo_is_tiled),
    is_userptr: Some(bo_is_userptr),
    is_shmem: Some(bo_is_shmem),
    is_protected: Some(bo_is_protected),
    key_check: Some(bo_key_check),
    // The V1 VM bridge does not yet implement the upstream i915 GEM VMA mmap
    // contract or panic-console page reads. Leave both absent so callers fail
    // closed instead of treating a userspace mapping as an ordinary RAM map.
    fb_mmap: None,
    read_from_page: None,
    describe: None,
    framebuffer_init: Some(bo_framebuffer_init),
    framebuffer_fini: Some(bo_framebuffer_fini),
    framebuffer_lookup: Some(bo_framebuffer_lookup),
    fbdev_create: None,
    fbdev_destroy: None,
    fbdev_fill_info: None,
    fbdev_pitch_align: None,
};

unsafe extern "C" fn irq_enabled(drm: *mut DrmDevice) -> bool {
    #[cfg(target_os = "none")]
    {
        // SAFETY: Caller passes a valid `drm` pointer from upstream display initialization.
        unsafe { crate::drm::intel::irq::upstream::display_irq_enabled(drm) }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = drm;
        false
    }
}

unsafe extern "C" fn irq_synchronize(drm: *mut DrmDevice) {
    #[cfg(target_os = "none")]
    // SAFETY: Caller passes a valid `drm` pointer from upstream display initialization.
    unsafe { crate::drm::intel::irq::upstream::display_irq_synchronize(drm) };
    #[cfg(not(target_os = "none"))]
    let _ = drm;
}

static IRQ: IntelDisplayIrqInterface = IntelDisplayIrqInterface {
    enabled: Some(irq_enabled),
    synchronize: Some(irq_synchronize),
};

unsafe extern "C" fn fence_priority_display(fence: *mut c_void) {
    // SAFETY: `fence` is a valid dma_fence pointer passed by display execution callbacks.
    unsafe { intel_gt::i915_driver_upstream::fence_priority_display(fence) };
}

unsafe extern "C" fn has_auxccs(drm: *mut DrmDevice) -> bool {
    // SAFETY: `drm` points to a valid DRM device managed by upstream GT/display driver.
    unsafe { intel_gt::i915_driver_upstream::has_auxccs(drm.cast()) }
}

unsafe extern "C" fn has_fenced_regions(drm: *mut DrmDevice) -> bool {
    // SAFETY: `drm` points to a valid DRM device managed by upstream GT/display driver.
    unsafe { intel_gt::i915_driver_upstream::has_fenced_regions(drm.cast()) }
}

unsafe extern "C" fn vgpu_active(drm: *mut DrmDevice) -> bool {
    // SAFETY: `drm` points to a valid DRM device managed by upstream GT/display driver.
    unsafe { intel_gt::i915_driver_upstream::vgpu_active(drm.cast()) }
}

struct ParentTable(UnsafeCell<IntelDisplayParentInterface>);
// SAFETY: this C callback table is initialized statically and never mutated
// after publication. UnsafeCell is only needed because raw-pointer fields are
// not automatically Sync in Rust.
unsafe impl Sync for ParentTable {}

static PARENT: ParentTable = ParentTable(UnsafeCell::new(IntelDisplayParentInterface {
    bo: ptr::addr_of!(BO),
    dpt: ptr::null(),
    dsb: ptr::addr_of!(DSB),
    fb_pin: ptr::addr_of!(FB_PIN),
    frontbuffer: ptr::addr_of!(intel_gt::i915_display_frontbuffer_interface).cast(),
    hdcp: ptr::null(),
    initial_plane: ptr::null(),
    irq: ptr::addr_of!(IRQ),
    panic: ptr::null(),
    overlay: ptr::null(),
    pc8: ptr::null(),
    pcode: ptr::addr_of!(intel_gt::i915_display_pcode_interface).cast(),
    rpm: ptr::addr_of!(intel_gt::intel_runtime_pm_upstream::i915_display_rpm_interface).cast(),
    rps: ptr::addr_of!(intel_gt::intel_rps_upstream::i915_display_rps_interface).cast(),
    stolen: ptr::addr_of!(intel_gt::i915_display_stolen_interface).cast(),
    vlv_iosf: ptr::null(),
    fence_priority_display: Some(fence_priority_display),
    has_auxccs: Some(has_auxccs),
    has_fenced_regions: Some(has_fenced_regions),
    vgpu_active: Some(vgpu_active),
}));

pub(super) unsafe extern "C" fn display_parent_interface() -> *const c_void {
    PARENT.0.get().cast()
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::ffi::c_void;

    use super::{
        DSB, DsbBuffer, EINVAL, EOPNOTSUPP, IntelFbPinParams, dsb_fill, dsb_read, dsb_write,
        ggtt_pin_flags,
    };

    fn pin_params() -> IntelFbPinParams {
        IntelFbPinParams {
            view: core::ptr::null(),
            alignment: 0,
            phys_alignment: 0,
            vtd_guard: 0,
            needs_cpu_lmem_access: false,
            needs_low_address: false,
            needs_physical: false,
            needs_fence: false,
        }
    }

    #[test]
    fn ggtt_pin_adapter_preserves_supported_constraints_and_fails_closed() {
        let mut params = pin_params();
        params.needs_cpu_lmem_access = true;
        params.needs_low_address = true;
        params.vtd_guard = 0x2000;
        let flags = ggtt_pin_flags(&params).unwrap();
        assert_ne!(flags & super::PIN_MAPPABLE as u64, 0);
        assert_ne!(flags & super::PIN_ZONE_4G as u64, 0);
        assert_eq!(
            flags & super::PIN_OFFSET_GUARD as u64,
            super::PIN_OFFSET_GUARD as u64
        );
        assert_eq!(flags & 0x2000, 0x2000);

        params.needs_physical = true;
        assert_eq!(ggtt_pin_flags(&params), Err(-EOPNOTSUPP));
        params.needs_physical = false;
        params.phys_alignment = 4096;
        assert_eq!(ggtt_pin_flags(&params), Err(-EOPNOTSUPP));
        params.phys_alignment = 0;
        params.alignment = 3;
        assert_eq!(ggtt_pin_flags(&params), Err(-EINVAL));
    }

    #[test]
    fn dsb_adapter_reads_writes_and_bounds_the_real_mapping() {
        let mut backing = Box::new([0u8; 32]);
        let mut buffer = DsbBuffer {
            object: core::ptr::null_mut(),
            vma: core::ptr::null_mut(),
            mapping: backing.as_mut_ptr(),
            size: backing.len(),
            ggtt_offset: 0x1000,
        };
        let raw = core::ptr::addr_of_mut!(buffer).cast::<c_void>();
        // SAFETY: `raw` points to the valid stack-allocated `DsbBuffer` instance above.
        unsafe { dsb_write(raw, 1, 0x1122_3344) };
        // SAFETY: `raw` points to the valid stack-allocated `DsbBuffer` instance above.
        assert_eq!(unsafe { dsb_read(raw, 1) }, 0x1122_3344);
        // SAFETY: `raw` points to the valid stack-allocated `DsbBuffer` instance above.
        unsafe { dsb_fill(raw, 2, 0xaabb_ccdd, 6) };
        // SAFETY: `raw` points to the valid stack-allocated `DsbBuffer` instance above.
        assert_eq!(unsafe { dsb_read(raw, 2) }, 0xaabb_ccdd);
        assert_eq!(backing[8], 0xdd);
        assert_eq!(backing[9], 0xcc);

        let before = *backing;
        // SAFETY: `raw` points to the valid stack-allocated `DsbBuffer` instance above.
        unsafe { dsb_write(raw, 8, u32::MAX) };
        // SAFETY: `raw` points to the valid stack-allocated `DsbBuffer` instance above.
        unsafe { dsb_fill(raw, 7, 0, 8) };
        assert_eq!(*backing, before, "out-of-range DSB writes are ignored");
        assert!(DSB.write.is_some());
    }
}
