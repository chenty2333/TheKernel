// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_client_modeset.c` (batch B2c).

#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use alloc::{vec, vec::Vec};
use core::mem::offset_of;

use tk_linux_kapi::config::{EINVAL, ENOMEM, IS_ERR, PTR_ERR};
use tk_linux_kapi::memory::{kfree, kzalloc_objs};
#[cfg(not(test))]
use tk_linux_kapi::mutex::{mutex_destroy, mutex_init, mutex_lock, mutex_unlock};
#[cfg(test)]
unsafe fn mutex_destroy<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_init<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_lock<M>(_mutex: M) {}
#[cfg(test)]
unsafe fn mutex_unlock<M>(_mutex: M) {}

use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_client_types::DrmClientDev;
use crate::drm_connector_types::{
    DrmCmdlineMode, DrmConnector, DrmConnectorListIter, CONNECTOR_STATUS_CONNECTED,
    CONNECTOR_STATUS_DISCONNECTED,
};
use crate::drm_connector_types::{
    DRM_MODE_PANEL_ORIENTATION_BOTTOM_UP, DRM_MODE_PANEL_ORIENTATION_LEFT_UP,
    DRM_MODE_PANEL_ORIENTATION_RIGHT_UP,
};
use crate::drm_crtc_types::{DrmCrtc, DrmModeSet};
use crate::drm_device_types::DrmDevice;
use crate::drm_encoder_types::DrmEncoder;
use crate::drm_modes_types::DrmDisplayMode;
use crate::drm_modeset_lock_types::DrmModesetAcquireCtx;
use crate::drm_plane_types::{DrmPlane, DRM_PLANE_TYPE_PRIMARY};
use crate::drm_property_types::DrmProperty;
use crate::uapi_drm_consts::{
    DRM_MODE_DPMS_ON, DRM_MODE_FLAG_INTERLACE,
    DRM_MODE_ROTATE_0, DRM_MODE_ROTATE_180, DRM_MODE_ROTATE_270, DRM_MODE_ROTATE_90,
    DRM_MODE_ROTATE_MASK, DRM_MODE_TYPE_PREFERRED, DRM_MODE_TYPE_USERDEF,
};

const DRM_CLIENT_MAX_CLONED_CONNECTORS: usize = 8;

#[derive(Clone, Copy, Default)]
struct ClientOffset {
    x: i32,
    y: i32,
}

#[inline]
unsafe fn mode_set_at(client: *mut DrmClientDev, index: usize) -> *mut DrmModeSet {
    unsafe { (*client).modesets.add(index) }
}

unsafe fn drm_client_for_each_modeset(
    client: *mut DrmClientDev,
    mut callback: impl FnMut(*mut DrmModeSet),
) {
    if client.is_null() || unsafe { (*client).modesets.is_null() } {
        return;
    }
    let mut i = 0;
    loop {
        let modeset = unsafe { mode_set_at(client, i) };
        if unsafe { (*modeset).crtc.is_null() } {
            break;
        }
        callback(modeset);
        i += 1;
    }
}

// upstream: drm_client_modeset.c drm_client_modeset_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_modeset_create(client: *mut DrmClientDev) -> i32 {
    if client.is_null() || unsafe { (*client).dev.is_null() } {
        return -EINVAL;
    }
    unsafe {
        let dev = (*client).dev;
        let num_crtc = (*dev).mode_config.num_crtc.max(0) as usize;
        let mut max_connector_count = 1usize;
        let modesets = kzalloc_objs::<DrmModeSet, _>(num_crtc.saturating_add(1));
        if modesets.is_null() {
            return -ENOMEM;
        }
        (*client).modesets = modesets;
        mutex_init(core::ptr::addr_of_mut!((*client).modeset_mutex));

        let head = core::ptr::addr_of_mut!((*dev).mode_config.crtc_list);
        let mut node = (*head).next;
        let mut i = 0usize;
        while !core::ptr::eq(node, head) && i < num_crtc {
            let crtc = node.cast::<u8>().sub(offset_of!(DrmCrtc, head)).cast::<DrmCrtc>();
            (*modesets.add(i)).crtc = crtc;
            node = (*node).next;
            i += 1;
        }

        // Cloning is supported only with a single CRTC, and Linux reserves
        // eight connector slots for that pipeline.
        if num_crtc == 1 {
            max_connector_count = DRM_CLIENT_MAX_CLONED_CONNECTORS;
        }
        for index in 0..i {
            let modeset = modesets.add(index);
            let connectors = kzalloc_objs::<*mut DrmConnector, _>(max_connector_count);
            if connectors.is_null() {
                drm_client_modeset_free(client);
                return -ENOMEM;
            }
            (*modeset).connectors = connectors;
        }
    }
    0
}

unsafe fn drm_client_modeset_release(client: *mut DrmClientDev) {
    unsafe {
        drm_client_for_each_modeset(client, |modeset| {
            if !(*modeset).mode.is_null() {
                crate::drm_modes_upstream::drm_mode_destroy((*client).dev, (*modeset).mode);
                (*modeset).mode = core::ptr::null_mut();
            }
            (*modeset).fb = core::ptr::null_mut();
            for j in 0..(*modeset).num_connectors {
                let connector = *(*modeset).connectors.add(j);
                if !connector.is_null() {
                    crate::drm_connector_upstream::drm_connector_put(connector);
                    *(*modeset).connectors.add(j) = core::ptr::null_mut();
                }
            }
            (*modeset).num_connectors = 0;
        });
    }
}

