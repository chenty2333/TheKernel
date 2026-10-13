// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// ADL-P path translation of drivers/gpu/drm/i915/display/intel_connector.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_char, c_void},
    mem::offset_of,
    ptr,
};

use tk_drm_upstream::{
    drm_connector_types::{DrmConnector, DrmConnectorState},
    drm_device_types::DrmDevice,
    drm_encoder_types::DrmEncoder,
    drm_property_types::{DrmPropEnumList, DrmProperty},
    uapi_drm_consts::{
        DRM_MODE_LINK_STATUS_BAD, DRM_MODE_PICTURE_ASPECT_NONE, DRM_MODE_PROP_ENUM,
        DRM_MODE_SCALE_ASPECT, DRM_MODE_SCALE_CENTER, DRM_MODE_SCALE_FULLSCREEN,
    },
};
use tk_linux_kapi::{
    config::{EINVAL, ERR_PTR, GFP_KERNEL, IS_ERR},
    memory::{kfree, kzalloc_objs},
    mutex::{mutex_lock, mutex_unlock},
    primitives::msecs_to_jiffies,
    types::WorkStruct,
    workqueue::{INIT_WORK_C, cancel_work_sync, queue_work},
};

use crate::{
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::{
        HDMI_AUDIO_AUTO, HDMI_AUDIO_OFF, HDMI_AUDIO_OFF_DVI, HDMI_AUDIO_ON, INTEL_HOTPLUG_CHANGED,
        IntelConnector, IntelCrtc, IntelDigitalConnectorState, IntelEncoder,
    },
};

unsafe extern "C" {
    fn drm_connector_get(connector: *mut DrmConnector);
    fn drm_connector_put(connector: *mut DrmConnector);
    fn drm_connector_set_link_status_property(connector: *mut DrmConnector, status: u64) -> i32;
    fn drm_kms_helper_connector_hotplug_event(connector: *mut DrmConnector);
    fn __drm_atomic_helper_connector_reset(
        connector: *mut DrmConnector,
        state: *mut DrmConnectorState,
    );
    fn drm_connector_attach_encoder(connector: *mut DrmConnector, encoder: *mut DrmEncoder) -> i32;
    fn drm_modeset_lock_assert_held(
        lock: *mut tk_drm_upstream::drm_modeset_lock_types::DrmModesetLock,
    );
    fn drm_edid_free(edid: *mut c_void);
    fn drm_edid_connector_update(connector: *mut DrmConnector, edid: *const c_void);
    fn drm_edid_connector_add_modes(connector: *mut DrmConnector) -> i32;
    fn drm_edid_read_ddc(connector: *mut DrmConnector, adapter: *mut c_void) -> *mut c_void;
    fn drm_connector_cleanup(connector: *mut DrmConnector);
    fn drm_property_create_enum(
        dev: *mut DrmDevice,
        flags: u32,
        name: *const c_char,
        values: *const DrmPropEnumList,
        count: i32,
    ) -> *mut DrmProperty;
    fn drm_object_attach_property(
        object: *mut tk_drm_upstream::drm_mode_object_types::DrmModeObject,
        property: *mut DrmProperty,
        value: u64,
    ) -> i32;
    fn drm_mode_create_aspect_ratio_property(dev: *mut DrmDevice) -> i32;
    fn drm_mode_create_hdmi_colorspace_property(
        connector: *mut DrmConnector,
        supported: u32,
    ) -> i32;
    fn drm_mode_create_dp_colorspace_property(connector: *mut DrmConnector, supported: u32) -> i32;
    fn drm_connector_attach_colorspace_property(connector: *mut DrmConnector) -> i32;
    fn drm_connector_attach_scaling_mode_property(connector: *mut DrmConnector, modes: u32) -> i32;
    fn intel_panel_init_alloc(connector: *mut IntelConnector);
    fn intel_panel_register(connector: *mut IntelConnector) -> i32;
    fn intel_panel_unregister(connector: *mut IntelConnector);
    fn intel_panel_fini(connector: *mut IntelConnector);
}

unsafe fn display_for_connector(connector: *mut DrmConnector) -> *mut IntelDisplay {
    if connector.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*connector).dev) }
}

