// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:
//! `include/drm/drm_vma_manager.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::types::DrmMm;
use tk_linux_kapi::types::DrmMmNode;
use tk_linux_kapi::types::RbNode;
use tk_linux_kapi::types::RbRoot;
use tk_linux_kapi::types::Rwlock;
use crate::drm_file_types::{DrmFile};

#[repr(C, align(8))]
pub struct DrmVmaOffsetFile {
    pub vm_rb: RbNode,
    pub vm_tag: *mut DrmFile,
    pub vm_count: u64,
}

#[repr(C, align(8))]
pub struct DrmVmaOffsetNode {
    pub vm_lock: Rwlock,
    pub vm_node: DrmMmNode,
    pub vm_files: RbRoot,
    pub driver_private: *mut c_void,
}

#[repr(C, align(8))]
pub struct DrmVmaOffsetManager {
    pub vm_lock: Rwlock,
    pub vm_addr_space_mm: DrmMm,
}

const _: () = assert!(size_of::<DrmVmaOffsetFile>() == 40);
const _: () = assert!(align_of::<DrmVmaOffsetFile>() == 8);
const _: () = assert!(core::mem::offset_of!(DrmVmaOffsetFile, vm_rb) == 0);
const _: () = assert!(size_of::<RbNode>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmVmaOffsetFile, vm_tag) == 24);
const _: () = assert!(core::mem::offset_of!(DrmVmaOffsetFile, vm_count) == 32);
const _: () = assert!(size_of::<DrmVmaOffsetNode>() == 192);
const _: () = assert!(align_of::<DrmVmaOffsetNode>() == 8);
const _: () = assert!(core::mem::offset_of!(DrmVmaOffsetNode, vm_lock) == 0);
const _: () = assert!(size_of::<Rwlock>() == 8);
const _: () = assert!(core::mem::offset_of!(DrmVmaOffsetNode, vm_node) == 8);
const _: () = assert!(size_of::<DrmMmNode>() == 168);
const _: () = assert!(core::mem::offset_of!(DrmVmaOffsetNode, vm_files) == 176);
const _: () = assert!(size_of::<RbRoot>() == 8);
const _: () = assert!(core::mem::offset_of!(DrmVmaOffsetNode, driver_private) == 184);
const _: () = assert!(size_of::<DrmVmaOffsetManager>() == 248);
const _: () = assert!(align_of::<DrmVmaOffsetManager>() == 8);
const _: () = assert!(core::mem::offset_of!(DrmVmaOffsetManager, vm_lock) == 0);
const _: () = assert!(size_of::<Rwlock>() == 8);
const _: () = assert!(core::mem::offset_of!(DrmVmaOffsetManager, vm_addr_space_mm) == 8);
const _: () = assert!(size_of::<DrmMm>() == 240);