// upstream: drm_client_modeset.c drm_client_modeset_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_modeset_free(client: *mut DrmClientDev) {
    if client.is_null() || unsafe { (*client).modesets.is_null() } {
        return;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*client).modeset_mutex));
        drm_client_modeset_release(client);
        drm_client_for_each_modeset(client, |modeset| {
            if !(*modeset).connectors.is_null() {
                kfree((*modeset).connectors);
                (*modeset).connectors = core::ptr::null_mut();
            }
        });
        mutex_unlock(core::ptr::addr_of_mut!((*client).modeset_mutex));
        mutex_destroy(&mut (*client).modeset_mutex);
        kfree((*client).modesets);
        (*client).modesets = core::ptr::null_mut();
    }
}

unsafe fn drm_client_find_modeset(client: *mut DrmClientDev, crtc: *mut DrmCrtc) -> *mut DrmModeSet {
    let mut found = core::ptr::null_mut();
    unsafe {
        drm_client_for_each_modeset(client, |modeset| {
            if (*modeset).crtc == crtc {
                found = modeset;
            }
        });
    }
    found
}

unsafe fn mode_at(list_node: *mut tk_linux_kapi::types::ListHead) -> *mut DrmDisplayMode {
    unsafe { list_node.cast::<u8>().sub(offset_of!(DrmDisplayMode, head)).cast() }
}

unsafe fn connector_first_mode(connector: *mut DrmConnector) -> *mut DrmDisplayMode {
    unsafe {
        let head = core::ptr::addr_of_mut!((*connector).modes);
        if core::ptr::eq((*head).next, head) {
            core::ptr::null_mut()
        } else {
            mode_at((*head).next)
        }
    }
}

unsafe fn connector_preferred_mode(
    connector: *mut DrmConnector,
    width: u32,
    height: u32,
) -> *mut DrmDisplayMode {
    unsafe {
        let head = core::ptr::addr_of_mut!((*connector).modes);
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let mode = mode_at(node);
            if (*mode).hdisplay as u32 <= width
                && (*mode).vdisplay as u32 <= height
                && ((*mode).r#type as u32 & DRM_MODE_TYPE_PREFERRED) != 0
            {
                return mode;
            }
            node = (*node).next;
        }
    }
    core::ptr::null_mut()
}

unsafe fn connector_cmdline_mode(connector: *mut DrmConnector) -> *mut DrmDisplayMode {
    unsafe {
        let head = core::ptr::addr_of_mut!((*connector).modes);
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let mode = mode_at(node);
            if ((*mode).r#type as u32 & DRM_MODE_TYPE_USERDEF) != 0 {
                return mode;
            }
            node = (*node).next;
        }

        let cmdline: *mut DrmCmdlineMode = core::ptr::addr_of_mut!((*connector).cmdline_mode);
        if !(*cmdline).specified {
            return core::ptr::null_mut();
        }
        let mut pass_interlace = !(*cmdline).interlace;
        loop {
            node = (*head).next;
            while !core::ptr::eq(node, head) {
                let mode = mode_at(node);
                if (*mode).hdisplay as i32 == (*cmdline).xres
                    && (*mode).vdisplay as i32 == (*cmdline).yres
                    && (!(*cmdline).refresh_specified
                        || crate::drm_modes_upstream::drm_mode_vrefresh(mode) == (*cmdline).refresh)
                    && ((!(*cmdline).interlace && !pass_interlace)
                        || ((*mode).flags & DRM_MODE_FLAG_INTERLACE != 0) == (*cmdline).interlace)
                {
                    return mode;
                }
                node = (*node).next;
            }
            if !pass_interlace {
                break;
            }
            pass_interlace = false;
        }
    }
    core::ptr::null_mut()
}

unsafe fn connector_tiled_mode(connector: *mut DrmConnector) -> *mut DrmDisplayMode {
    unsafe {
        let head = core::ptr::addr_of_mut!((*connector).modes);
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let mode = mode_at(node);
            if (*mode).hdisplay == (*connector).tile_h_size
                && (*mode).vdisplay == (*connector).tile_v_size
            {
                return mode;
            }
            node = (*node).next;
        }
    }
    core::ptr::null_mut()
}

unsafe fn connector_fallback_non_tiled_mode(connector: *mut DrmConnector) -> *mut DrmDisplayMode {
    unsafe {
        let head = core::ptr::addr_of_mut!((*connector).modes);
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let mode = mode_at(node);
            if (*mode).hdisplay != (*connector).tile_h_size
                || (*mode).vdisplay != (*connector).tile_v_size
            {
                return mode;
            }
            node = (*node).next;
        }
    }
    core::ptr::null_mut()
}

unsafe fn mode_replace(dev: *mut DrmDevice, dst: &mut *mut DrmDisplayMode, src: *mut DrmDisplayMode) {
    unsafe {
        if !(*dst).is_null() {
            crate::drm_modes_upstream::drm_mode_destroy(dev, *dst);
        }
        *dst = if src.is_null() {
            core::ptr::null_mut()
        } else {
            crate::drm_modes_upstream::drm_mode_duplicate(dev, src)
        };
    }
}

unsafe fn modes_destroy(dev: *mut DrmDevice, modes: &mut [*mut DrmDisplayMode]) {
    for mode in modes {
        unsafe { mode_replace(dev, mode, core::ptr::null_mut()) };
    }
}

