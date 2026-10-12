// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drivers/video/hdmi.c (B2e). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by hdmi_upstream.rs (B2e). Used by the drm_edid.c infoframe helpers.
//!
//! Symbols: hdmi_avi_infoframe_init, hdmi_vendor_infoframe_init.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use crate::hdmi_types::{HdmiAviInfoframe, HdmiVendorInfoframe, HDMI_INFOFRAME_TYPE_AVI};

// upstream: drivers/video/hdmi.c hdmi_avi_infoframe_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hdmi_avi_infoframe_init(frame: *mut HdmiAviInfoframe) {
    // SAFETY: the caller passes a live, correctly sized frame pointer.
    unsafe {
        core::ptr::write_bytes(frame, 0, 1);
        (*frame).r#type = HDMI_INFOFRAME_TYPE_AVI;
        (*frame).version = 2;
        (*frame).length = 13;
    }
}

// upstream: drivers/video/hdmi.c hdmi_vendor_infoframe_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hdmi_vendor_infoframe_init(frame: *mut HdmiVendorInfoframe) -> i32 {
    // SAFETY: the caller passes a live, correctly sized frame pointer.
    unsafe {
        core::ptr::write_bytes(frame, 0, 1);
        // upstream: include/linux/hdmi.h HDMI_IEEE_OUI
        (*frame).oui = 0x000c03;
    }
    0
}
