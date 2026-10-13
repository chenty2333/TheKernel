// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! VirtIO GPU DRM driver components.
//!
//! The userspace ABI is the Linux `virtgpu_drm.h` ABI.  Keep its fixed-width
//! layouts independent of the DRM core and transport implementations so that
//! ioctl dispatch and protocol work can share one audited definition.
#![no_std]
#![allow(non_camel_case_types, non_snake_case, dead_code)]

extern crate alloc;

pub mod uapi;
pub mod transport;
