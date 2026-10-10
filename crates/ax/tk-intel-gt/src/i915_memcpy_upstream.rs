// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Source-order translation of Linux 7.2.3 drivers/gpu/drm/i915/i915_memcpy.c.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::{
    arch::x86_64::{__m128i, _mm_store_si128, _mm_storeu_si128},
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

// upstream: i915_memcpy.c __memcpy_ntdqa()
#[target_feature(enable = "sse4.1")]
unsafe fn __memcpy_ntdqa(dst: *mut c_void, src: *const c_void, mut len: c_ulong) {
    let mut src = src.cast::<u8>();
    let mut dst = dst.cast::<u8>();
    while len >= 4 {
        let s = src as *const __m128i;
        let d = dst as *mut __m128i;
        let x0 = core::arch::x86_64::_mm_stream_load_si128(s.cast_mut());
        let x1 = core::arch::x86_64::_mm_stream_load_si128(s.add(1).cast_mut());
        let x2 = core::arch::x86_64::_mm_stream_load_si128(s.add(2).cast_mut());
        let x3 = core::arch::x86_64::_mm_stream_load_si128(s.add(3).cast_mut());
        _mm_store_si128(d, x0);
        _mm_store_si128(d.add(1), x1);
        _mm_store_si128(d.add(2), x2);
        _mm_store_si128(d.add(3), x3);
        src = src.add(64);
        dst = dst.add(64);
        len -= 4;
    }
    while len != 0 {
        len -= 1;
        let x0 = core::arch::x86_64::_mm_stream_load_si128(src.cast::<__m128i>().cast_mut());
        _mm_store_si128(dst.cast::<__m128i>(), x0);
        src = src.add(16);
        dst = dst.add(16);
    }
}

// upstream: i915_memcpy.c __memcpy_ntdqu()
#[target_feature(enable = "sse4.1")]
unsafe fn __memcpy_ntdqu(dst: *mut c_void, src: *const c_void, mut len: c_ulong) {
    let mut src = src.cast::<u8>();
    let mut dst = dst.cast::<u8>();
    while len >= 4 {
        let s = src as *const __m128i;
        let d = dst as *mut __m128i;
        let x0 = core::arch::x86_64::_mm_stream_load_si128(s.cast_mut());
        let x1 = core::arch::x86_64::_mm_stream_load_si128(s.add(1).cast_mut());
        let x2 = core::arch::x86_64::_mm_stream_load_si128(s.add(2).cast_mut());
        let x3 = core::arch::x86_64::_mm_stream_load_si128(s.add(3).cast_mut());
        _mm_storeu_si128(d, x0);
        _mm_storeu_si128(d.add(1), x1);
        _mm_storeu_si128(d.add(2), x2);
        _mm_storeu_si128(d.add(3), x3);
        src = src.add(64);
        dst = dst.add(64);
        len -= 4;
    }
    while len != 0 {
        len -= 1;
        let x0 = core::arch::x86_64::_mm_stream_load_si128(src.cast::<__m128i>().cast_mut());
        _mm_storeu_si128(dst.cast::<__m128i>(), x0);
        src = src.add(16);
        dst = dst.add(16);
    }
}

// upstream: i915_memcpy.c i915_memcpy_from_wc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_memcpy_from_wc(dst: *mut c_void, src: *const c_void, len: c_ulong) -> bool {
    if (dst as usize | src as usize | len as usize) & 15 != 0 {
        return false;
    }
    if HAS_MOVNTDQA.load(Ordering::Relaxed) {
        if len != 0 {
            __memcpy_ntdqa(dst, src, len >> 4);
        }
        return true;
    }
    false
}

// upstream: i915_memcpy.c i915_unaligned_memcpy_from_wc()
pub unsafe fn i915_unaligned_memcpy_from_wc(dst: *mut c_void, src: *const c_void, mut len: c_ulong) {
    let mut dst = dst.cast::<u8>();
    let mut src = src.cast::<u8>();
    let addr = src as usize;
    if addr & 15 != 0 {
        let aligned = (addr + 15) & !15usize;
        let x = core::cmp::min((aligned - addr) as c_ulong, len);
        core::ptr::copy_nonoverlapping(src, dst, x as usize);
        len -= x;
        dst = dst.add(x as usize);
        src = src.add(x as usize);
    }
    if len != 0 {
        __memcpy_ntdqu(dst.cast(), src.cast(), len.div_ceil(16));
    }
}

// upstream: i915_memcpy.c i915_memcpy_init_early()
//
// Linux enables the key only with SSE4.1 and without a hypervisor. TheKernel
// has no kernel-FPU guard, so enabling it is not representable yet; the key
// remains off and the checked fallback is used.
pub fn i915_memcpy_init_early() {
    HAS_MOVNTDQA.store(false, Ordering::Release);
}