unsafe fn drm_client_connectors_enabled(connectors: &[*mut DrmConnector], enabled: &mut [bool]) {
    let mut any_connected = false;
    for (i, connector) in connectors.iter().copied().enumerate() {
        enabled[i] = unsafe {
            (*connector).status == CONNECTOR_STATUS_CONNECTED
                && !(*connector).display_info.non_desktop
        };
        any_connected |= enabled[i];
    }
    if !any_connected {
        for (i, connector) in connectors.iter().copied().enumerate() {
            enabled[i] = unsafe {
                (*connector).status != CONNECTOR_STATUS_DISCONNECTED
                    && !(*connector).display_info.non_desktop
            };
        }
    }
}

unsafe fn target_cloned(
    dev: *mut DrmDevice,
    connectors: &[*mut DrmConnector],
    modes: &mut [*mut DrmDisplayMode],
    enabled: &[bool],
) -> bool {
    unsafe {
        if (*dev).mode_config.num_crtc > 1 || enabled.iter().filter(|e| **e).count() <= 1 {
            return false;
        }
        let mut ok = true;
        for i in 0..connectors.len() {
            if !enabled[i] {
                continue;
            }
            mode_replace(dev, &mut modes[i], connector_cmdline_mode(connectors[i]));
            if modes[i].is_null() {
                ok = false;
                break;
            }
            for j in 0..i {
                if enabled[j]
                    && !crate::drm_modes_upstream::drm_mode_equal(modes[j], modes[i])
                {
                    ok = false;
                }
            }
        }
        if ok {
            return true;
        }
        modes_destroy(dev, modes);

        let dmt = crate::drm_edid_upstream::drm_mode_find_dmt(dev, 1024, 768, 60, false);
        if dmt.is_null() {
            return false;
        }
        ok = true;
        for i in 0..connectors.len() {
            if !enabled[i] {
                continue;
            }
            let mut node = (*connectors[i]).modes.next;
            let head = core::ptr::addr_of_mut!((*connectors[i]).modes);
            while !core::ptr::eq(node, head) {
                let mode = mode_at(node);
                if crate::drm_modes_upstream::drm_mode_equal(dmt, mode) {
                    mode_replace(dev, &mut modes[i], mode);
                    break;
                }
                node = (*node).next;
            }
            if modes[i].is_null() {
                ok = false;
            }
        }
        crate::drm_modes_upstream::drm_mode_destroy(dev, dmt);
        ok
    }
}

unsafe fn target_preferred(
    dev: *mut DrmDevice,
    connectors: &[*mut DrmConnector],
    modes: &mut [*mut DrmDisplayMode],
    offsets: &mut [ClientOffset],
    enabled: &[bool],
    width: u32,
    height: u32,
) -> bool {
    let mut tiled_count = 0usize;
    for c in connectors.iter().copied() {
        unsafe {
            if (*c).has_tile && (*c).status == CONNECTOR_STATUS_CONNECTED {
                tiled_count += 1;
            }
        }
    }
    let mut configured = vec![false; connectors.len()];
    for pass in 0..=connectors.len() {
        let mut progress = false;
        for (i, connector) in connectors.iter().copied().enumerate() {
            if configured[i] || !enabled[i] {
                configured[i] = true;
                continue;
            }
            unsafe {
                if (*connector).has_tile {
                    let h = (*connector).tile_h_loc;
                    let v = (*connector).tile_v_loc;
                    if pass == 0 || (pass == 1 && (h != 0 || v != 0)) {
                        continue;
                    }
                    if pass > 1 && h != (pass - 1) as u8 && v != (pass - 1) as u8 {
                        continue;
                    }
                    for prior in connectors.iter().copied() {
                        if (*prior).has_tile
                            && ((*prior).tile_h_loc < h || (*prior).tile_v_loc < v)
                        {
                            if let Some(prior_mode) = modes.get(
                                connectors.iter().position(|c| *c == prior).unwrap_or(usize::MAX),
                            ) {
                                if !prior_mode.is_null() {
                                    if (*prior).tile_h_loc < h {
                                        offsets[i].x += (**prior_mode).hdisplay as i32;
                                    }
                                    if (*prior).tile_v_loc < v {
                                        offsets[i].y += (**prior_mode).vdisplay as i32;
                                    }
                                }
                            }
                        }
                    }
                } else if pass > 0 {
                    configured[i] = true;
                    continue;
                }
                mode_replace(dev, &mut modes[i], connector_cmdline_mode(connector));
                if modes[i].is_null() {
                    mode_replace(dev, &mut modes[i], connector_preferred_mode(connector, width, height));
                }
                if modes[i].is_null() {
                    mode_replace(dev, &mut modes[i], connector_first_mode(connector));
                }
                if (*connector).has_tile {
                    let complete = tiled_count
                        >= ((*connector).num_h_tile as usize * (*connector).num_v_tile as usize);
                    let tile_mode = connector_tiled_mode(connector);
                    if !complete || (tile_mode.is_null() && (*connector).tile_h_loc == 0 && (*connector).tile_v_loc == 0) {
                        mode_replace(dev, &mut modes[i], connector_fallback_non_tiled_mode(connector));
                    } else {
                        mode_replace(dev, &mut modes[i], tile_mode);
                    }
                }
            }
            configured[i] = true;
            progress = true;
        }
        if configured.iter().all(|v| *v) || !progress {
            break;
        }
    }
    modes.iter().zip(enabled).all(|(mode, enabled)| !*enabled || !mode.is_null())
}

