// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// ADL-P path translation of drivers/gpu/drm/i915/display/intel_atomic.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{mem::size_of, ptr};

use tk_drm_upstream::{
    drm_atomic_types::DrmAtomicCommit,
    drm_atomic_upstream::{
        drm_atomic_commit_default_clear, drm_atomic_commit_default_release, drm_atomic_commit_init,
        drm_atomic_crtc_needs_modeset, drm_atomic_get_connector_state, drm_atomic_get_crtc_state,
        drm_atomic_get_new_connector_state, drm_atomic_get_new_crtc_state,
        drm_atomic_get_new_plane_state, drm_atomic_get_old_connector_state,
        drm_atomic_get_plane_state,
    },
    drm_connector_types::{DrmConnector, DrmConnectorState},
    drm_crtc_types::{DrmCrtc, DrmCrtcState},
    drm_device_types::DrmDevice,
    drm_plane_types::{DrmPlane, DrmPlaneState},
    drm_property_types::DrmProperty,
};
use tk_linux_kapi::{
    config::{EINVAL, ERR_PTR, IS_ERR},
    memory::{kfree, kzalloc_objs},
};

use crate::{
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::{
        IntelAtomicState, IntelCrtc, IntelCrtcState, IntelDigitalConnectorState, IntelPlane,
        IntelPlaneState,
    },
    intel_global_state_upstream::intel_atomic_clear_global_state,
};

unsafe extern "C" {
    fn drm_property_blob_get(
        blob: *mut tk_drm_upstream::drm_property_types::DrmPropertyBlob,
    ) -> *mut tk_drm_upstream::drm_property_types::DrmPropertyBlob;
    fn drm_property_blob_put(blob: *mut tk_drm_upstream::drm_property_types::DrmPropertyBlob);
    fn __drm_atomic_helper_connector_duplicate_state(
        connector: *mut DrmConnector,
        state: *mut DrmConnectorState,
    );
    fn __drm_atomic_helper_crtc_duplicate_state(crtc: *mut DrmCrtc, state: *mut DrmCrtcState);
    fn __drm_atomic_helper_crtc_destroy_state(state: *mut DrmCrtcState);
    fn drm_connector_atomic_hdr_metadata_equal(
        old_state: *const DrmConnectorState,
        new_state: *const DrmConnectorState,
    ) -> bool;
}

// These are the inline fallbacks from intel_dp_tunnel.h and
// drm_dp_tunnel.h for the build with DP tunnel support disabled.
#[inline]
unsafe fn intel_dp_tunnel_atomic_cleanup_inherited_state(_state: *mut IntelAtomicState) {}
#[inline]
unsafe fn drm_dp_tunnel_ref_get(
    _tunnel: *mut core::ffi::c_void,
    _reference: *mut crate::i915_external_placeholder_types_upstream::DrmDpTunnelRef,
) {
}
#[inline]
unsafe fn drm_dp_tunnel_ref_put(
    _reference: *mut crate::i915_external_placeholder_types_upstream::DrmDpTunnelRef,
) {
}
#[inline]
unsafe fn intel_hdcp_atomic_check(
    _connector: *mut DrmConnector,
    _old: *mut DrmConnectorState,
    _new: *mut DrmConnectorState,
) {
}

