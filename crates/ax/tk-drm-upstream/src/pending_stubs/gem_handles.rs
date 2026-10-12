// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_gem.c handle and print helpers (GEM batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_gem_upstream.rs when the GEM batch lands.
//!
//! Symbols: drm_gem_handle_delete, drm_gem_object_handle_get_if_exists_unlocked, drm_gem_object_handle_put_unlocked, drm_gem_print_info.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;
use crate::drm_file_types::DrmFile;
use crate::drm_print_types::DrmPrinter;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_handle_delete(_file_priv: *mut DrmFile, _handle: u32) -> i32 { -tk_linux_kapi::config::EOPNOTSUPP }

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_handle_get_if_exists_unlocked(_obj: *mut c_void) -> bool { false }

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_object_handle_put_unlocked(_obj: *mut c_void) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_print_info(_p: *mut DrmPrinter, _indent: u32, _obj: *const c_void) {}
