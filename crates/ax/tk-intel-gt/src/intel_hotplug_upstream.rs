// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of the ADL-P hotplug detection, work, and lifecycle path in
// Linux v7.2.3 drivers/gpu/drm/i915/display/intel_hotplug.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    mem::offset_of,
    sync::atomic::{AtomicBool, Ordering},
};

use tk_drm_upstream::{
    drm_connector_types::{DRM_FORCE_UNSPECIFIED, DrmConnector, DrmConnectorListIter},
    drm_device_types::DrmDevice,
    drm_encoder_types::DrmEncoder,
    drm_probe_helper_upstream::{
        drm_helper_probe_detect, drm_kms_helper_connector_hotplug_event,
        drm_kms_helper_hotplug_event, drm_kms_helper_poll_reschedule,
    },
};
use tk_linux_kapi::{
    locks::{spin_lock_irq, spin_unlock_irq},
    mutex::{mutex_lock, mutex_unlock},
    primitives::{jiffies, msecs_to_jiffies},
    types::{DelayedWork, WorkStruct},
    workqueue::{
        INIT_DELAYED_WORK, INIT_WORK_C, cancel_delayed_work_sync, cancel_work, cancel_work_sync,
        flush_work, mod_delayed_work, queue_delayed_work, queue_work,
    },
};

use crate::{
    intel_display_core_types_upstream::{IntelDisplay, IntelHotplug},
    intel_display_limits_types_upstream::{
        HPD_NONE, HPD_NUM_PINS, HPD_PORT_A, HpdPin, PORT_A, Port,
    },
    intel_display_power_types_upstream::POWER_DOMAIN_DISPLAY_CORE,
    intel_display_types_upstream::{
        INTEL_HOTPLUG_CHANGED, INTEL_HOTPLUG_RETRY, INTEL_HOTPLUG_UNCHANGED, INTEL_OUTPUT_DDI,
        INTEL_OUTPUT_DP, INTEL_OUTPUT_EDP, INTEL_OUTPUT_HDMI, IntelConnector, IntelDigitalPort,
        IntelDp, IntelEncoder,
    },
};

const HPD_DISABLED: u32 = 1;
const HPD_ENABLED: u32 = 0;
const HPD_MARK_DISABLED: u32 = 2;
const HPD_STORM_DEFAULT_THRESHOLD: u32 = 50;
const HPD_STORM_DETECT_PERIOD_MS: u32 = 1000;
const HPD_STORM_REENABLE_DELAY_MS: u32 = 2 * 60 * 1000;
const HPD_RETRY_DELAY_MS: u32 = 1000;
const DRM_CONNECTOR_POLL_CONNECT: u8 = 1 << 0;
const DRM_CONNECTOR_POLL_DISCONNECT: u8 = 1 << 1;
const DRM_CONNECTOR_POLL_HPD: u8 = 1 << 2;

unsafe extern "C" {
    fn intel_dp_dpcd_set_probe(dp: *mut IntelDp, force_on_external: bool);
}

#[inline]
unsafe fn display_has_display(display: *mut IntelDisplay) -> bool {
    !display.is_null() && unsafe { (*display).info.__runtime_info.pipe_mask != 0 }
}

#[inline]
unsafe fn display_has_dp_mst(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    let info = unsafe { (*display).info.__device_info };
    !info.is_null() && unsafe { (*info)._bitfields_46[0] & (1 << 4) != 0 }
}

#[inline]
unsafe fn hotplug_poll_enabled(display: *mut IntelDisplay) -> bool {
    let field =
        unsafe { core::ptr::addr_of!((*display).hotplug.poll_enabled).cast::<AtomicBool>() };
    unsafe { (*field).load(Ordering::Relaxed) }
}

#[inline]
unsafe fn set_hotplug_poll_enabled(display: *mut IntelDisplay, enabled: bool) {
    let field =
        unsafe { core::ptr::addr_of_mut!((*display).hotplug.poll_enabled).cast::<AtomicBool>() };
    unsafe { (*field).store(enabled, Ordering::Relaxed) };
}

#[inline]
unsafe fn ignore_long_hpd(display: *mut IntelDisplay) -> bool {
    let field =
        unsafe { core::ptr::addr_of!((*display).hotplug.ignore_long_hpd).cast::<AtomicBool>() };
    unsafe { (*field).load(Ordering::Relaxed) }
}

#[inline]
unsafe fn display_from_encoder(encoder: *mut IntelEncoder) -> *mut IntelDisplay {
    if encoder.is_null() || unsafe { (*encoder).base.dev.is_null() } {
        return core::ptr::null_mut();
    }
    unsafe { crate::intel_display_conversion_upstream::__drm_to_display((*encoder).base.dev) }
}

