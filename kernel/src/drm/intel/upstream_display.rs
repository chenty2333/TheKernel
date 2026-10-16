// SPDX-License-Identifier: MIT
//! The display parent interface i915 hands to the upstream display code.
//!
//! Every table except IRQ is the translated i915 owner in `tk-intel-gt`
//! (Linux `i915_driver.c` `parent`); IRQ state belongs to the kernel's
//! display interrupt owner.

use core::{cell::UnsafeCell, ffi::c_void, ptr};

use intel_gt::intel_parent_upstream::{IntelDisplayIrqInterface, IntelDisplayParentInterface};
use tk_drm_upstream::drm_device_types::DrmDevice;

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
    bo: ptr::addr_of!(intel_gt::i915_display_bo_interface).cast(),
    dpt: ptr::addr_of!(intel_gt::i915_display_dpt_interface).cast(),
    dsb: ptr::addr_of!(intel_gt::i915_display_dsb_interface).cast(),
    fb_pin: ptr::addr_of!(intel_gt::i915_display_fb_pin_interface).cast(),
    frontbuffer: ptr::addr_of!(intel_gt::i915_display_frontbuffer_interface).cast(),
    hdcp: ptr::null(),
    initial_plane: ptr::addr_of!(intel_gt::i915_display_initial_plane_interface).cast(),
    irq: ptr::addr_of!(IRQ),
    panic: ptr::addr_of!(intel_gt::i915_display_panic_interface).cast(),
    overlay: ptr::null(),
    pc8: ptr::addr_of!(intel_gt::i915_display_pc8_interface).cast(),
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