unsafe extern "C" fn connector_modeset_retry_work(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let connector = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(IntelConnector, modeset_retry_work))
            .cast::<IntelConnector>()
    };
    let display = unsafe { display_for_connector(&mut (*connector).base) };
    if display.is_null() {
        return;
    }
    unsafe {
        mutex_lock(&mut (*(*display).drm).mode_config.mutex);
        let _ = drm_connector_set_link_status_property(
            &mut (*connector).base,
            DRM_MODE_LINK_STATUS_BAD as u64,
        );
        mutex_unlock(&mut (*(*display).drm).mode_config.mutex);
        drm_kms_helper_connector_hotplug_event(&mut (*connector).base);
        drm_connector_put(&mut (*connector).base);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_queue_modeset_retry_work(connector: *mut IntelConnector) {
    if connector.is_null() {
        return;
    }
    let display = unsafe { display_for_connector(&mut (*connector).base) };
    if display.is_null() {
        return;
    }
    unsafe {
        drm_connector_get(&mut (*connector).base);
        if !queue_work(
            (*display).wq.unordered,
            &mut (*connector).modeset_retry_work,
        ) {
            drm_connector_put(&mut (*connector).base);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_cancel_modeset_retry_work(connector: *mut IntelConnector) {
    if connector.is_null() {
        return;
    }
    unsafe {
        if cancel_work_sync(&mut (*connector).modeset_retry_work) {
            drm_connector_put(&mut (*connector).base);
        }
    }
}

unsafe fn intel_connector_init(connector: *mut IntelConnector) -> i32 {
    let state = unsafe { kzalloc_objs::<IntelDigitalConnectorState, _>(1) };
    if state.is_null() {
        return -tk_linux_kapi::config::ENOMEM;
    }
    unsafe {
        __drm_atomic_helper_connector_reset(&mut (*connector).base, &mut (*state).base);
        intel_panel_init_alloc(connector);
        INIT_WORK_C(
            &mut (*connector).modeset_retry_work,
            connector_modeset_retry_work,
        );
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_alloc() -> *mut IntelConnector {
    let connector = unsafe { kzalloc_objs::<IntelConnector, _>(1) };
    if connector.is_null() {
        return ptr::null_mut();
    }
    let ret = unsafe { intel_connector_init(connector) };
    if ret < 0 {
        unsafe { kfree(connector.cast::<c_void>()) };
        return ptr::null_mut();
    }
    connector
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_free(connector: *mut IntelConnector) {
    if connector.is_null() {
        return;
    }
    unsafe {
        kfree((*connector).base.state.cast::<c_void>());
        kfree(connector.cast::<c_void>());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_destroy(connector: *mut DrmConnector) {
    if connector.is_null() {
        return;
    }
    let intel = connector.cast::<IntelConnector>();
    unsafe {
        // HDCP, MST and debugfs are compile-out features in this B4e bring-up.
        if !(*intel).detect_edid.is_null() {
            drm_edid_free((*intel).detect_edid);
        }
        intel_panel_fini(intel);
        drm_connector_cleanup(connector);
        kfree(connector.cast::<c_void>());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_register(connector: *mut DrmConnector) -> i32 {
    if connector.is_null() {
        return -EINVAL;
    }
    unsafe { intel_panel_register(connector.cast()) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_unregister(connector: *mut DrmConnector) {
    if !connector.is_null() {
        unsafe { intel_panel_unregister(connector.cast()) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_attach_encoder(
    connector: *mut IntelConnector,
    encoder: *mut IntelEncoder,
) {
    if connector.is_null() || encoder.is_null() {
        return;
    }
    unsafe {
        (*connector).encoder = encoder;
        let _ = drm_connector_attach_encoder(&mut (*connector).base, &mut (*encoder).base);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_get_hw_state(connector: *mut IntelConnector) -> bool {
    if connector.is_null() {
        return false;
    }
    let encoder = unsafe { (*connector).encoder };
    if encoder.is_null() {
        return false;
    }
    let Some(get_hw_state) = (unsafe { (*encoder).get_hw_state }) else {
        return false;
    };
    let mut pipe = 0;
    unsafe { get_hw_state(encoder, &mut pipe) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_get_pipe(connector: *mut IntelConnector) -> i32 {
    if connector.is_null() {
        return -1;
    }
    let display = unsafe { display_for_connector(&mut (*connector).base) };
    if display.is_null() {
        return -1;
    }
    let dev = unsafe { (*display).drm };
    unsafe { drm_modeset_lock_assert_held(&mut (*dev).mode_config.connection_mutex) };
    let state = unsafe { (*connector).base.state };
    if state.is_null() || unsafe { (*state).crtc.is_null() } {
        return -1;
    }
    unsafe { (*(*state).crtc.cast::<IntelCrtc>()).pipe }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_update_modes(
    connector: *mut DrmConnector,
    edid: *const c_void,
) -> i32 {
    if connector.is_null() || edid.is_null() {
        return 0;
    }
    unsafe {
        drm_edid_connector_update(connector, edid);
        drm_edid_connector_add_modes(connector)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ddc_get_modes(
    connector: *mut DrmConnector,
    adapter: *mut c_void,
) -> i32 {
    if connector.is_null() || adapter.is_null() {
        return 0;
    }
    let edid = unsafe { drm_edid_read_ddc(connector, adapter) };
    if edid.is_null() {
        return 0;
    }
    let ret = unsafe { intel_connector_update_modes(connector, edid) };
    unsafe { drm_edid_free(edid) };
    ret
}

const INTEL_BROADCAST_RGB_AUTO: i32 = 0;
const INTEL_BROADCAST_RGB_FULL: i32 = 1;
const INTEL_BROADCAST_RGB_LIMITED: i32 = 2;

unsafe fn attach_property(connector: *mut DrmConnector, property: *mut DrmProperty, value: u64) {
    if !property.is_null() {
        unsafe {
            let _ = drm_object_attach_property(&mut (*connector).base, property, value);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_attach_force_audio_property(connector: *mut DrmConnector) {
    if connector.is_null() {
        return;
    }
    let display = unsafe { display_for_connector(connector) };
    if display.is_null() {
        return;
    }
    let mut prop = unsafe { (*display).properties.force_audio };
    if prop.is_null() {
        let values = [
            DrmPropEnumList {
                r#type: HDMI_AUDIO_OFF_DVI,
                name: c"force-dvi".as_ptr().cast_mut(),
            },
            DrmPropEnumList {
                r#type: HDMI_AUDIO_OFF,
                name: c"off".as_ptr().cast_mut(),
            },
            DrmPropEnumList {
                r#type: HDMI_AUDIO_AUTO,
                name: c"auto".as_ptr().cast_mut(),
            },
            DrmPropEnumList {
                r#type: HDMI_AUDIO_ON,
                name: c"on".as_ptr().cast_mut(),
            },
        ];
        prop = unsafe {
            drm_property_create_enum(
                (*display).drm,
                0,
                c"audio".as_ptr(),
                values.as_ptr(),
                values.len() as i32,
            )
        };
        if prop.is_null() {
            return;
        }
        unsafe {
            (*display).properties.force_audio = prop;
        }
    }
    unsafe { attach_property(connector, prop, HDMI_AUDIO_AUTO as u64) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_attach_broadcast_rgb_property(connector: *mut DrmConnector) {
    if connector.is_null() {
        return;
    }
    let display = unsafe { display_for_connector(connector) };
    if display.is_null() {
        return;
    }
    let mut prop = unsafe { (*display).properties.broadcast_rgb };
    if prop.is_null() {
        let values = [
            DrmPropEnumList {
                r#type: INTEL_BROADCAST_RGB_AUTO,
                name: c"Automatic".as_ptr().cast_mut(),
            },
            DrmPropEnumList {
                r#type: INTEL_BROADCAST_RGB_FULL,
                name: c"Full".as_ptr().cast_mut(),
            },
            DrmPropEnumList {
                r#type: INTEL_BROADCAST_RGB_LIMITED,
                name: c"Limited 16:235".as_ptr().cast_mut(),
            },
        ];
        prop = unsafe {
            drm_property_create_enum(
                (*display).drm,
                DRM_MODE_PROP_ENUM,
                c"Broadcast RGB".as_ptr(),
                values.as_ptr(),
                values.len() as i32,
            )
        };
        if prop.is_null() {
            return;
        }
        unsafe {
            (*display).properties.broadcast_rgb = prop;
        }
    }
    unsafe { attach_property(connector, prop, INTEL_BROADCAST_RGB_AUTO as u64) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_attach_aspect_ratio_property(connector: *mut DrmConnector) {
    if connector.is_null() {
        return;
    }
    let dev = unsafe { (*connector).dev };
    if unsafe { drm_mode_create_aspect_ratio_property(dev) } == 0 {
        unsafe {
            attach_property(
                connector,
                (*dev).mode_config.aspect_ratio_property,
                DRM_MODE_PICTURE_ASPECT_NONE as u64,
            )
        };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_attach_hdmi_colorspace_property(connector: *mut DrmConnector) {
    if !connector.is_null()
        && unsafe { drm_mode_create_hdmi_colorspace_property(connector, 0) } == 0
    {
        unsafe {
            drm_connector_attach_colorspace_property(connector);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_attach_dp_colorspace_property(connector: *mut DrmConnector) {
    if !connector.is_null() && unsafe { drm_mode_create_dp_colorspace_property(connector, 0) } == 0
    {
        unsafe {
            drm_connector_attach_colorspace_property(connector);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_attach_scaling_mode_property(connector: *mut DrmConnector) {
    if connector.is_null() {
        return;
    }
    let modes = (1u32 << DRM_MODE_SCALE_ASPECT)
        | (1u32 << DRM_MODE_SCALE_FULLSCREEN)
        | (1u32 << DRM_MODE_SCALE_CENTER);
    unsafe {
        let _ = drm_connector_attach_scaling_mode_property(connector, modes);
        if !(*connector).state.is_null() {
            (*(*connector).state).scaling_mode = DRM_MODE_SCALE_ASPECT;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn property_enums_match_linux_order() {
        assert_eq!(HDMI_AUDIO_OFF_DVI, -2);
        assert_eq!(HDMI_AUDIO_OFF, -1);
        assert_eq!(HDMI_AUDIO_AUTO, 0);
        assert_eq!(INTEL_BROADCAST_RGB_AUTO, 0);
        assert_eq!(INTEL_BROADCAST_RGB_FULL, 1);
        assert_eq!(INTEL_BROADCAST_RGB_LIMITED, 2);
    }
}