#[unsafe(no_mangle)]
pub extern "C" fn intel_hpd_pin_default(port: Port) -> HpdPin {
    (HPD_PORT_A as i32 + port - PORT_A) as HpdPin
}

#[inline]
fn hpd_pin_valid(pin: HpdPin) -> bool {
    (pin as usize) < HPD_NUM_PINS as usize
}

#[inline]
fn bit(pin: HpdPin) -> u32 {
    1u32.wrapping_shl(pin)
}

unsafe fn for_each_encoder(display: *mut IntelDisplay, mut f: impl FnMut(*mut IntelEncoder)) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let head = unsafe { core::ptr::addr_of_mut!((*(*display).drm).mode_config.encoder_list) };
    let mut node = unsafe { (*head).next };
    while node != head {
        let encoder = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(DrmEncoder, head))
                .cast::<IntelEncoder>()
        };
        f(encoder);
        node = unsafe { (*node).next };
    }
}

unsafe fn connector_iter_begin(dev: *mut DrmDevice, iter: &mut DrmConnectorListIter) {
    unsafe { tk_drm_upstream::drm_connector_upstream::drm_connector_list_iter_begin(dev, iter) };
}

unsafe fn connector_iter_next(iter: &mut DrmConnectorListIter) -> *mut DrmConnector {
    unsafe { tk_drm_upstream::drm_connector_upstream::drm_connector_list_iter_next(iter) }
}

unsafe fn connector_iter_end(iter: &mut DrmConnectorListIter) {
    unsafe { tk_drm_upstream::drm_connector_upstream::drm_connector_list_iter_end(iter) };
}

#[inline]
unsafe fn connector_from_base(connector: *mut DrmConnector) -> *mut IntelConnector {
    connector.cast::<IntelConnector>()
}

#[inline]
unsafe fn connector_hpd_pin(connector: *mut IntelConnector) -> HpdPin {
    if connector.is_null() {
        return HPD_NONE;
    }
    let encoder = unsafe { (*connector).encoder };
    if encoder.is_null() {
        HPD_NONE
    } else {
        unsafe { (*encoder).hpd_pin }
    }
}

#[inline]
unsafe fn connector_ref(connector: *mut DrmConnector) {
    if !connector.is_null() {
        unsafe {
            tk_drm_upstream::drm_mode_object_upstream::drm_mode_object_get(&mut (*connector).base)
        };
    }
}

#[inline]
unsafe fn connector_unref(connector: *mut DrmConnector) {
    if !connector.is_null() {
        unsafe {
            tk_drm_upstream::drm_mode_object_upstream::drm_mode_object_put(&mut (*connector).base)
        };
    }
}

#[inline]
fn in_jiffies_range(now: u64, start: u64, end: u64) -> bool {
    now.wrapping_sub(start) <= end.wrapping_sub(start)
}

fn storm_count(count: i32, long_hpd: bool) -> i32 {
    count.saturating_add(if long_hpd { 10 } else { 1 })
}

unsafe fn intel_hpd_irq_storm_detect(
    display: *mut IntelDisplay,
    pin: HpdPin,
    long_hpd: bool,
) -> bool {
    if !hpd_pin_valid(pin) {
        return false;
    }
    let hpd = unsafe { &mut (*display).hotplug };
    let threshold = hpd.hpd_storm_threshold;
    if threshold == 0 || (!long_hpd && hpd.hpd_short_storm_enabled == 0) {
        return false;
    }

    let now = jiffies();
    let start = hpd.stats[pin as usize].last_jiffies;
    let end = start.wrapping_add(msecs_to_jiffies(HPD_STORM_DETECT_PERIOD_MS));
    if !in_jiffies_range(now, start, end) {
        hpd.stats[pin as usize].last_jiffies = now;
        hpd.stats[pin as usize].count = 0;
    }
    hpd.stats[pin as usize].count = storm_count(hpd.stats[pin as usize].count, long_hpd);
    if hpd.stats[pin as usize].count as u32 > threshold {
        hpd.stats[pin as usize].state = HPD_MARK_DISABLED;
        true
    } else {
        false
    }
}

#[inline]
unsafe fn detection_work_enabled(display: *mut IntelDisplay) -> bool {
    unsafe { (*display).hotplug.detection_work_enabled }
}

unsafe fn queue_delayed_detection_work(
    display: *mut IntelDisplay,
    work: *mut DelayedWork,
    delay: u64,
    modify: bool,
) -> bool {
    if display.is_null() || work.is_null() || unsafe { !detection_work_enabled(display) } {
        return false;
    }
    let wq = unsafe { (*display).wq.unordered };
    if wq.is_null() {
        return false;
    }
    if modify {
        unsafe { mod_delayed_work(wq, work, delay) }
    } else {
        unsafe { queue_delayed_work(wq, work, delay) }
    }
}