unsafe fn connector_has_possible_crtc(connector: *mut DrmConnector, crtc: *mut DrmCrtc) -> bool {
    unsafe {
        let dev = (*connector).dev;
        let head = core::ptr::addr_of_mut!((*dev).mode_config.encoder_list);
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let encoder = node.cast::<u8>().sub(offset_of!(DrmEncoder, head)).cast::<DrmEncoder>();
            if ((*connector).possible_encoders & (1u32 << (*encoder).index)) != 0
                && ((*encoder).possible_crtcs & crate::drm_crtc_upstream::drm_crtc_mask(crtc)) != 0
            {
                return true;
            }
            node = (*node).next;
        }
    }
    false
}

fn pick_crtcs_recursive(
    client: *mut DrmClientDev,
    connectors: &[*mut DrmConnector],
    modes: &[*mut DrmDisplayMode],
    index: usize,
    assignment: &mut [*mut DrmCrtc],
    width: u32,
    height: u32,
) -> i32 {
    if index == connectors.len() {
        return 0;
    }
    assignment[index] = core::ptr::null_mut();
    let mut best_score = pick_crtcs_recursive(client, connectors, modes, index + 1, assignment, width, height);
    if modes[index].is_null() {
        return best_score;
    }
    let connector = connectors[index];
    let mut score = 1;
    unsafe {
        if (*connector).status == CONNECTOR_STATUS_CONNECTED {
            score += 1;
        }
        if (*connector).cmdline_mode.specified {
            score += 1;
        }
        if !connector_preferred_mode(connector, width, height).is_null() {
            score += 1;
        }
        let mut best_assignment = assignment.to_vec();
        drm_client_for_each_modeset(client, |modeset| {
            let crtc = unsafe { (*modeset).crtc };
            if !unsafe { connector_has_possible_crtc(connector, crtc) } {
                return;
            }
            for prev in 0..index {
                if assignment[prev] == crtc {
                    if unsafe { (*(*client).dev).mode_config.num_crtc } > 1
                        || !unsafe { crate::drm_modes_upstream::drm_mode_equal(modes[prev], modes[index]) }
                    {
                        return;
                    }
                }
            }
            let mut candidate = assignment.to_vec();
            candidate[index] = crtc;
            let child_score = pick_crtcs_recursive(client, connectors, modes, index + 1, &mut candidate, width, height);
            let total = score + child_score;
            if total > best_score {
                best_score = total;
                best_assignment = candidate;
            }
        });
        assignment.copy_from_slice(&best_assignment);
    }
    best_score
}

unsafe fn drm_client_firmware_config(
    client: *mut DrmClientDev,
    connectors: &[*mut DrmConnector],
    modes: &mut [*mut DrmDisplayMode],
    crtcs: &mut [*mut DrmCrtc],
    enabled: &[bool],
    width: u32,
    height: u32,
) -> bool {
    unsafe {
        let dev = (*client).dev;
        if !crate::drm_color_mgmt_upstream::drm_drv_uses_atomic_modeset(dev) {
            return false;
        }
        let mut ctx: DrmModesetAcquireCtx = core::mem::zeroed();
        crate::drm_modeset_lock_upstream::drm_modeset_acquire_init(&mut ctx, 0);
        while crate::drm_modeset_lock_upstream::drm_modeset_lock_all_ctx(dev, &mut ctx) != 0 {
            crate::drm_modeset_lock_upstream::drm_modeset_backoff(&mut ctx);
        }
        let mut count = 0usize;
        let mut fallback = false;
        for i in 0..connectors.len() {
            if !enabled[i] {
                continue;
            }
            let connector = connectors[i];
            let state = (*connector).state;
            if state.is_null() || (*state).crtc.is_null() || (*state).best_encoder.is_null() {
                fallback = true;
                continue;
            }
            let crtc = (*state).crtc;
            if (0..i).any(|j| crtcs[j] == crtc) {
                fallback = true;
                continue;
            }
            let crtc_state = (*crtc).state;
            let current = if !crtc_state.is_null() && (*crtc_state).active {
                &mut (*crtc_state).mode as *mut DrmDisplayMode
            } else {
                core::ptr::null_mut()
            };
            let mode = if !current.is_null() { current } else { connector_cmdline_mode(connector) };
            mode_replace(dev, &mut modes[i], mode);
            if modes[i].is_null() {
                mode_replace(dev, &mut modes[i], connector_preferred_mode(connector, width, height));
            }
            if modes[i].is_null() {
                mode_replace(dev, &mut modes[i], connector_first_mode(connector));
            }
            if modes[i].is_null() {
                fallback = true;
                continue;
            }
            crtcs[i] = crtc;
            count += 1;
        }
        crate::drm_modeset_lock_upstream::drm_modeset_drop_locks(&mut ctx);
        crate::drm_modeset_lock_upstream::drm_modeset_acquire_fini(&mut ctx);
        count > 0 && !fallback
    }
}

