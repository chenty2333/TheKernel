// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for callees owned by later batches. Each symbol is defined once
//! here; delete the entry when its owner lands.
//!
//! Symbols: drm_dp_mst_dpcd_read, drm_dp_mst_dpcd_write, drm_dp_aux_register_devnode, drm_dp_aux_unregister_devnode.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;
use crate::drm_dp_helper_types::DrmDpAux;

// upstream: drivers/gpu/drm/display/drm_dp_mst_topology.c drm_dp_mst_dpcd_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dp_mst_dpcd_read(
    _aux: *mut DrmDpAux,
    _offset: u32,
    _buffer: *mut c_void,
    _size: usize,
) -> isize {
    -tk_linux_kapi::config::EIO as isize
}

// upstream: drivers/gpu/drm/display/drm_dp_mst_topology.c drm_dp_mst_dpcd_write()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dp_mst_dpcd_write(
    _aux: *mut DrmDpAux,
    _offset: u32,
    _buffer: *mut c_void,
    _size: usize,
) -> isize {
    -tk_linux_kapi::config::EIO as isize
}

// upstream: drivers/gpu/drm/display/drm_dp_aux_dev.c drm_dp_aux_register_devnode()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dp_aux_register_devnode(_aux: *mut DrmDpAux) -> i32 {
    0
}

// upstream: drivers/gpu/drm/display/drm_dp_aux_dev.c drm_dp_aux_unregister_devnode()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_dp_aux_unregister_devnode(_aux: *mut DrmDpAux) {}
