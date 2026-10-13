// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Panel state shared by the Intel display connector paths.
//!
//! The ADL-P DP path has no panel power/backlight sequencer.  This module
//! retains the upstream connector bookkeeping and fixed-mode semantics while
//! deliberately leaving the eDP-only backlight sequencing opt-out.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{mem::offset_of, ptr};

use tk_drm_upstream::{
    drm_connector_types::{
        CONNECTOR_STATUS_CONNECTED, CONNECTOR_STATUS_DISCONNECTED, DrmConnector,
    },
    drm_edid_upstream::DrmEdid,
    drm_modes_types::{DrmDisplayMode, MODE_OK, MODE_PANEL},
};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::{DRRS_TYPE_NONE, DrrsType, IntelConnector, IntelEncoder},
};

const QUIRK_LVDS_SSC_DISABLE: u64 = 1 << 4;
const DRM_MODE_TYPE_PREFERRED: u8 = 1 << 3;
const DRM_MODE_TYPE_DRIVER: u8 = 1 << 6;

fn mode_from_head(head: *mut tk_linux_kapi::types::ListHead) -> *mut DrmDisplayMode {
    if head.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        head.cast::<u8>()
            .sub(offset_of!(DrmDisplayMode, head))
            .cast()
    }
}

unsafe fn next_mode(head: *mut tk_linux_kapi::types::ListHead) -> *mut DrmDisplayMode {
    let node = unsafe { (*head).next };
    if node == head {
        ptr::null_mut()
    } else {
        mode_from_head(node)
    }
}

unsafe fn display_from_connector(connector: *mut IntelConnector) -> *mut IntelDisplay {
    if connector.is_null() || (*connector).base.dev.is_null() {
        return ptr::null_mut();
    }
    crate::intel_display_conversion_upstream::__drm_to_display((*connector).base.dev)
}

unsafe fn fixed_modes_head(connector: *mut IntelConnector) -> *mut tk_linux_kapi::types::ListHead {
    if connector.is_null() {
        ptr::null_mut()
    } else {
        ptr::addr_of_mut!((*connector).panel.fixed_modes)
    }
}

unsafe fn panel_fixed_mode(
    connector: *mut IntelConnector,
    mode: *const DrmDisplayMode,
) -> *const DrmDisplayMode {
    if connector.is_null() || mode.is_null() {
        return ptr::null();
    }
    let head = unsafe { fixed_modes_head(connector) };
    if head.is_null() {
        return ptr::null();
    }
    let requested = unsafe { tk_drm_upstream::drm_modes_upstream::drm_mode_vrefresh(mode) };
    let mut best: *mut DrmDisplayMode = ptr::null_mut();
    let mut node = unsafe { (*head).next };
    while node != head {
        let candidate = mode_from_head(node);
        let refresh = unsafe { tk_drm_upstream::drm_modes_upstream::drm_mode_vrefresh(candidate) };
        if best.is_null()
            || (refresh - requested).abs()
                < (unsafe { tk_drm_upstream::drm_modes_upstream::drm_mode_vrefresh(best) }
                    - requested)
                    .abs()
        {
            best = candidate;
        }
        node = unsafe { (*node).next };
    }
    best
}

unsafe fn add_fixed_mode(connector: *mut IntelConnector, mode: *mut DrmDisplayMode) {
    if connector.is_null() || mode.is_null() {
        return;
    }
    unsafe {
        (*mode).r#type |= DRM_MODE_TYPE_PREFERRED | DRM_MODE_TYPE_DRIVER;
        tk_linux_kapi::list::list_add_tail(
            ptr::addr_of_mut!((*mode).head),
            ptr::addr_of_mut!((*connector).panel.fixed_modes),
        );
    }
}

// upstream: intel_panel.c intel_panel_init_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_init_alloc(connector: *mut IntelConnector) {
    if connector.is_null() {
        return;
    }
    unsafe {
        (*connector).panel.vbt.panel_type = -1;
        (*connector).panel.vbt.backlight.controller = -1;
        tk_linux_kapi::list::INIT_LIST_HEAD(ptr::addr_of_mut!((*connector).panel.fixed_modes));
    }
}

// upstream: intel_panel.c intel_panel_init().  ADL-P external connectors have
// no panel sequencer; retain the optional fixed EDID pointer and don't claim
// that eDP/backlight setup has happened.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_init(
    connector: *mut IntelConnector,
    fixed_edid: *const DrmEdid,
) -> i32 {
    if connector.is_null() {
        return -22;
    }
    unsafe { (*connector).panel.fixed_edid = fixed_edid.cast_mut().cast() };
    0
}