// upstream: drm_client_modeset.c drm_client_modeset_probe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_modeset_probe(
    client: *mut DrmClientDev,
    mut width: u32,
    mut height: u32,
) -> i32 {
    if client.is_null() || unsafe { (*client).dev.is_null() } {
        return -EINVAL;
    }
    let dev = unsafe { (*client).dev };
    if width == 0 {
        width = unsafe { (*dev).mode_config.max_width };
    }
    if height == 0 {
        height = unsafe { (*dev).mode_config.max_height };
    }

    let mut connectors = Vec::<*mut DrmConnector>::new();
    let mut iter: DrmConnectorListIter = unsafe { core::mem::zeroed() };
    unsafe { crate::drm_connector_upstream::drm_connector_list_iter_begin(dev, &mut iter) };
    loop {
        let connector = unsafe { crate::drm_connector_upstream::drm_connector_list_iter_next(&mut iter) };
        if connector.is_null() {
            break;
        }
        if connectors.try_reserve(1).is_err() {
            unsafe { crate::drm_connector_upstream::drm_connector_list_iter_end(&mut iter) };
            for connector in connectors.iter().copied() {
                unsafe { crate::drm_connector_upstream::drm_connector_put(connector) };
            }
            return -ENOMEM;
        }
        unsafe { crate::drm_connector_upstream::drm_connector_get(connector) };
        connectors.push(connector);
    }
    unsafe { crate::drm_connector_upstream::drm_connector_list_iter_end(&mut iter) };
    if connectors.is_empty() {
        return 0;
    }

    let count = connectors.len();
    let mut modes = vec![core::ptr::null_mut(); count];
    let mut crtcs = vec![core::ptr::null_mut(); count];
    let mut offsets = vec![ClientOffset::default(); count];
    let mut enabled = vec![false; count];

    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*client).modeset_mutex));
        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.mutex));
        for connector in connectors.iter().copied() {
            if !(*connector).funcs.is_null() {
                if let Some(fill_modes) = (*(*connector).funcs).fill_modes {
                    let _ = fill_modes(connector, width, height);
                }
            }
        }
        drm_client_connectors_enabled(&connectors, &mut enabled);

        if !drm_client_firmware_config(client, &connectors, &mut modes, &mut crtcs, &enabled, width, height) {
            modes_destroy(dev, &mut modes);
            crtcs.fill(core::ptr::null_mut());
            offsets.fill(ClientOffset::default());
            if !target_cloned(dev, &connectors, &mut modes, &enabled)
                && !target_preferred(dev, &connectors, &mut modes, &mut offsets, &enabled, width, height)
            {
                modes_destroy(dev, &mut modes);
            }
            let mut assignment = vec![core::ptr::null_mut(); count];
            pick_crtcs_recursive(client, &connectors, &modes, 0, &mut assignment, width, height);
            crtcs.copy_from_slice(&assignment);
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.mutex));

        drm_client_modeset_release(client);
        for i in 0..count {
            if modes[i].is_null() || crtcs[i].is_null() {
                continue;
            }
            let modeset = drm_client_find_modeset(client, crtcs[i]);
            if modeset.is_null() || (*modeset).connectors.is_null() {
                continue;
            }
            let max_connectors = if (*dev).mode_config.num_crtc == 1 { DRM_CLIENT_MAX_CLONED_CONNECTORS } else { 1 };
            if (*modeset).num_connectors >= max_connectors
                || ((*dev).mode_config.num_crtc > 1 && (*modeset).num_connectors >= 1)
            {
                mutex_unlock(core::ptr::addr_of_mut!((*client).modeset_mutex));
                modes_destroy(dev, &mut modes);
                for connector in connectors.iter().copied() {
                    crate::drm_connector_upstream::drm_connector_put(connector);
                }
                return -EINVAL;
            }
            if !(*modeset).mode.is_null() {
                crate::drm_modes_upstream::drm_mode_destroy(dev, (*modeset).mode);
            }
            (*modeset).mode = crate::drm_modes_upstream::drm_mode_duplicate(dev, modes[i]);
            if (*modeset).mode.is_null() {
                mutex_unlock(core::ptr::addr_of_mut!((*client).modeset_mutex));
                modes_destroy(dev, &mut modes);
                for connector in connectors.iter().copied() {
                    crate::drm_connector_upstream::drm_connector_put(connector);
                }
                return -ENOMEM;
            }
            crate::drm_connector_upstream::drm_connector_get(connectors[i]);
            *(*modeset).connectors.add((*modeset).num_connectors) = connectors[i];
            (*modeset).num_connectors += 1;
            (*modeset).x = offsets[i].x.max(0) as u32;
            (*modeset).y = offsets[i].y.max(0) as u32;
        }
        mutex_unlock(core::ptr::addr_of_mut!((*client).modeset_mutex));
    }

    unsafe {
        modes_destroy(dev, &mut modes);
        for connector in connectors {
            crate::drm_connector_upstream::drm_connector_put(connector);
        }
    }
    0
}

