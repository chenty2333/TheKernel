// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Source-order translation of Linux 7.2.3 drivers/gpu/drm/i915/i915_memcpy.c.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_ulong, c_void},
    sync::atomic::{AtomicBool, Ordering},
};

// Linux static key `has_movntdqa`. It is enabled only by `i915_memcpy_init_early()`
// when the source's two prerequisites hold: SSE4.1 is present and the CPU is not
// a hypervisor guest. The copy routines additionally need the kernel FPU
// save/restore that `kernel_fpu_begin()` provides; TheKernel exposes no such
// guard, so the key stays disabled and `i915_memcpy_from_wc()` returns `false`,
// which the callers already treat as "use the checked fallback".
static HAS_MOVNTDQA: AtomicBool = AtomicBool::new(false);

// upstream: i915_memcpy.c i915_memcpy_from_wc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_memcpy_from_wc(dst: *mut c_void, src: *const c_void, len: c_ulong) -> bool {
    if (dst as usize | src as usize | len as usize) & 15 != 0 {
        return false;
    }
    // The movntdqa copy needs a kernel-FPU guard that TheKernel does not provide,
    // so the static key is never enabled and the caller uses its fallback.
    // The enabled branch is kept as the source's unreachable arm.
    if HAS_MOVNTDQA.load(Ordering::Relaxed) {
        panic!("i915_memcpy_from_wc: movntdqa enabled without a kernel-FPU guard");
    }
    false
}

// upstream: i915_memcpy.c i915_memcpy_init_early()
//
// Linux enables the key only with SSE4.1 and without a hypervisor. TheKernel
// has no kernel-FPU guard, so enabling it is not representable yet; the key
// remains off and the checked fallback is used.
pub fn i915_memcpy_init_early() {
    HAS_MOVNTDQA.store(false, Ordering::Release);
}
