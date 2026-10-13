// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B2c (drm_probe_helper / drm_client / drm_crtc_helper).
//! Each symbol is defined in exactly one file here. Delete an entry when its owner lands.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

// upstream: drm_crtc_helper.c drm_connector_get_single_encoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_get_single_encoder(
    connector: *mut crate::drm_connector_types::DrmConnector,
) -> *mut crate::drm_encoder_types::DrmEncoder {
    unsafe {
        if connector.is_null() {
            return core::ptr::null_mut();
        }
        let dev = (*connector).dev;
        if dev.is_null() {
            return core::ptr::null_mut();
        }
        let possible_encoders = (*connector).possible_encoders;
        let head = core::ptr::addr_of_mut!((*dev).mode_config.encoder_list);
        let mut cur = (*head).next;
        while cur != head {
            let encoder = (cur as *mut u8).sub(8) as *mut crate::drm_encoder_types::DrmEncoder;
            let mask = 1u32 << (*encoder).index;
            if (possible_encoders & mask) != 0 {
                return encoder;
            }
            cur = (*cur).next;
        }
        core::ptr::null_mut()
    }
}