// upstream: drm_client_modeset.c drm_client_rotation()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_rotation(
    modeset: *mut DrmModeSet,
    rotation: *mut u32,
) -> bool {
    if modeset.is_null() || rotation.is_null() || unsafe { (*modeset).num_connectors == 0 } {
        return false;
    }
    unsafe {
        let connector = *(*modeset).connectors;
        let plane = (*(*modeset).crtc).primary;
        if connector.is_null() || plane.is_null() {
            return false;
        }
        *rotation = match (*connector).display_info.panel_orientation {
            DRM_MODE_PANEL_ORIENTATION_BOTTOM_UP => DRM_MODE_ROTATE_180,
            DRM_MODE_PANEL_ORIENTATION_LEFT_UP => DRM_MODE_ROTATE_90,
            DRM_MODE_PANEL_ORIENTATION_RIGHT_UP => DRM_MODE_ROTATE_270,
            _ => DRM_MODE_ROTATE_0,
        };
        let cmdline = core::ptr::addr_of!((*connector).cmdline_mode);
        if (*cmdline).specified && (*cmdline).rotation_reflection != 0 {
            let panel_rot = (*rotation & DRM_MODE_ROTATE_MASK).trailing_zeros();
            let cmdline_rot = ((*cmdline).rotation_reflection & DRM_MODE_ROTATE_MASK).trailing_zeros();
            let sum_rot = (panel_rot + cmdline_rot) % 4;
            let panel_rest = *rotation & !DRM_MODE_ROTATE_MASK;
            let cmdline_rest = (*cmdline).rotation_reflection & !DRM_MODE_ROTATE_MASK;
            *rotation = (1 << sum_rot) | (panel_rest ^ cmdline_rest);
        }
        if ((*rotation & DRM_MODE_ROTATE_MASK) != DRM_MODE_ROTATE_0
            && (*rotation & DRM_MODE_ROTATE_MASK) != DRM_MODE_ROTATE_180)
            || (*plane).rotation_property.is_null()
        {
            return false;
        }
        let property: *mut DrmProperty = (*plane).rotation_property;
        let mut valid_mask = 0u64;
        for i in 0..(*property).num_values as usize {
            valid_mask |= 1u64 << *(*property).values.add(i);
        }
        (*rotation as u64 & valid_mask) != 0
    }
}

unsafe fn drm_client_modeset_commit_atomic(client: *mut DrmClientDev, active: bool, check: bool) -> i32 {
    let dev = unsafe { (*client).dev };
    let mut ctx: DrmModesetAcquireCtx = unsafe { core::mem::zeroed() };
    unsafe { crate::drm_modeset_lock_upstream::drm_modeset_acquire_init(&mut ctx, 0) };
    let state: *mut DrmAtomicCommit = unsafe { crate::drm_atomic_upstream::drm_atomic_commit_alloc(dev) };
    if state.is_null() {
        unsafe {
            crate::drm_modeset_lock_upstream::drm_modeset_acquire_fini(&mut ctx);
        }
        return -ENOMEM;
    }
    unsafe { (*state).acquire_ctx = &mut ctx };

    let ret = loop {
        let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.plane_list) };
        let mut node = unsafe { (*head).next };
        let mut ret = 0;
        while !core::ptr::eq(node, head) {
            let plane = unsafe { node.cast::<u8>().sub(offset_of!(DrmPlane, head)).cast::<DrmPlane>() };
            let plane_state = unsafe { crate::drm_atomic_upstream::drm_atomic_get_plane_state(state, plane) };
            if IS_ERR(plane_state) {
                ret = PTR_ERR(plane_state);
                break;
            }
            unsafe { (*plane_state).rotation = DRM_MODE_ROTATE_0 };
            if unsafe { (*plane).r#type != DRM_PLANE_TYPE_PRIMARY } {
                ret = unsafe { crate::drm_atomic_upstream::__drm_atomic_helper_disable_plane(plane, plane_state) };
                if ret != 0 {
                    break;
                }
            }
            node = unsafe { (*node).next };
        }
        if ret == 0 {
            let mut mode_index = 0usize;
            loop {
                let modeset = unsafe { mode_set_at(client, mode_index) };
                if unsafe { (*modeset).crtc.is_null() } {
                    break;
                }
                let primary = unsafe { (*(*modeset).crtc).primary };
                let mut rotation = DRM_MODE_ROTATE_0;
                if unsafe { drm_client_rotation(modeset, &mut rotation) } {
                    let plane_state = unsafe { crate::drm_atomic_upstream::drm_atomic_get_new_plane_state(state, primary) };
                    if plane_state.is_null() {
                        ret = -EINVAL;
                        break;
                    }
                    unsafe { (*plane_state).rotation = rotation };
                }
                ret = unsafe { crate::drm_atomic_upstream::__drm_atomic_helper_set_config(modeset, state) };
                if ret != 0 {
                    break;
                }
                if !active {
                    let crtc_state = unsafe { crate::drm_atomic_upstream::drm_atomic_get_new_crtc_state(state, (*modeset).crtc) };
                    if crtc_state.is_null() {
                        ret = -EINVAL;
                        break;
                    }
                    unsafe { (*crtc_state).active = false };
                }
                mode_index += 1;
            }
        }
        if ret == 0 {
            ret = unsafe {
                if check {
                    crate::drm_atomic_upstream::drm_atomic_check_only(state)
                } else {
                    crate::drm_atomic_upstream::drm_atomic_commit(state)
                }
            };
        }
        if ret == -tk_linux_kapi::config::EDEADLK {
            unsafe {
                crate::drm_atomic_upstream::drm_atomic_commit_clear(state);
                crate::drm_modeset_lock_upstream::drm_modeset_backoff(&mut ctx);
            }
            continue;
        }
        break ret;
    };

    unsafe {
        crate::drm_atomic_upstream::drm_atomic_commit_put(state);
        crate::drm_modeset_lock_upstream::drm_modeset_drop_locks(&mut ctx);
        crate::drm_modeset_lock_upstream::drm_modeset_acquire_fini(&mut ctx);
    }
    ret
}

