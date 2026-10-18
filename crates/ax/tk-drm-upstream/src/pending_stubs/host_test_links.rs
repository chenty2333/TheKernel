// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Host-test link stubs for C-ABI eventfd services owned by the kernel.
//! DMA-BUF and dma-fence core functions use their real implementations in
//! both host tests and the kernel image.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

// The ioctl table retains eventfd entry points in this isolated component test
// binary. Their services belong to the kernel-linked tests, not this
// fixture. Trap if reached: linking this table must not falsely validate them.
macro_rules! kernel_only_test_service {
    ($name:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> $ret {
            $(let _ = $arg;)*
            panic!(concat!(stringify!($name), " requires the kernel-linked test harness"));
        }
    };
}

kernel_only_test_service!(thekernel_eventfd_signal(ctx: *mut c_void, count: u64) -> i32);
kernel_only_test_service!(thekernel_eventfd_ctx_put(ctx: *mut c_void) -> ());
kernel_only_test_service!(thekernel_eventfd_ctx_fdget(fd: i32) -> *mut c_void);