#[inline]
unsafe fn display(connector: *mut DrmConnector) -> *mut IntelDisplay {
    if connector.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*connector).dev) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_digital_connector_atomic_get_property(
    connector: *mut DrmConnector,
    state: *const DrmConnectorState,
    property: *mut DrmProperty,
    val: *mut u64,
) -> i32 {
    if connector.is_null() || state.is_null() || property.is_null() || val.is_null() {
        return -EINVAL;
    }
    let display = unsafe { display(connector) };
    if display.is_null() {
        return -EINVAL;
    }
    let digital = state.cast::<IntelDigitalConnectorState>();
    unsafe {
        if property == (*display).properties.force_audio {
            *val = (*digital).force_audio as u64;
        } else if property == (*display).properties.broadcast_rgb {
            *val = (*digital).broadcast_rgb as u64;
        } else {
            return -EINVAL;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_digital_connector_atomic_set_property(
    connector: *mut DrmConnector,
    state: *mut DrmConnectorState,
    property: *mut DrmProperty,
    val: u64,
) -> i32 {
    if connector.is_null() || state.is_null() || property.is_null() {
        return -EINVAL;
    }
    let display = unsafe { display(connector) };
    if display.is_null() {
        return -EINVAL;
    }
    let digital = state.cast::<IntelDigitalConnectorState>();
    unsafe {
        if property == (*display).properties.force_audio {
            (*digital).force_audio = val as i32;
            return 0;
        }
        if property == (*display).properties.broadcast_rgb {
            (*digital).broadcast_rgb = val as i32;
            return 0;
        }
    }
    -EINVAL
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_digital_connector_atomic_check(
    connector: *mut DrmConnector,
    state: *mut DrmAtomicCommit,
) -> i32 {
    if connector.is_null() || state.is_null() {
        return -EINVAL;
    }
    let new_state = unsafe { drm_atomic_get_new_connector_state(state, connector) };
    let old_state = unsafe { drm_atomic_get_old_connector_state(state, connector) };
    if new_state.is_null() || old_state.is_null() {
        return -EINVAL;
    }
    let new_digital = new_state.cast::<IntelDigitalConnectorState>();
    let old_digital = old_state.cast::<IntelDigitalConnectorState>();
    unsafe { intel_hdcp_atomic_check(connector, old_state, new_state) };
    if unsafe { (*new_state).crtc.is_null() } {
        return 0;
    }
    let crtc_state = unsafe { drm_atomic_get_new_crtc_state(state, (*new_state).crtc) };
    if crtc_state.is_null() {
        return -EINVAL;
    }
    unsafe {
        let new = &*new_digital;
        let old = &*old_digital;
        let base_new = &*new_state;
        let base_old = &*old_state;
        if new.force_audio != old.force_audio
            || new.broadcast_rgb != old.broadcast_rgb
            || base_new.colorspace != base_old.colorspace
            || base_new.picture_aspect_ratio != base_old.picture_aspect_ratio
            || base_new.content_type != base_old.content_type
            || base_new.scaling_mode != base_old.scaling_mode
            || base_new.privacy_screen_sw_state != base_old.privacy_screen_sw_state
            || !drm_connector_atomic_hdr_metadata_equal(old_state, new_state)
        {
            (*crtc_state).set_mode_changed(true);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_digital_connector_duplicate_state(
    connector: *mut DrmConnector,
) -> *mut DrmConnectorState {
    if connector.is_null() || unsafe { (*connector).state.is_null() } {
        return ptr::null_mut();
    }
    let state = unsafe { kzalloc_objs::<IntelDigitalConnectorState, _>(1) };
    if state.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(
            (*connector).state.cast::<IntelDigitalConnectorState>(),
            state,
            1,
        );
        __drm_atomic_helper_connector_duplicate_state(connector, &mut (*state).base);
    }
    unsafe { ptr::addr_of_mut!((*state).base) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_connector_needs_modeset(
    state: *mut IntelAtomicState,
    connector: *mut DrmConnector,
) -> bool {
    if state.is_null() || connector.is_null() {
        return false;
    }
    let old = unsafe { drm_atomic_get_old_connector_state(&mut (*state).base, connector) };
    let new = unsafe { drm_atomic_get_new_connector_state(&mut (*state).base, connector) };
    if old.is_null() || new.is_null() {
        return false;
    }
    unsafe {
        (*old).crtc != (*new).crtc
            || (!(*new).crtc.is_null()
                && drm_atomic_crtc_needs_modeset(drm_atomic_get_new_crtc_state(
                    &mut (*state).base,
                    (*new).crtc,
                )))
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_any_crtc_needs_modeset(state: *mut IntelAtomicState) -> bool {
    if state.is_null() || unsafe { (*state).base.dev.is_null() || (*state).base.crtcs.is_null() } {
        return false;
    }
    let count = unsafe { (*(*state).base.dev).mode_config.num_crtc }.max(0) as usize;
    for index in 0..count {
        let new_state = unsafe { (*(*state).base.crtcs.add(index)).new_state };
        if !new_state.is_null() && unsafe { drm_atomic_crtc_needs_modeset(new_state) } {
            return true;
        }
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_digital_connector_state(
    state: *mut IntelAtomicState,
    connector: *mut crate::intel_display_types_upstream::IntelConnector,
) -> *mut IntelDigitalConnectorState {
    if state.is_null() || connector.is_null() {
        return ERR_PTR(-EINVAL);
    }
    let base =
        unsafe { drm_atomic_get_connector_state(&mut (*state).base, &mut (*connector).base) };
    if unsafe { IS_ERR(base) } {
        return base.cast();
    }
    base.cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_duplicate_state(crtc: *mut DrmCrtc) -> *mut DrmCrtcState {
    if crtc.is_null() || unsafe { (*crtc).state.is_null() } {
        return ptr::null_mut();
    }
    let old = unsafe { (*crtc).state.cast::<IntelCrtcState>() };
    let state = unsafe { kzalloc_objs::<IntelCrtcState, _>(1) };
    if state.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(old, state, 1);
        __drm_atomic_helper_crtc_duplicate_state(crtc, &mut (*state).uapi);
        for blob in [
            (*state).hw.degamma_lut,
            (*state).hw.ctm,
            (*state).hw.gamma_lut,
            (*state).pre_csc_lut,
            (*state).post_csc_lut,
        ] {
            if !blob.is_null() {
                drm_property_blob_get(blob);
            }
        }
        (*state).update_pipe = false;
        (*state).update_m_n = false;
        (*state).update_lrr = false;
        (*state).disable_cxsr = false;
        (*state).update_wm_pre = false;
        (*state).update_wm_post = false;
        (*state).fifo_changed = false;
        (*state).preload_luts = false;
        (*state).wm.need_postvbl_update = false;
        (*state).do_async_flip = false;
        (*state).fb_bits = 0;
        (*state).update_planes = 0;
        (*state).dsb_color = ptr::null_mut();
        (*state).dsb_commit = ptr::null_mut();
        (*state).use_dsb = false;
        (*state).use_flipq = false;
        // DP tunnel support is compile-time disabled in this configuration;
        // the reference is therefore required to be null. Preserve the source
        // invariant instead of silently duplicating a live reference.
        if !(*state).dp_tunnel_ref.tunnel.is_null() {
            drm_dp_tunnel_ref_get((*state).dp_tunnel_ref.tunnel, &mut (*state).dp_tunnel_ref);
        }
    }
    unsafe { ptr::addr_of_mut!((*state).uapi) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_free_hw_state(state: *mut IntelCrtcState) {
    if state.is_null() {
        return;
    }
    unsafe {
        drm_property_blob_put((*state).hw.degamma_lut);
        drm_property_blob_put((*state).hw.gamma_lut);
        drm_property_blob_put((*state).hw.ctm);
        drm_property_blob_put((*state).pre_csc_lut);
        drm_property_blob_put((*state).post_csc_lut);
        (*state).hw.degamma_lut = ptr::null_mut();
        (*state).hw.gamma_lut = ptr::null_mut();
        (*state).hw.ctm = ptr::null_mut();
        (*state).pre_csc_lut = ptr::null_mut();
        (*state).post_csc_lut = ptr::null_mut();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_destroy_state(crtc: *mut DrmCrtc, state: *mut DrmCrtcState) {
    if state.is_null() {
        return;
    }
    let intel_state = state.cast::<IntelCrtcState>();
    unsafe {
        __drm_atomic_helper_crtc_destroy_state(state);
        intel_crtc_free_hw_state(intel_state);
        if !(*intel_state).dp_tunnel_ref.tunnel.is_null() {
            drm_dp_tunnel_ref_put(&mut (*intel_state).dp_tunnel_ref);
        }
        kfree(intel_state.cast::<core::ffi::c_void>());
    }
    let _ = crtc;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_state_alloc(dev: *mut DrmDevice) -> *mut DrmAtomicCommit {
    let state = unsafe { kzalloc_objs::<IntelAtomicState, _>(1) };
    if state.is_null() {
        return ptr::null_mut();
    }
    let ret = unsafe { drm_atomic_commit_init(dev, &mut (*state).base) };
    if ret < 0 {
        unsafe { kfree(state.cast::<core::ffi::c_void>()) };
        return ptr::null_mut();
    }
    unsafe { ptr::addr_of_mut!((*state).base) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_state_free(state: *mut DrmAtomicCommit) {
    if state.is_null() {
        return;
    }
    let intel = state.cast::<IntelAtomicState>();
    unsafe {
        drm_atomic_commit_default_release(&mut (*intel).base);
        kfree((*intel).global_objs);
        kfree(intel.cast::<core::ffi::c_void>());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_state_clear(state: *mut DrmAtomicCommit) {
    if state.is_null() {
        return;
    }
    let intel = state.cast::<IntelAtomicState>();
    unsafe {
        drm_atomic_commit_default_clear(&mut (*intel).base);
        intel_atomic_clear_global_state(intel);
        (*intel).dpll_set = false;
        (*intel).modeset = false;
        intel_dp_tunnel_atomic_cleanup_inherited_state(intel);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_crtc_state(
    state: *mut DrmAtomicCommit,
    crtc: *mut IntelCrtc,
) -> *mut IntelCrtcState {
    if state.is_null() || crtc.is_null() {
        return ERR_PTR(-EINVAL);
    }
    let base = unsafe { drm_atomic_get_crtc_state(state, &mut (*crtc).base) };
    if unsafe { IS_ERR(base) } {
        base.cast()
    } else {
        base.cast()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_new_crtc_state(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> *mut IntelCrtcState {
    if state.is_null() || crtc.is_null() {
        return ptr::null_mut();
    }
    unsafe { drm_atomic_get_new_crtc_state(&mut (*state).base, &mut (*crtc).base).cast() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_old_crtc_state(
    state: *mut IntelAtomicState,
    crtc: *mut IntelCrtc,
) -> *mut IntelCrtcState {
    if state.is_null() || crtc.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        tk_drm_upstream::drm_atomic_upstream::drm_atomic_get_old_crtc_state(
            &mut (*state).base,
            &mut (*crtc).base,
        )
        .cast()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_new_plane_state(
    state: *mut IntelAtomicState,
    plane: *mut IntelPlane,
) -> *mut IntelPlaneState {
    if state.is_null() || plane.is_null() {
        return ptr::null_mut();
    }
    unsafe { drm_atomic_get_new_plane_state(&mut (*state).base, &mut (*plane).base).cast() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_plane_state(
    state: *mut IntelAtomicState,
    plane: *mut IntelPlane,
) -> *mut IntelPlaneState {
    if state.is_null() || plane.is_null() {
        return ERR_PTR(-EINVAL);
    }
    unsafe { drm_atomic_get_plane_state(&mut (*state).base, &mut (*plane).base).cast() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_needs_modeset(state: *const IntelCrtcState) -> bool {
    !state.is_null() && unsafe { drm_atomic_crtc_needs_modeset(ptr::addr_of!((*state).uapi)) }
}

// upstream: intel_display_types.h intel_crtc_has_type()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_has_type(
    crtc_state: *const IntelCrtcState,
    output_type: u32,
) -> bool {
    unsafe { (*crtc_state).output_types & (1u32 << output_type) != 0 }
}

// upstream: intel_display_types.h intel_crtc_has_dp_encoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_crtc_has_dp_encoder(crtc_state: *const IntelCrtcState) -> bool {
    use crate::intel_display_types_upstream::{INTEL_OUTPUT_DP, INTEL_OUTPUT_DP_MST, INTEL_OUTPUT_EDP};
    unsafe {
        (*crtc_state).output_types
            & ((1u32 << INTEL_OUTPUT_DP) | (1u32 << INTEL_OUTPUT_DP_MST) | (1u32 << INTEL_OUTPUT_EDP))
            != 0
    }
}

const _: () = assert!(size_of::<IntelAtomicState>() >= size_of::<DrmAtomicCommit>());

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crtc_modeset_predicate_matches_all_three_drm_flags() {
        let mut s = unsafe { core::mem::zeroed::<IntelCrtcState>() };
        assert!(!unsafe { intel_crtc_needs_modeset(&s) });
        s.uapi.set_mode_changed(true);
        assert!(unsafe { intel_crtc_needs_modeset(&s) });
        s.uapi.set_mode_changed(false);
        s.uapi.set_active_changed(true);
        assert!(unsafe { intel_crtc_needs_modeset(&s) });
        s.uapi.set_active_changed(false);
        s.uapi.set_connectors_changed(true);
        assert!(unsafe { intel_crtc_needs_modeset(&s) });
    }

    #[test]
    fn destroy_partial_crtc_state_releases_null_owned_fields_and_frees_state() {
        let state = unsafe { kzalloc_objs::<IntelCrtcState, _>(1) };
        assert!(!state.is_null());
        unsafe {
            intel_crtc_destroy_state(ptr::null_mut(), ptr::addr_of_mut!((*state).uapi));
        }
    }
}