unsafe fn drm_client_modeset_commit_legacy(client: *mut DrmClientDev) -> i32 {
    let dev = unsafe { (*client).dev };
    unsafe { crate::drm_modeset_lock_upstream::drm_modeset_lock_all(dev) };
    let mut ret = 0;
    unsafe {
        let head = core::ptr::addr_of_mut!((*dev).mode_config.plane_list);
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let plane = node.cast::<u8>().sub(offset_of!(DrmPlane, head)).cast::<DrmPlane>();
            if (*plane).r#type != DRM_PLANE_TYPE_PRIMARY {
                crate::drm_plane_upstream::drm_plane_force_disable(plane);
            }
            if !(*plane).rotation_property.is_null() {
                crate::drm_plane_upstream::drm_mode_plane_set_obj_prop(
                    plane,
                    (*plane).rotation_property,
                    DRM_MODE_ROTATE_0 as u64,
                );
            }
            node = (*node).next;
        }
        let mut mode_index = 0usize;
        loop {
            let modeset = mode_set_at(client, mode_index);
            if (*modeset).crtc.is_null() {
                break;
            }
            let crtc = (*modeset).crtc;
            let funcs = (*crtc).funcs;
            if !funcs.is_null() {
                if let Some(cursor_set2) = (*funcs).cursor_set2 {
                    ret = cursor_set2(crtc, core::ptr::null_mut(), 0, 0, 0, 0, 0);
                } else if let Some(cursor_set) = (*funcs).cursor_set {
                    ret = cursor_set(crtc, core::ptr::null_mut(), 0, 0, 0);
                }
            }
            if ret != 0 {
                break;
            }
            ret = crate::drm_crtc_upstream::drm_mode_set_config_internal(modeset);
            if ret != 0 {
                break;
            }
            mode_index += 1;
        }
        crate::drm_modeset_lock_upstream::drm_modeset_unlock_all(dev);
    }
    ret
}

// upstream: drm_client_modeset.c drm_client_modeset_check()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_modeset_check(client: *mut DrmClientDev) -> i32 {
    if client.is_null() || unsafe { (*client).dev.is_null() } {
        return -EINVAL;
    }
    let dev = unsafe { (*client).dev };
    if !unsafe { crate::drm_color_mgmt_upstream::drm_drv_uses_atomic_modeset(dev) } {
        return 0;
    }
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*client).modeset_mutex));
        let ret = drm_client_modeset_commit_atomic(client, true, true);
        mutex_unlock(core::ptr::addr_of_mut!((*client).modeset_mutex));
        ret
    }
}

// upstream: drm_client_modeset.c drm_client_modeset_commit_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_modeset_commit_locked(client: *mut DrmClientDev) -> i32 {
    if client.is_null() || unsafe { (*client).dev.is_null() } {
        return -EINVAL;
    }
    unsafe {
        let dev = (*client).dev;
        mutex_lock(core::ptr::addr_of_mut!((*client).modeset_mutex));
        let ret = if crate::drm_color_mgmt_upstream::drm_drv_uses_atomic_modeset(dev) {
            drm_client_modeset_commit_atomic(client, true, false)
        } else {
            drm_client_modeset_commit_legacy(client)
        };
        mutex_unlock(core::ptr::addr_of_mut!((*client).modeset_mutex));
        ret
    }
}

// upstream: drm_client_modeset.c drm_client_modeset_commit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_modeset_commit(client: *mut DrmClientDev) -> i32 {
    if client.is_null() || unsafe { (*client).dev.is_null() } {
        return -EINVAL;
    }
    unsafe {
        let dev = (*client).dev;
        if !crate::drm_auth_upstream::drm_master_internal_acquire(dev) {
            return -tk_linux_kapi::config::EBUSY;
        }
        let ret = drm_client_modeset_commit_locked(client);
        crate::drm_auth_upstream::drm_master_internal_release(dev);
        ret
    }
}

// upstream: drm_client_modeset.c drm_client_modeset_dpms()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_modeset_dpms(client: *mut DrmClientDev, mode: i32) -> i32 {
    if client.is_null() || unsafe { (*client).dev.is_null() } {
        return -EINVAL;
    }
    unsafe {
        let dev = (*client).dev;
        if !crate::drm_auth_upstream::drm_master_internal_acquire(dev) {
            return -tk_linux_kapi::config::EBUSY;
        }
        mutex_lock(core::ptr::addr_of_mut!((*client).modeset_mutex));
        let ret = if crate::drm_color_mgmt_upstream::drm_drv_uses_atomic_modeset(dev) {
            drm_client_modeset_commit_atomic(client, mode == DRM_MODE_DPMS_ON as i32, false)
        } else {
            drm_client_modeset_dpms_legacy(client, mode);
            0
        };
        mutex_unlock(core::ptr::addr_of_mut!((*client).modeset_mutex));
        crate::drm_auth_upstream::drm_master_internal_release(dev);
        ret
    }
}