// upstream: intel_panel.c intel_panel_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_fini(connector: *mut IntelConnector) {
    if connector.is_null() {
        return;
    }
    let panel = unsafe { ptr::addr_of_mut!((*connector).panel) };
    let edid = unsafe { (*panel).fixed_edid.cast::<DrmEdid>() };
    if !edid.is_null() {
        unsafe { tk_drm_upstream::drm_edid_upstream::drm_edid_free(edid) };
        unsafe { (*panel).fixed_edid = ptr::null_mut() };
    }
    let head = unsafe { ptr::addr_of_mut!((*panel).fixed_modes) };
    if !head.is_null() {
        let mut node = unsafe { (*head).next };
        while !node.is_null() && node != head {
            let next = unsafe { (*node).next };
            let mode = mode_from_head(node);
            unsafe {
                tk_linux_kapi::list::list_del_init(node);
                tk_drm_upstream::drm_modes_upstream::drm_mode_destroy((*connector).base.dev, mode);
            }
            node = next;
        }
    }
}

// upstream registration only installs the eDP backlight hook.  ADL-P external
// SST uses no panel/backlight device.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_register(_connector: *mut IntelConnector) -> i32 {
    0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_unregister(_connector: *mut IntelConnector) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_detect(connector: *mut DrmConnector, _force: bool) -> u32 {
    if connector.is_null() {
        CONNECTOR_STATUS_DISCONNECTED
    } else if unsafe { (*connector).status } == CONNECTOR_STATUS_CONNECTED {
        CONNECTOR_STATUS_CONNECTED
    } else {
        unsafe { (*connector).status }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_use_ssc(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    let params = unsafe { (*display).params.panel_use_ssc };
    if params >= 0 {
        return params != 0;
    }
    let vbt = unsafe { (*display).vbt._bitfields_2[0] & (1 << 2) != 0 };
    vbt && unsafe { (*display).quirks.mask & QUIRK_LVDS_SSC_DISABLE == 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_preferred_fixed_mode(
    connector: *mut IntelConnector,
) -> *const DrmDisplayMode {
    let head = unsafe { fixed_modes_head(connector) };
    if head.is_null() {
        return ptr::null();
    }
    unsafe { next_mode(head) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_fixed_mode(
    connector: *mut IntelConnector,
    mode: *const DrmDisplayMode,
) -> *const DrmDisplayMode {
    unsafe { panel_fixed_mode(connector, mode) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_downclock_mode(
    connector: *mut IntelConnector,
    adjusted_mode: *const DrmDisplayMode,
) -> *const DrmDisplayMode {
    if connector.is_null() || adjusted_mode.is_null() {
        return ptr::null();
    }
    let head = unsafe { fixed_modes_head(connector) };
    if head.is_null() {
        return ptr::null();
    }
    let mut best = adjusted_mode;
    let mut node = unsafe { (*head).next };
    while node != head {
        let candidate = mode_from_head(node);
        if unsafe { (*candidate).clock < (*best).clock } {
            best = candidate;
        }
        node = unsafe { (*node).next };
    }
    best
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_highest_mode(
    connector: *mut IntelConnector,
    adjusted_mode: *const DrmDisplayMode,
) -> *const DrmDisplayMode {
    if connector.is_null() || adjusted_mode.is_null() {
        return ptr::null();
    }
    let head = unsafe { fixed_modes_head(connector) };
    if head.is_null() {
        return ptr::null();
    }
    let mut best = adjusted_mode;
    let mut node = unsafe { (*head).next };
    while node != head {
        let candidate = mode_from_head(node);
        if unsafe { (*candidate).clock > (*best).clock } {
            best = candidate;
        }
        node = unsafe { (*node).next };
    }
    best
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_get_modes(connector: *mut IntelConnector) -> i32 {
    if connector.is_null() {
        return 0;
    }
    let head = unsafe { fixed_modes_head(connector) };
    if head.is_null() {
        return 0;
    }
    let mut added = 0;
    let mut node = unsafe { (*head).next };
    while node != head {
        let mode = mode_from_head(node);
        let duplicate = unsafe {
            tk_drm_upstream::drm_modes_upstream::drm_mode_duplicate((*connector).base.dev, mode)
        };
        if !duplicate.is_null() {
            unsafe {
                tk_drm_upstream::drm_modes_upstream::drm_mode_probed_add(
                    &mut (*connector).base,
                    duplicate,
                )
            };
            added += 1;
        }
        node = unsafe { (*node).next };
    }
    added
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_drrs_type(connector: *mut IntelConnector) -> DrrsType {
    if connector.is_null() {
        DRRS_TYPE_NONE
    } else {
        unsafe { (*connector).panel.vbt.drrs_type }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_mode_valid(
    connector: *mut IntelConnector,
    mode: *const DrmDisplayMode,
    target_clock: *mut i32,
) -> i32 {
    if mode.is_null() {
        return MODE_PANEL;
    }
    if !target_clock.is_null() {
        unsafe { *target_clock = (*mode).clock };
    }
    let fixed = unsafe { panel_fixed_mode(connector, mode) };
    if fixed.is_null() {
        return MODE_OK;
    }
    if unsafe { (*mode).hdisplay != (*fixed).hdisplay || (*mode).vdisplay != (*fixed).vdisplay }
        || unsafe {
            tk_drm_upstream::drm_modes_upstream::drm_mode_vrefresh(mode)
                != tk_drm_upstream::drm_modes_upstream::drm_mode_vrefresh(fixed)
        }
    {
        return MODE_PANEL;
    }
    if !target_clock.is_null() {
        unsafe { *target_clock = (*fixed).clock };
    }
    MODE_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_compute_config(
    connector: *mut IntelConnector,
    adjusted_mode: *mut DrmDisplayMode,
) -> i32 {
    if connector.is_null() || adjusted_mode.is_null() {
        return 0;
    }
    let fixed = unsafe { panel_fixed_mode(connector, adjusted_mode) };
    if !fixed.is_null() {
        unsafe { tk_drm_upstream::drm_modes_upstream::drm_mode_copy(adjusted_mode, fixed) };
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_add_edid_fixed_modes(
    connector: *mut IntelConnector,
    use_alt_fixed_modes: bool,
) {
    if connector.is_null() {
        return;
    }
    let probed = unsafe { ptr::addr_of_mut!((*connector).base.probed_modes) };
    let fixed = unsafe { ptr::addr_of_mut!((*connector).panel.fixed_modes) };
    if probed.is_null() || fixed.is_null() {
        return;
    }
    let mut node = unsafe { (*probed).next };
    let mut preferred: *mut DrmDisplayMode = ptr::null_mut();
    while node != probed {
        let mode = mode_from_head(node);
        if unsafe { (*mode).r#type & DRM_MODE_TYPE_PREFERRED != 0 } {
            preferred = mode;
            break;
        }
        node = unsafe { (*node).next };
    }
    if preferred.is_null() {
        preferred = unsafe { next_mode(probed) };
    }
    if !preferred.is_null() {
        unsafe {
            (*preferred).r#type |= DRM_MODE_TYPE_PREFERRED;
            tk_linux_kapi::list::list_move_tail(ptr::addr_of_mut!((*preferred).head), fixed);
        }
    }
    if use_alt_fixed_modes && !preferred.is_null() {
        let mut node = unsafe { (*probed).next };
        while node != probed {
            let next = unsafe { (*node).next };
            let candidate = mode_from_head(node);
            if unsafe {
                (*candidate).hdisplay == (*preferred).hdisplay
                    && (*candidate).vdisplay == (*preferred).vdisplay
                    && (*candidate).flags == (*preferred).flags
            } {
                unsafe { tk_linux_kapi::list::list_move_tail(node, fixed) };
            }
            node = next;
        }
    }
    let mut node = unsafe { (*probed).next };
    while node != probed {
        let next = unsafe { (*node).next };
        let mode = mode_from_head(node);
        unsafe {
            tk_linux_kapi::list::list_del_init(node);
            tk_drm_upstream::drm_modes_upstream::drm_mode_destroy((*connector).base.dev, mode);
        }
        node = next;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_add_vbt_lfp_fixed_mode(connector: *mut IntelConnector) {
    if connector.is_null() {
        return;
    }
    let display = unsafe { display_from_connector(connector) };
    let source = unsafe { (*connector).panel.vbt.lfp_vbt_mode };
    if display.is_null() || source.is_null() {
        return;
    }
    let mode =
        unsafe { tk_drm_upstream::drm_modes_upstream::drm_mode_duplicate((*display).drm, source) };
    unsafe { add_fixed_mode(connector, mode) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_add_vbt_sdvo_fixed_mode(connector: *mut IntelConnector) {
    if connector.is_null() {
        return;
    }
    let display = unsafe { display_from_connector(connector) };
    let source = unsafe { (*connector).panel.vbt.sdvo_lvds_vbt_mode };
    if display.is_null() || source.is_null() {
        return;
    }
    let mode =
        unsafe { tk_drm_upstream::drm_modes_upstream::drm_mode_duplicate((*display).drm, source) };
    unsafe { add_fixed_mode(connector, mode) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_add_encoder_fixed_mode(
    _connector: *mut IntelConnector,
    _encoder: *mut IntelEncoder,
) {
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_prepare(
    _crtc_state: *const crate::intel_display_types_upstream::IntelCrtcState,
    _conn_state: *const tk_drm_upstream::drm_connector_types::DrmConnectorState,
) {
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_unprepare(
    _old_conn_state: *const tk_drm_upstream::drm_connector_types::DrmConnectorState,
) {
}

// Upstream defines this in intel_display.c, adjacent to panel code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_panel_sanitize_ssc(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    use crate::intel_pch_types_upstream::{PCH_CPT, PCH_IBX};
    if unsafe { (*display).pch_type } != PCH_IBX && unsafe { (*display).pch_type } != PCH_CPT {
        return;
    }
    let bios_enabled = unsafe {
        crate::intel_de_upstream::intel_de_read(
            display,
            crate::intel_display_regs_upstream::PCH_DREF_CONTROL,
        ) & crate::intel_display_regs_upstream::DREF_SSC1_ENABLE
            != 0
    };
    let vbt = unsafe { &mut (*display).vbt };
    if bios_enabled {
        vbt._bitfields_2[0] |= 1 << 2;
    } else {
        vbt._bitfields_2[0] &= !(1 << 2);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ssc_disable_quirk_bit_matches_upstream_enum_position() {
        assert_eq!(super::QUIRK_LVDS_SSC_DISABLE, 1 << 4);
    }
}
