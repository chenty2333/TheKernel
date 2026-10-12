// SPDX-License-Identifier: MIT
//! Native display's reservations in the upstream GGTT allocator. The callback
//! runs after DRM-MM initialization, before any GGTT allocation or PTE writes.
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::intel_gtt_api_upstream::I915AddressSpace;

pub struct GgttOwner {
    pub attach: unsafe fn(*mut I915AddressSpace) -> i32,
    pub detach: unsafe fn(*mut I915AddressSpace),
}
static OWNER: AtomicPtr<GgttOwner> = AtomicPtr::new(core::ptr::null_mut());
pub fn install(owner: &'static GgttOwner) -> Result<(), &'static str> {
    OWNER
        .compare_exchange(
            core::ptr::null_mut(),
            core::ptr::from_ref(owner).cast_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| "GGTT owner already installed")
}
pub unsafe fn attach(vm: *mut I915AddressSpace) -> i32 {
    let owner = OWNER.load(Ordering::Acquire);
    if owner.is_null() {
        // Hosted models do not touch a firmware-owned GGTT. A product build
        // must never silently allow a second allocator over the display table.
        return if cfg!(target_os = "none") { -19 } else { 0 };
    }
    unsafe { ((*owner).attach)(vm) }
}
pub unsafe fn detach(vm: *mut I915AddressSpace) {
    let owner = OWNER.load(Ordering::Acquire);
    if !owner.is_null() {
        unsafe { ((*owner).detach)(vm) };
    }
}