unsafe fn drm_client_modeset_dpms_legacy(client: *mut DrmClientDev, dpms_mode: i32) {
    let dev = unsafe { (*client).dev };
    let mut ctx: DrmModesetAcquireCtx = unsafe { core::mem::zeroed() };
    unsafe {
        crate::drm_modeset_lock_upstream::drm_modeset_acquire_init(&mut ctx, 0);
        while crate::drm_modeset_lock_upstream::drm_modeset_lock_all_ctx(dev, &mut ctx) != 0 {
            crate::drm_modeset_lock_upstream::drm_modeset_backoff(&mut ctx);
        }
        let mut index = 0usize;
        loop {
            let modeset = mode_set_at(client, index);
            if (*modeset).crtc.is_null() {
                break;
            }
            if (*modeset).crtc.is_null() || !(*(*modeset).crtc).enabled {
                index += 1;
                continue;
            }
            for i in 0..(*modeset).num_connectors {
                let connector = *(*modeset).connectors.add(i);
                if connector.is_null() {
                    continue;
                }
                if let Some(dpms) = (*(*connector).funcs).dpms {
                    dpms(connector, dpms_mode);
                }
                if !(*dev).mode_config.dpms_property.is_null() {
                    crate::drm_mode_object_upstream::drm_object_property_set_value(
                        core::ptr::addr_of_mut!((*connector).base),
                        (*dev).mode_config.dpms_property,
                        dpms_mode as u64,
                    );
                }
            }
            index += 1;
        }
        crate::drm_modeset_lock_upstream::drm_modeset_drop_locks(&mut ctx);
        crate::drm_modeset_lock_upstream::drm_modeset_acquire_fini(&mut ctx);
    }
}

// upstream: drm_client_modeset.c drm_client_modeset_wait_for_vblank()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_client_modeset_wait_for_vblank(
    client: *mut DrmClientDev,
    crtc_index: u32,
) -> i32 {
    if client.is_null() || unsafe { (*client).dev.is_null() || (*client).modesets.is_null() } {
        return -EINVAL;
    }
    unsafe {
        let dev = (*client).dev;
        if !crate::drm_auth_upstream::drm_master_internal_acquire(dev) {
            return -tk_linux_kapi::config::EBUSY;
        }
        if crtc_index >= (*dev).mode_config.num_crtc.max(0) as u32 {
            crate::drm_auth_upstream::drm_master_internal_release(dev);
            return -EINVAL;
        }
        let crtc = (*(*client).modesets.add(crtc_index as usize)).crtc;
        let ret = crate::drm_vblank_upstream::drm_crtc_vblank_get(crtc);
        if ret == 0 {
            crate::drm_vblank_upstream::drm_crtc_wait_one_vblank(crtc);
            crate::drm_vblank_upstream::drm_crtc_vblank_put(crtc);
        }
        crate::drm_auth_upstream::drm_master_internal_release(dev);
        0
    }
}

// upstream: drm_crtc_helper.c drm_helper_encoder_in_use()
unsafe fn drm_helper_encoder_in_use(encoder: *mut DrmEncoder) -> bool {
    unsafe {
        let dev = (*encoder).dev;
        let mut iter: DrmConnectorListIter = core::mem::zeroed();
        crate::drm_connector_upstream::drm_connector_list_iter_begin(dev, &mut iter);
        loop {
            let connector = crate::drm_connector_upstream::drm_connector_list_iter_next(&mut iter);
            if connector.is_null() {
                break;
            }
            if (*connector).encoder == encoder {
                crate::drm_connector_upstream::drm_connector_list_iter_end(&mut iter);
                return true;
            }
        }
        crate::drm_connector_upstream::drm_connector_list_iter_end(&mut iter);
    }
    false
}

// upstream: drm_crtc_helper.c drm_helper_crtc_in_use()
unsafe fn drm_helper_crtc_in_use(crtc: *mut DrmCrtc) -> bool {
    unsafe {
        let dev = (*crtc).dev;
        let head = core::ptr::addr_of_mut!((*dev).mode_config.encoder_list);
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let encoder = node.cast::<u8>().sub(offset_of!(DrmEncoder, head)).cast::<DrmEncoder>();
            if (*encoder).crtc == crtc && drm_helper_encoder_in_use(encoder) {
                return true;
            }
            node = (*node).next;
        }
    }
    false
}

// upstream: drm_crtc_helper.c drm_helper_disable_unused_functions()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_helper_disable_unused_functions(dev: *mut DrmDevice) {
    if dev.is_null() {
        return;
    }
    unsafe {
        crate::drm_modeset_lock_upstream::drm_modeset_lock_all(dev);
        let head = core::ptr::addr_of_mut!((*dev).mode_config.encoder_list);
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let encoder = node.cast::<u8>().sub(offset_of!(DrmEncoder, head)).cast::<DrmEncoder>();
            if !drm_helper_encoder_in_use(encoder) {
                if let Some(funcs) = (*encoder).helper_private.as_ref() {
                    if let Some(disable) = funcs.disable {
                        disable(encoder);
                    } else if let Some(dpms) = funcs.dpms {
                        dpms(encoder, 3);
                    }
                }
                (*encoder).crtc = core::ptr::null_mut();
            }
            node = (*node).next;
        }
        let head = core::ptr::addr_of_mut!((*dev).mode_config.crtc_list);
        let mut node = (*head).next;
        while !core::ptr::eq(node, head) {
            let crtc = node.cast::<u8>().sub(offset_of!(DrmCrtc, head)).cast::<DrmCrtc>();
            (*crtc).enabled = drm_helper_crtc_in_use(crtc);
            if !(*crtc).enabled {
                if let Some(funcs) = (*crtc).helper_private.as_ref() {
                    if let Some(disable) = funcs.disable {
                        disable(crtc);
                    } else if let Some(dpms) = funcs.dpms {
                        dpms(crtc, 3);
                    }
                }
                if !(*crtc).primary.is_null() {
                    (*(*crtc).primary).fb = core::ptr::null_mut();
                }
            }
            node = (*node).next;
        }
        crate::drm_modeset_lock_upstream::drm_modeset_unlock_all(dev);
    }
}
