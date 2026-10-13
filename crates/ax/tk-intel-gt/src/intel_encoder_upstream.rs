// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of drivers/gpu/drm/i915/display/intel_encoder.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ptr;

use tk_drm_upstream::{drm_device_types::DrmDevice, drm_encoder_types::DrmEncoder};
use tk_linux_kapi::{
    config::GFP_KERNEL,
    memory::kzalloc_objs,
    mutex::mutex_init,
    primitives::msecs_to_jiffies,
    types::WorkStruct,
    workqueue::{INIT_DELAYED_WORK, cancel_delayed_work_sync, mod_delayed_work},
};

use crate::{
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_device_upstream::intel_display_device_present,
    intel_display_limits_types_upstream::AUX_CH_NONE,
    intel_display_reg_defs_upstream::INVALID_MMIO_REG,
    intel_display_types_upstream::{IntelDigitalPort, IntelEncoder},
};

unsafe extern "C" {
    fn intel_hpd_unblock(encoder: *mut IntelEncoder);
    fn intel_hpd_block(encoder: *mut IntelEncoder);
    fn drm_modeset_lock_all(dev: *mut DrmDevice);
    fn drm_modeset_unlock_all(dev: *mut DrmDevice);
}

unsafe extern "C" fn intel_encoder_link_check_work_fn(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let encoder = unsafe {
        (work
            .cast::<u8>()
            .sub(core::mem::offset_of!(IntelEncoder, link_check_work)))
        .cast::<IntelEncoder>()
    };
    if let Some(callback) = unsafe { (*encoder).link_check } {
        unsafe { callback(encoder) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_link_check_init(
    encoder: *mut IntelEncoder,
    callback: Option<unsafe extern "C" fn(*mut IntelEncoder)>,
) {
    if encoder.is_null() {
        return;
    }
    unsafe {
        INIT_DELAYED_WORK(&mut (*encoder).link_check_work, |work| unsafe {
            intel_encoder_link_check_work_fn(work)
        });
        (*encoder).link_check = callback;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_link_check_flush_work(encoder: *mut IntelEncoder) {
    if !encoder.is_null() {
        unsafe { cancel_delayed_work_sync(&mut (*encoder).link_check_work) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_link_check_queue_work(
    encoder: *mut IntelEncoder,
    delay_ms: i32,
) {
    if encoder.is_null() {
        return;
    }
    let drm = unsafe { (*encoder).base.dev };
    if drm.is_null() {
        return;
    }
    let display = unsafe { __drm_to_display(drm) };
    if display.is_null() {
        return;
    }
    unsafe {
        mod_delayed_work(
            (*display).wq.unordered,
            &mut (*encoder).link_check_work,
            msecs_to_jiffies(delay_ms.max(0) as u32),
        );
    }
}

unsafe fn for_each_encoder(display: *mut IntelDisplay, mut f: impl FnMut(*mut IntelEncoder)) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let dev = unsafe { (*display).drm };
    let head = unsafe { ptr::addr_of_mut!((*dev).mode_config.encoder_list) };
    let mut node = unsafe { (*head).next };
    while node != head {
        let base = unsafe {
            node.cast::<u8>()
                .sub(core::mem::offset_of!(DrmEncoder, head))
        }
        .cast::<DrmEncoder>();
        f(base.cast::<IntelEncoder>());
        node = unsafe { (*node).next };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_unblock_all_hpds(display: *mut IntelDisplay) {
    if !unsafe { intel_display_device_present(display) } {
        return;
    }
    unsafe { for_each_encoder(display, |encoder| intel_hpd_unblock(encoder)) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_block_all_hpds(display: *mut IntelDisplay) {
    if !unsafe { intel_display_device_present(display) } {
        return;
    }
    unsafe { for_each_encoder(display, |encoder| intel_hpd_block(encoder)) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_suspend_all(display: *mut IntelDisplay) {
    if !unsafe { intel_display_device_present(display) } {
        return;
    }
    let dev = unsafe { (*display).drm };
    unsafe { drm_modeset_lock_all(dev) };
    unsafe {
        for_each_encoder(display, |encoder| {
            if let Some(suspend) = (*encoder).suspend {
                suspend(encoder);
            }
        })
    };
    unsafe { drm_modeset_unlock_all(dev) };
    unsafe {
        for_each_encoder(display, |encoder| {
            if let Some(complete) = (*encoder).suspend_complete {
                complete(encoder);
            }
        })
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_shutdown_all(display: *mut IntelDisplay) {
    if !unsafe { intel_display_device_present(display) } {
        return;
    }
    let dev = unsafe { (*display).drm };
    unsafe { drm_modeset_lock_all(dev) };
    unsafe {
        for_each_encoder(display, |encoder| {
            if let Some(shutdown) = (*encoder).shutdown {
                shutdown(encoder);
            }
        })
    };
    unsafe { drm_modeset_unlock_all(dev) };
    unsafe {
        for_each_encoder(display, |encoder| {
            if let Some(complete) = (*encoder).shutdown_complete {
                complete(encoder);
            }
        })
    };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dig_port_alloc() -> *mut IntelDigitalPort {
    let dig_port = unsafe { kzalloc_objs::<IntelDigitalPort, _>(1) };
    if dig_port.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        (*dig_port).hdmi.hdmi_reg = INVALID_MMIO_REG;
        (*dig_port).dp.output_reg = INVALID_MMIO_REG;
        (*dig_port).aux_ch = AUX_CH_NONE;
        (*dig_port).max_lanes = 4;
        mutex_init(ptr::addr_of_mut!((*dig_port).hdcp.mutex));
    }
    dig_port
}