unsafe fn queue_detection_work(display: *mut IntelDisplay, work: *mut WorkStruct) -> bool {
    if display.is_null() || work.is_null() || unsafe { !detection_work_enabled(display) } {
        return false;
    }
    let wq = unsafe { (*display).wq.unordered };
    !wq.is_null() && unsafe { queue_work(wq, work) }
}

#[inline]
unsafe fn hpd_pin_is_blocked(display: *mut IntelDisplay, pin: HpdPin) -> bool {
    hpd_pin_valid(pin) && unsafe { (*display).hotplug.stats[pin as usize].blocked_count != 0 }
}

unsafe fn blocked_pin_mask(display: *mut IntelDisplay) -> u32 {
    let mut mask = 0;
    for pin in 0..HPD_NUM_PINS {
        if unsafe { hpd_pin_is_blocked(display, pin) } {
            mask |= bit(pin);
        }
    }
    mask
}

unsafe fn encoder_has_hpd_pulse(encoder: *mut IntelEncoder) -> bool {
    if encoder.is_null() {
        return false;
    }
    matches!(
        unsafe { (*encoder).r#type },
        INTEL_OUTPUT_DDI | INTEL_OUTPUT_DP | INTEL_OUTPUT_EDP | INTEL_OUTPUT_HDMI
    ) && unsafe { (*encoder.cast::<IntelDigitalPort>()).hpd_pulse.is_some() }
}

unsafe fn pin_has_hpd_pulse(display: *mut IntelDisplay, pin: HpdPin) -> bool {
    let mut found = false;
    unsafe {
        for_each_encoder(display, |encoder| {
            if (*encoder).hpd_pin == pin && encoder_has_hpd_pulse(encoder) {
                found = true;
            }
        });
    }
    found
}

unsafe fn queue_work_for_missed_irqs(display: *mut IntelDisplay) {
    let hpd = unsafe { &mut (*display).hotplug };
    let blocked = unsafe { blocked_pin_mask(display) };
    let mut queue_hotplug = (hpd.event_bits | hpd.retry_bits) & !blocked != 0;
    for pin in 0..HPD_NUM_PINS as usize {
        match hpd.stats[pin].state {
            HPD_MARK_DISABLED => queue_hotplug = true,
            HPD_DISABLED | HPD_ENABLED => {}
            _ => {}
        }
    }
    if (hpd.long_hpd_pin_mask | hpd.short_hpd_pin_mask) & !blocked != 0 {
        if !hpd.dp_wq.is_null() {
            unsafe { queue_work(hpd.dp_wq, &mut hpd.dig_port_work) };
        }
    }
    if queue_hotplug {
        unsafe {
            queue_delayed_detection_work(display, &mut hpd.hotplug_work, 0, false);
        }
    }
}

unsafe extern "C" fn dig_port_work_func(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let offset = offset_of!(IntelDisplay, hotplug) + offset_of!(IntelHotplug, dig_port_work);
    let display = unsafe { work.cast::<u8>().sub(offset).cast::<IntelDisplay>() };
    let hpd = unsafe { &mut (*display).hotplug };
    let mut flags: core::ffi::c_ulong = 0;
    tk_linux_kapi::locks::spin_lock_irqsave(unsafe { &mut (*display).irq.lock }, &mut flags);
    let blocked = unsafe { blocked_pin_mask(display) };
    let long_mask = hpd.long_hpd_pin_mask & !blocked;
    hpd.long_hpd_pin_mask &= !long_mask;
    let short_mask = hpd.short_hpd_pin_mask & !blocked;
    hpd.short_hpd_pin_mask &= !short_mask;
    tk_linux_kapi::locks::spin_unlock_irqrestore(unsafe { &mut (*display).irq.lock }, flags);

    let mut old_bits = 0;
    unsafe {
        for_each_encoder(display, |encoder| {
            if !encoder_has_hpd_pulse(encoder) {
                return;
            }
            let pin = (*encoder).hpd_pin;
            let is_long = long_mask & bit(pin) != 0;
            let is_short = short_mask & bit(pin) != 0;
            if !is_long && !is_short {
                return;
            }
            let dig_port = encoder.cast::<IntelDigitalPort>();
            if let Some(pulse) = (*dig_port).hpd_pulse {
                if pulse(dig_port, is_long)
                    == crate::i915_external_placeholder_types_upstream::IRQ_NONE
                {
                    old_bits |= bit(pin);
                }
            }
        });
    }
    if old_bits != 0 {
        unsafe {
            spin_lock_irq(&mut (*display).irq.lock);
            (*display).hotplug.event_bits |= old_bits;
            queue_delayed_detection_work(display, &mut (*display).hotplug.hotplug_work, 0, false);
            spin_unlock_irq(&mut (*display).irq.lock);
        }
    }
}

unsafe fn intel_hotplug_detect_connector(connector: *mut IntelConnector) -> u32 {
    if connector.is_null() || unsafe { (*connector).base.dev.is_null() } {
        return INTEL_HOTPLUG_UNCHANGED;
    }
    let base = unsafe { &mut (*connector).base };
    let old_epoch = base.epoch_counter;
    let force = base.force != DRM_FORCE_UNSPECIFIED;
    let status = unsafe { drm_helper_probe_detect(base, core::ptr::null_mut(), false) };
    if !force && status >= 0 {
        base.status = status as u32;
    }
    if base.epoch_counter != old_epoch {
        INTEL_HOTPLUG_CHANGED
    } else {
        INTEL_HOTPLUG_UNCHANGED
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_encoder_hotplug(
    _encoder: *mut IntelEncoder,
    connector: *mut IntelConnector,
) -> u32 {
    unsafe { intel_hotplug_detect_connector(connector) }
}

unsafe fn hpd_irq_storm_switch_to_polling(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let dev = unsafe { (*display).drm };
    let mut iter: DrmConnectorListIter = unsafe { core::mem::zeroed() };
    unsafe { connector_iter_begin(dev, &mut iter) };
    let mut hpd_disabled = false;
    loop {
        let base = unsafe { connector_iter_next(&mut iter) };
        if base.is_null() {
            break;
        }
        if unsafe { (*base).polled != DRM_CONNECTOR_POLL_HPD } {
            continue;
        }
        let connector = unsafe { connector_from_base(base) };
        let pin = unsafe { connector_hpd_pin(connector) };
        if !hpd_pin_valid(pin)
            || unsafe { (*display).hotplug.stats[pin as usize].state != HPD_MARK_DISABLED }
        {
            continue;
        }
        unsafe {
            (*display).hotplug.stats[pin as usize].state = HPD_DISABLED;
            (*base).polled = DRM_CONNECTOR_POLL_CONNECT | DRM_CONNECTOR_POLL_DISCONNECT;
        }
        hpd_disabled = true;
    }
    unsafe { connector_iter_end(&mut iter) };
    if hpd_disabled {
        unsafe {
            drm_kms_helper_poll_reschedule(dev);
            queue_delayed_detection_work(
                display,
                &mut (*display).hotplug.reenable_work,
                msecs_to_jiffies(HPD_STORM_REENABLE_DELAY_MS),
                true,
            );
        }
    }
}

unsafe extern "C" fn hpd_irq_storm_reenable_work_func(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let offset = offset_of!(IntelDisplay, hotplug)
        + offset_of!(IntelHotplug, reenable_work)
        + offset_of!(DelayedWork, work);
    let display = unsafe { work.cast::<u8>().sub(offset).cast::<IntelDisplay>() };
    if unsafe { (*display).drm.is_null() } {
        return;
    }
    let wakeref = unsafe { crate::intel_display_rpm_upstream::intel_display_rpm_get(display) };
    unsafe { spin_lock_irq(&mut (*display).irq.lock) };
    let dev = unsafe { (*display).drm };
    let mut iter: DrmConnectorListIter = unsafe { core::mem::zeroed() };
    unsafe { connector_iter_begin(dev, &mut iter) };
    loop {
        let base = unsafe { connector_iter_next(&mut iter) };
        if base.is_null() {
            break;
        }
        let connector = unsafe { connector_from_base(base) };
        let pin = unsafe { connector_hpd_pin(connector) };
        if !hpd_pin_valid(pin)
            || unsafe { (*display).hotplug.stats[pin as usize].state != HPD_DISABLED }
        {
            continue;
        }
        unsafe { (*base).polled = (*connector).polled };
    }
    unsafe { connector_iter_end(&mut iter) };
    for pin in 0..HPD_NUM_PINS as usize {
        if unsafe { (*display).hotplug.stats[pin].state == HPD_DISABLED } {
            unsafe { (*display).hotplug.stats[pin].state = HPD_ENABLED };
        }
    }
    unsafe { crate::intel_hotplug_irq_upstream::intel_hpd_irq_setup(display) };
    unsafe { spin_unlock_irq(&mut (*display).irq.lock) };
    unsafe { crate::intel_display_rpm_upstream::intel_display_rpm_put(display, wakeref) };
}

unsafe extern "C" fn hotplug_work_func(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let offset = offset_of!(IntelDisplay, hotplug)
        + offset_of!(IntelHotplug, hotplug_work)
        + offset_of!(DelayedWork, work);
    let display = unsafe { work.cast::<u8>().sub(offset).cast::<IntelDisplay>() };
    if unsafe { (*display).drm.is_null() } {
        return;
    }
    let dev = unsafe { (*display).drm };
    unsafe { mutex_lock(&mut (*dev).mode_config.mutex) };
    let mut flags: core::ffi::c_ulong = 0;
    unsafe { tk_linux_kapi::locks::spin_lock_irqsave(&mut (*display).irq.lock, &mut flags) };
    let blocked = unsafe { blocked_pin_mask(display) };
    let hotplug = unsafe { &mut (*display).hotplug };
    let event_bits = hotplug.event_bits & !blocked;
    hotplug.event_bits &= !event_bits;
    let retry_bits = hotplug.retry_bits & !blocked;
    hotplug.retry_bits &= !retry_bits;
    unsafe { hpd_irq_storm_switch_to_polling(display) };
    unsafe { tk_linux_kapi::locks::spin_unlock_irqrestore(&mut (*display).irq.lock, flags) };

    if unsafe { ignore_long_hpd(display) } {
        unsafe { mutex_unlock(&mut (*dev).mode_config.mutex) };
        return;
    }

    let mut changed_mask = 0;
    let mut retry_mask = 0;
    let mut changed_count = 0;
    let mut first_changed: *mut DrmConnector = core::ptr::null_mut();
    let mut iter: DrmConnectorListIter = unsafe { core::mem::zeroed() };
    unsafe { connector_iter_begin(dev, &mut iter) };
    loop {
        let base = unsafe { connector_iter_next(&mut iter) };
        if base.is_null() {
            break;
        }
        let connector = unsafe { connector_from_base(base) };
        let pin = unsafe { connector_hpd_pin(connector) };
        if !hpd_pin_valid(pin) {
            continue;
        }
        let pin_bit = bit(pin);
        if (event_bits | retry_bits) & pin_bit == 0 {
            continue;
        }
        let encoder = unsafe { (*connector).encoder };
        if encoder.is_null() {
            continue;
        }
        unsafe {
            if event_bits & pin_bit != 0 {
                (*connector).hotplug_retries = 0;
            } else {
                (*connector).hotplug_retries += 1;
            }
        }
        let state = unsafe {
            match (*encoder).hotplug {
                Some(hotplug) => hotplug(encoder, connector),
                None => INTEL_HOTPLUG_UNCHANGED,
            }
        };
        match state {
            INTEL_HOTPLUG_UNCHANGED => {}
            INTEL_HOTPLUG_CHANGED => {
                changed_mask |= pin_bit;
                changed_count += 1;
                if first_changed.is_null() {
                    first_changed = base;
                    unsafe { connector_ref(first_changed) };
                }
            }
            INTEL_HOTPLUG_RETRY => retry_mask |= pin_bit,
            _ => {}
        }
    }
    unsafe { connector_iter_end(&mut iter) };
    unsafe { mutex_unlock(&mut (*dev).mode_config.mutex) };

    if changed_count == 1 {
        unsafe { drm_kms_helper_connector_hotplug_event(first_changed) };
    } else if changed_count > 1 {
        unsafe { drm_kms_helper_hotplug_event(dev) };
    }
    if !first_changed.is_null() {
        unsafe { connector_unref(first_changed) };
    }

    retry_mask &= !changed_mask;
    if retry_mask != 0 {
        unsafe {
            spin_lock_irq(&mut (*display).irq.lock);
            (*display).hotplug.retry_bits |= retry_mask;
            queue_delayed_detection_work(
                display,
                &mut (*display).hotplug.hotplug_work,
                msecs_to_jiffies(HPD_RETRY_DELAY_MS),
                true,
            );
            spin_unlock_irq(&mut (*display).irq.lock);
        }
    }
}

unsafe extern "C" fn poll_init_work_func(work: *mut WorkStruct) {
    if work.is_null() {
        return;
    }
    let offset = offset_of!(IntelDisplay, hotplug) + offset_of!(IntelHotplug, poll_init_work);
    let display = unsafe { work.cast::<u8>().sub(offset).cast::<IntelDisplay>() };
    if unsafe { (*display).drm.is_null() } {
        return;
    }
    let dev = unsafe { (*display).drm };
    unsafe { mutex_lock(&mut (*dev).mode_config.mutex) };
    let enabled = unsafe { hotplug_poll_enabled(display) };
    let wakeref = if enabled {
        core::ptr::null_mut()
    } else {
        unsafe {
            let wakeref = crate::intel_display_power_upstream::intel_display_power_get(
                display,
                POWER_DOMAIN_DISPLAY_CORE,
            );
            cancel_work(&mut (*display).hotplug.poll_init_work);
            wakeref
        }
    };

    let mut flags: core::ffi::c_ulong = 0;
    unsafe { tk_linux_kapi::locks::spin_lock_irqsave(&mut (*display).irq.lock, &mut flags) };
    let mut iter: DrmConnectorListIter = unsafe { core::mem::zeroed() };
    unsafe { connector_iter_begin(dev, &mut iter) };
    loop {
        let base = unsafe { connector_iter_next(&mut iter) };
        if base.is_null() {
            break;
        }
        let connector = unsafe { connector_from_base(base) };
        let pin = unsafe { connector_hpd_pin(connector) };
        if !hpd_pin_valid(pin)
            || unsafe { (*display).hotplug.stats[pin as usize].state == HPD_DISABLED }
        {
            continue;
        }
        unsafe {
            (*base).polled = (*connector).polled;
            if enabled && (*base).polled == DRM_CONNECTOR_POLL_HPD {
                (*base).polled = DRM_CONNECTOR_POLL_CONNECT | DRM_CONNECTOR_POLL_DISCONNECT;
            }
        }
    }
    unsafe { connector_iter_end(&mut iter) };
    unsafe { tk_linux_kapi::locks::spin_unlock_irqrestore(&mut (*display).irq.lock, flags) };
    if enabled {
        unsafe { drm_kms_helper_poll_reschedule(dev) };
    }
    unsafe { mutex_unlock(&mut (*dev).mode_config.mutex) };
    if !enabled {
        unsafe {
            hpd_poll_detect_connectors(display);
            crate::intel_display_power_upstream::intel_display_power_put(
                display,
                POWER_DOMAIN_DISPLAY_CORE,
                wakeref,
            );
        }
    }
}

unsafe fn hpd_poll_detect_connectors(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let dev = unsafe { (*display).drm };
    unsafe { mutex_lock(&mut (*dev).mode_config.mutex) };
    if !unsafe { (*dev).mode_config.poll_enabled } {
        unsafe { mutex_unlock(&mut (*dev).mode_config.mutex) };
        return;
    }
    let mut changed_count = 0;
    let mut first_changed: *mut DrmConnector = core::ptr::null_mut();
    let mut iter: DrmConnectorListIter = unsafe { core::mem::zeroed() };
    unsafe { connector_iter_begin(dev, &mut iter) };
    loop {
        let base = unsafe { connector_iter_next(&mut iter) };
        if base.is_null() {
            break;
        }
        if unsafe { (*base).polled & DRM_CONNECTOR_POLL_HPD == 0 } {
            continue;
        }
        let connector = unsafe { connector_from_base(base) };
        if unsafe { intel_hotplug_detect_connector(connector) != INTEL_HOTPLUG_CHANGED } {
            continue;
        }
        changed_count += 1;
        if changed_count == 1 {
            first_changed = base;
            unsafe { connector_ref(first_changed) };
        }
    }
    unsafe { connector_iter_end(&mut iter) };
    unsafe { mutex_unlock(&mut (*dev).mode_config.mutex) };
    if changed_count == 1 {
        unsafe { drm_kms_helper_connector_hotplug_event(first_changed) };
    } else if changed_count > 1 {
        unsafe { drm_kms_helper_hotplug_event(dev) };
    }
    if !first_changed.is_null() {
        unsafe { connector_unref(first_changed) };
    }
}

unsafe fn cancel_all_detection_work(display: *mut IntelDisplay) -> bool {
    let hpd = unsafe { &mut (*display).hotplug };
    let mut pending = false;
    unsafe {
        pending |= cancel_delayed_work_sync(&mut hpd.hotplug_work);
        pending |= cancel_work_sync(&mut hpd.poll_init_work);
        pending |= cancel_delayed_work_sync(&mut hpd.reenable_work);
    }
    pending
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_init_early(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        let hpd = &mut (*display).hotplug;
        INIT_DELAYED_WORK(&mut hpd.hotplug_work, |work| unsafe {
            hotplug_work_func(work)
        });
        INIT_WORK_C(&mut hpd.dig_port_work, dig_port_work_func);
        INIT_WORK_C(&mut hpd.poll_init_work, poll_init_work_func);
        INIT_DELAYED_WORK(&mut hpd.reenable_work, |work| unsafe {
            hpd_irq_storm_reenable_work_func(work)
        });
        hpd.hpd_storm_threshold = HPD_STORM_DEFAULT_THRESHOLD;
        hpd.hpd_short_storm_enabled = (!display_has_dp_mst(display)) as u8;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_init_impl(display: *mut IntelDisplay) {
    if !unsafe { display_has_display(display) } {
        return;
    }
    for pin in 0..HPD_NUM_PINS as usize {
        unsafe {
            (*display).hotplug.stats[pin].count = 0;
            (*display).hotplug.stats[pin].state = HPD_ENABLED;
        }
    }
    unsafe {
        spin_lock_irq(&mut (*display).irq.lock);
        crate::intel_hotplug_irq_upstream::intel_hpd_irq_setup(display);
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_poll_enable(display: *mut IntelDisplay) {
    if !unsafe { display_has_display(display) }
        || !unsafe { crate::intel_display_device_upstream::intel_display_device_enabled(display) }
    {
        return;
    }
    unsafe { set_hotplug_poll_enabled(display, true) };
    unsafe {
        spin_lock_irq(&mut (*display).irq.lock);
        queue_detection_work(display, &mut (*display).hotplug.poll_init_work);
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_poll_disable_impl(display: *mut IntelDisplay) {
    if !unsafe { display_has_display(display) } {
        return;
    }
    unsafe {
        for_each_encoder(display, |encoder| {
            let kind = (*encoder).r#type;
            if kind == INTEL_OUTPUT_DP || kind == INTEL_OUTPUT_EDP {
                let dp = &mut (*encoder.cast::<IntelDigitalPort>()).dp;
                intel_dp_dpcd_set_probe(dp, true);
            } else if kind == INTEL_OUTPUT_DDI
                && (*encoder.cast::<IntelDigitalPort>()).dp.output_reg.reg != 0
            {
                let dp = &mut (*encoder.cast::<IntelDigitalPort>()).dp;
                intel_dp_dpcd_set_probe(dp, true);
            }
        });
        set_hotplug_poll_enabled(display, false);
        spin_lock_irq(&mut (*display).irq.lock);
        queue_detection_work(display, &mut (*display).hotplug.poll_init_work);
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_poll_fini_impl(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let dev = unsafe { (*display).drm };
    let mut iter: DrmConnectorListIter = unsafe { core::mem::zeroed() };
    unsafe { connector_iter_begin(dev, &mut iter) };
    loop {
        let base = unsafe { connector_iter_next(&mut iter) };
        if base.is_null() {
            break;
        }
        let connector = unsafe { connector_from_base(base) };
        unsafe {
            crate::intel_connector_upstream::intel_connector_cancel_modeset_retry_work(connector);
            if (*connector).hdcp.check_work.work.function.is_some() {
                cancel_delayed_work_sync(&mut (*connector).hdcp.check_work);
            }
            if (*connector).hdcp.prop_work.function.is_some() {
                cancel_work_sync(&mut (*connector).hdcp.prop_work);
            }
        }
    }
    unsafe { connector_iter_end(&mut iter) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_cancel_work(display: *mut IntelDisplay) {
    if !unsafe { display_has_display(display) } {
        return;
    }
    unsafe {
        spin_lock_irq(&mut (*display).irq.lock);
        (*display).hotplug.long_hpd_pin_mask = 0;
        (*display).hotplug.short_hpd_pin_mask = 0;
        (*display).hotplug.event_bits = 0;
        (*display).hotplug.retry_bits = 0;
        spin_unlock_irq(&mut (*display).irq.lock);
        cancel_work_sync(&mut (*display).hotplug.dig_port_work);
        let _ = cancel_all_detection_work(display);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_enable_detection_work_impl(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        spin_lock_irq(&mut (*display).irq.lock);
        (*display).hotplug.detection_work_enabled = true;
        queue_work_for_missed_irqs(display);
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_disable_detection_work_impl(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        spin_lock_irq(&mut (*display).irq.lock);
        (*display).hotplug.detection_work_enabled = false;
        spin_unlock_irq(&mut (*display).irq.lock);
        cancel_all_detection_work(display);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_schedule_detection(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    unsafe {
        let mut flags: core::ffi::c_ulong = 0;
        tk_linux_kapi::locks::spin_lock_irqsave(&mut (*display).irq.lock, &mut flags);
        let queued =
            queue_delayed_detection_work(display, &mut (*display).hotplug.hotplug_work, 0, false);
        tk_linux_kapi::locks::spin_unlock_irqrestore(&mut (*display).irq.lock, flags);
        queued
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_irq_handler(
    display: *mut IntelDisplay,
    pin_mask: u32,
    long_mask: u32,
) {
    if display.is_null() || pin_mask == 0 {
        return;
    }
    unsafe { spin_lock_irq(&mut (*display).irq.lock) };
    let mut queue_dp = false;
    let mut queue_hotplug = false;
    let mut storm_detected = false;
    let mut long_pulse_mask = 0;
    let mut short_pulse_mask = 0;

    unsafe {
        for_each_encoder(display, |encoder| {
            let pin = (*encoder).hpd_pin;
            if !hpd_pin_valid(pin) || pin_mask & bit(pin) == 0 || !encoder_has_hpd_pulse(encoder) {
                return;
            }
            let is_long = long_mask & bit(pin) != 0;
            if !hpd_pin_is_blocked(display, pin) {
                queue_dp = true;
            }
            if is_long {
                long_pulse_mask |= bit(pin);
                (*display).hotplug.long_hpd_pin_mask |= bit(pin);
            } else {
                short_pulse_mask |= bit(pin);
                (*display).hotplug.short_hpd_pin_mask |= bit(pin);
            }
        });
    }

    for pin in 0..HPD_NUM_PINS {
        let mask = bit(pin);
        if pin_mask & mask == 0 {
            continue;
        }
        let state = unsafe { (*display).hotplug.stats[pin as usize].state };
        if state != HPD_ENABLED {
            continue;
        }
        let long_hpd = if (long_pulse_mask | short_pulse_mask) & mask != 0 {
            long_pulse_mask & mask != 0
        } else {
            unsafe { (*display).hotplug.event_bits |= mask };
            if !unsafe { hpd_pin_is_blocked(display, pin) } {
                queue_hotplug = true;
            }
            true
        };
        if unsafe { intel_hpd_irq_storm_detect(display, pin, long_hpd) } {
            unsafe { (*display).hotplug.event_bits &= !mask };
            storm_detected = true;
            queue_hotplug = true;
        }
    }

    if storm_detected {
        unsafe { crate::intel_hotplug_irq_upstream::intel_hpd_irq_setup(display) };
    }
    unsafe {
        if queue_dp && !(*display).hotplug.dp_wq.is_null() {
            queue_work(
                (*display).hotplug.dp_wq,
                &mut (*display).hotplug.dig_port_work,
            );
        }
        if queue_hotplug {
            queue_delayed_detection_work(display, &mut (*display).hotplug.hotplug_work, 0, false);
        }
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_trigger_irq(dig_port: *mut IntelDigitalPort) {
    if dig_port.is_null() {
        return;
    }
    let encoder = unsafe { &mut (*dig_port).base } as *mut IntelEncoder;
    let display = unsafe { display_from_encoder(encoder) };
    if display.is_null() {
        return;
    }
    let pin = unsafe { (*encoder).hpd_pin };
    if !hpd_pin_valid(pin) {
        return;
    }
    unsafe {
        spin_lock_irq(&mut (*display).irq.lock);
        (*display).hotplug.short_hpd_pin_mask |= bit(pin);
        if !hpd_pin_is_blocked(display, pin) && !(*display).hotplug.dp_wq.is_null() {
            queue_work(
                (*display).hotplug.dp_wq,
                &mut (*display).hotplug.dig_port_work,
            );
        }
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

unsafe fn unblock_hpd_pin(display: *mut IntelDisplay, pin: HpdPin) -> bool {
    if !hpd_pin_valid(pin) {
        return true;
    }
    let blocked = unsafe { &mut (*display).hotplug.stats[pin as usize].blocked_count };
    if *blocked == 0 {
        return true;
    }
    *blocked -= 1;
    *blocked == 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_block(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let display = unsafe { display_from_encoder(encoder) };
    let pin = unsafe { (*encoder).hpd_pin };
    if display.is_null() || !hpd_pin_valid(pin) || pin == HPD_NONE {
        return;
    }
    unsafe { spin_lock_irq(&mut (*display).irq.lock) };
    let blocked = unsafe { &mut (*display).hotplug.stats[pin as usize].blocked_count };
    *blocked = blocked.wrapping_add(1);
    let do_flush = *blocked == 1;
    unsafe { spin_unlock_irq(&mut (*display).irq.lock) };
    if do_flush && unsafe { pin_has_hpd_pulse(display, pin) } {
        unsafe { flush_work(&mut (*display).hotplug.dig_port_work) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_unblock(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let display = unsafe { display_from_encoder(encoder) };
    let pin = unsafe { (*encoder).hpd_pin };
    if display.is_null() || !hpd_pin_valid(pin) || pin == HPD_NONE {
        return;
    }
    unsafe {
        spin_lock_irq(&mut (*display).irq.lock);
        if unblock_hpd_pin(display, pin) {
            queue_work_for_missed_irqs(display);
        }
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_clear_and_unblock(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let display = unsafe { display_from_encoder(encoder) };
    let pin = unsafe { (*encoder).hpd_pin };
    if display.is_null() || !hpd_pin_valid(pin) || pin == HPD_NONE {
        return;
    }
    unsafe {
        spin_lock_irq(&mut (*display).irq.lock);
        if unblock_hpd_pin(display, pin) {
            let mask = !bit(pin);
            (*display).hotplug.event_bits &= mask;
            (*display).hotplug.retry_bits &= mask;
            (*display).hotplug.short_hpd_pin_mask &= mask;
            (*display).hotplug.long_hpd_pin_mask &= mask;
        }
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_poll_detectors(display: *mut IntelDisplay) {
    unsafe { hpd_poll_detect_connectors(display) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storm_counter_weights_long_pulses_and_time_wraparound() {
        assert_eq!(storm_count(3, false), 4);
        assert_eq!(storm_count(3, true), 13);
        assert!(in_jiffies_range(3, u64::MAX - 2, 4));
        assert!(!in_jiffies_range(5, u64::MAX - 2, 4));
    }
}
