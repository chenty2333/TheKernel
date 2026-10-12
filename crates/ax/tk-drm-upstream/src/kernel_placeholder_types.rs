// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:

#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


/// Sized placeholder for `i2c_adapter`: a later batch replaces it with the full definition.
#[repr(C, align(8))]
pub struct I2cAdapter {
    _opaque: [u8; 1064],
}

/// Sized placeholder for `kthread_work`: a later batch replaces it with the full definition.
#[repr(C, align(8))]
pub struct KthreadWork {
    _opaque: [u8; 40],
}

const _: () = assert!(size_of::<I2cAdapter>() == 1064);
const _: () = assert!(size_of::<KthreadWork>() == 40);
