// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of the Alder Lake-P / display-IP 13 path in Linux v7.2.3
// drivers/gpu/drm/i915/display/intel_hotplug_irq.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use tk_drm_upstream::drm_encoder_types::DrmEncoder;
use tk_linux_kapi::locks::{spin_lock, spin_lock_irq, spin_unlock, spin_unlock_irq};

use crate::{
    intel_de_upstream::{intel_de_posting_read, intel_de_read, intel_de_rmw, intel_de_write},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_limits_types_upstream::{
        HPD_NUM_PINS, HPD_PORT_A, HPD_PORT_B, HPD_PORT_C, HPD_PORT_D, HPD_PORT_TC1, HPD_PORT_TC2,
        HPD_PORT_TC3, HPD_PORT_TC4, HPD_PORT_TC5, HPD_PORT_TC6, HpdPin,
    },
    intel_display_reg_defs_upstream::I915RegT,
    intel_display_regs_upstream::*,
    intel_display_types_upstream::IntelEncoder,
    intel_pch_types_upstream::{PCH_ADP, PCH_ICP},
};

const HPD_ENABLED: u32 = 0;

#[repr(C)]
struct IntelHotplugIrqFuncs {
    hpd_irq_setup: Option<unsafe extern "C" fn(*mut IntelDisplay)>,
    hpd_enable_detection: Option<unsafe extern "C" fn(*mut IntelEncoder)>,
}

static GEN11_HPD_IRQ_FUNCS: IntelHotplugIrqFuncs = IntelHotplugIrqFuncs {
    hpd_irq_setup: Some(gen11_hpd_irq_setup),
    hpd_enable_detection: Some(gen11_hpd_enable_detection),
};

const fn gen11_hpd_map() -> [u32; HPD_NUM_PINS as usize] {
    let mut map = [0; HPD_NUM_PINS as usize];
    map[HPD_PORT_TC1 as usize] = GEN11_TC_HOTPLUG(HPD_PORT_TC1) | GEN11_TBT_HOTPLUG(HPD_PORT_TC1);
    map[HPD_PORT_TC2 as usize] = GEN11_TC_HOTPLUG(HPD_PORT_TC2) | GEN11_TBT_HOTPLUG(HPD_PORT_TC2);
    map[HPD_PORT_TC3 as usize] = GEN11_TC_HOTPLUG(HPD_PORT_TC3) | GEN11_TBT_HOTPLUG(HPD_PORT_TC3);
    map[HPD_PORT_TC4 as usize] = GEN11_TC_HOTPLUG(HPD_PORT_TC4) | GEN11_TBT_HOTPLUG(HPD_PORT_TC4);
    map[HPD_PORT_TC5 as usize] = GEN11_TC_HOTPLUG(HPD_PORT_TC5) | GEN11_TBT_HOTPLUG(HPD_PORT_TC5);
    map[HPD_PORT_TC6 as usize] = GEN11_TC_HOTPLUG(HPD_PORT_TC6) | GEN11_TBT_HOTPLUG(HPD_PORT_TC6);
    map
}

const fn icp_hpd_map() -> [u32; HPD_NUM_PINS as usize] {
    let mut map = [0; HPD_NUM_PINS as usize];
    map[HPD_PORT_A as usize] = SDE_DDI_HOTPLUG_ICP(HPD_PORT_A);
    map[HPD_PORT_B as usize] = SDE_DDI_HOTPLUG_ICP(HPD_PORT_B);
    map[HPD_PORT_C as usize] = SDE_DDI_HOTPLUG_ICP(HPD_PORT_C);
    map[HPD_PORT_D as usize] = SDE_DDI_HOTPLUG_ICP(HPD_PORT_D);
    map[HPD_PORT_TC1 as usize] = SDE_TC_HOTPLUG_ICP(HPD_PORT_TC1);
    map[HPD_PORT_TC2 as usize] = SDE_TC_HOTPLUG_ICP(HPD_PORT_TC2);
    map[HPD_PORT_TC3 as usize] = SDE_TC_HOTPLUG_ICP(HPD_PORT_TC3);
    map[HPD_PORT_TC4 as usize] = SDE_TC_HOTPLUG_ICP(HPD_PORT_TC4);
    map[HPD_PORT_TC5 as usize] = SDE_TC_HOTPLUG_ICP(HPD_PORT_TC5);
    map[HPD_PORT_TC6 as usize] = SDE_TC_HOTPLUG_ICP(HPD_PORT_TC6);
    map
}

static HPD_GEN11: [u32; HPD_NUM_PINS as usize] = gen11_hpd_map();
static HPD_ICP: [u32; HPD_NUM_PINS as usize] = icp_hpd_map();

#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_hotplug_interrupt_update_locked(
    display: *mut IntelDisplay,
    mask: u32,
    bits: u32,
) {
    if display.is_null() || bits & !mask != 0 {
        return;
    }
    let reg = unsafe { PORT_HOTPLUG_EN(display) };
    unsafe { intel_de_rmw(display, reg, mask, bits) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_hotplug_interrupt_update(
    display: *mut IntelDisplay,
    mask: u32,
    bits: u32,
) {
    if display.is_null() {
        return;
    }
    unsafe {
        spin_lock_irq(&mut (*display).irq.lock);
        i915_hotplug_interrupt_update_locked(display, mask, bits);
        spin_unlock_irq(&mut (*display).irq.lock);
    }
}

#[inline]
unsafe fn has_hotplug(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    let info = unsafe { (*display).info.__device_info };
    !info.is_null() && unsafe { (*info)._bitfields_46[1] & 1 != 0 }
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
                .sub(core::mem::offset_of!(DrmEncoder, head))
                .cast::<IntelEncoder>()
        };
        f(encoder);
        node = unsafe { (*node).next };
    }
}

unsafe fn encoder_hotplug_mask(
    display: *mut IntelDisplay,
    map: &[u32; HPD_NUM_PINS as usize],
    enabled_only: bool,
) -> u32 {
    let mut bits = 0;
    unsafe {
        for_each_encoder(display, |encoder| {
            let pin = (*encoder).hpd_pin as usize;
            if pin >= map.len()
                || (enabled_only && (*display).hotplug.stats[pin].state != HPD_ENABLED)
            {
                return;
            }
            bits |= map[pin];
        });
    }
    bits
}

fn all_hpd_mask(f: impl Fn(HpdPin) -> u32) -> u32 {
    let mut mask = 0;
    for pin in 0..HPD_NUM_PINS {
        mask |= f(pin);
    }
    mask
}

fn ddi_enable_mask(pin: HpdPin) -> u32 {
    match pin {
        HPD_PORT_A | HPD_PORT_B | HPD_PORT_C | HPD_PORT_D => SHOTPLUG_CTL_DDI_HPD_ENABLE(pin),
        _ => 0,
    }
}

fn tc_enable_mask(pin: HpdPin) -> u32 {
    match pin {
        HPD_PORT_TC1 | HPD_PORT_TC2 | HPD_PORT_TC3 | HPD_PORT_TC4 | HPD_PORT_TC5 | HPD_PORT_TC6 => {
            ICP_TC_HPD_ENABLE(pin)
        }
        _ => 0,
    }
}

fn gen11_enable_mask(pin: HpdPin) -> u32 {
    match pin {
        HPD_PORT_TC1 | HPD_PORT_TC2 | HPD_PORT_TC3 | HPD_PORT_TC4 | HPD_PORT_TC5 | HPD_PORT_TC6 => {
            GEN11_HOTPLUG_CTL_ENABLE(pin)
        }
        _ => 0,
    }
}

// Upstream's maps are per-pin interrupt bits; detection-control enable bits
// have different encodings. Build the latter from the encoder set explicitly.
unsafe fn detection_enables(display: *mut IntelDisplay, f: impl Fn(HpdPin) -> u32) -> u32 {
    let mut enabled = 0;
    unsafe {
        for_each_encoder(display, |encoder| enabled |= f((*encoder).hpd_pin));
    }
    enabled
}

unsafe fn write_detection_setup(
    display: *mut IntelDisplay,
    reg: I915RegT,
    mask_fn: impl Fn(HpdPin) -> u32,
    enable_fn: impl Fn(HpdPin) -> u32,
) {
    let mask = all_hpd_mask(mask_fn);
    let enables = detection_enables(display, enable_fn);
    unsafe { intel_de_rmw(display, reg, mask, enables) };
}

unsafe fn icp_ddi_hpd_enable_detection(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let pin = unsafe { (*encoder).hpd_pin };
    let display = unsafe { to_display(encoder) };
    if !display.is_null() {
        unsafe {
            intel_de_rmw(
                display,
                SHOTPLUG_CTL_DDI,
                ddi_enable_mask(pin),
                ddi_enable_mask(pin),
            )
        };
    }
}

unsafe fn icp_tc_hpd_enable_detection(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let pin = unsafe { (*encoder).hpd_pin };
    let display = unsafe { to_display(encoder) };
    if !display.is_null() {
        unsafe {
            intel_de_rmw(
                display,
                SHOTPLUG_CTL_TC,
                tc_enable_mask(pin),
                tc_enable_mask(pin),
            )
        };
    }
}

unsafe fn to_display(encoder: *mut IntelEncoder) -> *mut IntelDisplay {
    if encoder.is_null() || unsafe { (*encoder).base.dev.is_null() } {
        return core::ptr::null_mut();
    }
    crate::intel_display_conversion_upstream::__drm_to_display(unsafe { (*encoder).base.dev })
}

unsafe extern "C" fn gen11_hpd_enable_detection(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let display = unsafe { to_display(encoder) };
    if display.is_null() {
        return;
    }
    let pin = unsafe { (*encoder).hpd_pin };
    unsafe {
        intel_de_rmw(
            display,
            GEN11_TC_HOTPLUG_CTL,
            gen11_enable_mask(pin),
            gen11_enable_mask(pin),
        );
        intel_de_rmw(
            display,
            GEN11_TBT_HOTPLUG_CTL,
            gen11_enable_mask(pin),
            gen11_enable_mask(pin),
        );
        if (*display).pch_type == PCH_ADP {
            icp_ddi_hpd_enable_detection(encoder);
            icp_tc_hpd_enable_detection(encoder);
        }
    }
}

unsafe extern "C" fn gen11_hpd_irq_setup(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let enabled_irqs = unsafe { encoder_hotplug_mask(display, &HPD_GEN11, true) };
    let hotplug_irqs = unsafe { encoder_hotplug_mask(display, &HPD_GEN11, false) };
    unsafe {
        let old = intel_de_read(display, GEN11_DE_HPD_IMR);
        let new = (old & !hotplug_irqs) | (!enabled_irqs & hotplug_irqs);
        if new != old {
            intel_de_write(display, GEN11_DE_HPD_IMR, new);
            intel_de_posting_read(display, GEN11_DE_HPD_IMR);
        }
        write_detection_setup(display, GEN11_TC_HOTPLUG_CTL, gen11_enable_mask, |pin| {
            if HPD_GEN11[pin as usize] != 0 {
                gen11_enable_mask(pin)
            } else {
                0
            }
        });
        write_detection_setup(display, GEN11_TBT_HOTPLUG_CTL, gen11_enable_mask, |pin| {
            if HPD_GEN11[pin as usize] != 0 {
                gen11_enable_mask(pin)
            } else {
                0
            }
        });
        if (*display).pch_type == PCH_ADP {
            icp_hpd_irq_setup(display);
        }
    }
}

unsafe fn icp_hpd_irq_setup(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let enabled_irqs = unsafe { encoder_hotplug_mask(display, &HPD_ICP, true) };
    let hotplug_irqs = unsafe { encoder_hotplug_mask(display, &HPD_ICP, false) };
    unsafe {
        intel_de_write(display, SHPD_FILTER_CNT, SHPD_FILTER_CNT_250);
        crate::intel_display_irq_upstream::ibx_display_interrupt_update(
            display,
            hotplug_irqs,
            enabled_irqs,
        );
        write_detection_setup(display, SHOTPLUG_CTL_DDI, ddi_enable_mask, |pin| {
            if HPD_ICP[pin as usize] == 0 || pin >= HPD_PORT_TC1 {
                0
            } else {
                ddi_enable_mask(pin)
            }
        });
        write_detection_setup(display, SHOTPLUG_CTL_TC, tc_enable_mask, |pin| {
            if HPD_ICP[pin as usize] == 0 || pin < HPD_PORT_TC1 {
                0
            } else {
                tc_enable_mask(pin)
            }
        });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_enable_detection(encoder: *mut IntelEncoder) {
    if encoder.is_null() {
        return;
    }
    let display = unsafe { to_display(encoder) };
    if display.is_null() {
        return;
    }
    let funcs = unsafe { (*display).hotplug.funcs.cast::<IntelHotplugIrqFuncs>() };
    if !funcs.is_null() {
        if let Some(enable) = unsafe { (*funcs).hpd_enable_detection } {
            unsafe { enable(encoder) };
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hpd_irq_setup(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let funcs = unsafe { (*display).hotplug.funcs.cast::<IntelHotplugIrqFuncs>() };
    if !funcs.is_null() {
        if let Some(setup) = unsafe { (*funcs).hpd_irq_setup } {
            unsafe { setup(display) };
        }
    }
}

// Set the selected north/south HPD maps and callback table before display IRQ
// postinstall starts dispatching hotplug events.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_hotplug_irq_init(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        crate::intel_hotplug_upstream::intel_hpd_init_early(display);
        let ver = (*display).info.__runtime_info.ip.ver;
        let pch = (*display).pch_type;
        let adlp_path = has_hotplug(display) && ver == 13 && pch == PCH_ADP;
        (*display).hotplug.hpd = if adlp_path {
            HPD_GEN11.as_ptr() as *mut u32
        } else {
            core::ptr::null_mut()
        };
        (*display).hotplug.pch_hpd = if adlp_path {
            HPD_ICP.as_ptr() as *mut u32
        } else {
            core::ptr::null_mut()
        };
        (*display).hotplug.funcs = if adlp_path {
            (&GEN11_HPD_IRQ_FUNCS as *const IntelHotplugIrqFuncs)
                .cast_mut()
                .cast::<c_void>()
        } else {
            core::ptr::null_mut()
        };
    }
}

fn collect_hpd_pins(
    trigger: u32,
    dig_hotplug_reg: u32,
    map: &[u32; HPD_NUM_PINS as usize],
    mut is_long: impl FnMut(HpdPin, u32) -> bool,
) -> (u32, u32) {
    let mut pins = 0;
    let mut longs = 0;
    for pin in 0..HPD_NUM_PINS {
        let pin = pin as usize;
        if map[pin] & trigger == 0 {
            continue;
        }
        pins |= 1u32 << pin;
        if is_long(pin as HpdPin, dig_hotplug_reg) {
            longs |= 1u32 << pin;
        }
    }
    (pins, longs)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen11_hpd_irq_handler(display: *mut IntelDisplay, iir: u32) {
    if display.is_null() {
        return;
    }
    let tc_trigger = iir & GEN11_DE_TC_HOTPLUG_MASK;
    let tbt_trigger = iir & GEN11_DE_TBT_HOTPLUG_MASK;
    let map_ptr = unsafe { (*display).hotplug.hpd };
    if map_ptr.is_null() {
        return;
    }
    let map = unsafe { &*map_ptr.cast::<[u32; HPD_NUM_PINS as usize]>() };
    let mut pins = 0;
    let mut longs = 0;
    if tc_trigger != 0 {
        let reg = unsafe { intel_de_rmw(display, GEN11_TC_HOTPLUG_CTL, 0, 0) };
        let (p, l) = collect_hpd_pins(tc_trigger, reg, map, |pin, val| {
            matches!(
                pin,
                HPD_PORT_TC1
                    | HPD_PORT_TC2
                    | HPD_PORT_TC3
                    | HPD_PORT_TC4
                    | HPD_PORT_TC5
                    | HPD_PORT_TC6
            ) && val & GEN11_HOTPLUG_CTL_LONG_DETECT(pin) != 0
        });
        pins |= p;
        longs |= l;
    }
    if tbt_trigger != 0 {
        let reg = unsafe { intel_de_rmw(display, GEN11_TBT_HOTPLUG_CTL, 0, 0) };
        let (p, l) = collect_hpd_pins(tbt_trigger, reg, map, |pin, val| {
            matches!(
                pin,
                HPD_PORT_TC1
                    | HPD_PORT_TC2
                    | HPD_PORT_TC3
                    | HPD_PORT_TC4
                    | HPD_PORT_TC5
                    | HPD_PORT_TC6
            ) && val & GEN11_HOTPLUG_CTL_LONG_DETECT(pin) != 0
        });
        pins |= p;
        longs |= l;
    }
    if pins != 0 {
        unsafe { crate::intel_hotplug_upstream::intel_hpd_irq_handler(display, pins, longs) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn icp_irq_handler(display: *mut IntelDisplay, pch_iir: u32) {
    if display.is_null() {
        return;
    }
    let map_ptr = unsafe { (*display).hotplug.pch_hpd };
    if !map_ptr.is_null() {
        let map = unsafe { &*map_ptr.cast::<[u32; HPD_NUM_PINS as usize]>() };
        let ddi_trigger = pch_iir & SDE_DDI_HOTPLUG_MASK_ICP;
        let tc_trigger = pch_iir & SDE_TC_HOTPLUG_MASK_ICP;
        let mut pins = 0;
        let mut longs = 0;
        if ddi_trigger != 0 {
            unsafe { spin_lock(&mut (*display).irq.lock) };
            let reg = unsafe { intel_de_rmw(display, SHOTPLUG_CTL_DDI, 0, 0) };
            unsafe { spin_unlock(&mut (*display).irq.lock) };
            let (p, l) = collect_hpd_pins(ddi_trigger, reg, map, |pin, val| {
                matches!(pin, HPD_PORT_A | HPD_PORT_B | HPD_PORT_C | HPD_PORT_D)
                    && val & SHOTPLUG_CTL_DDI_HPD_LONG_DETECT(pin) != 0
            });
            pins |= p;
            longs |= l;
        }
        if tc_trigger != 0 {
            let reg = unsafe { intel_de_rmw(display, SHOTPLUG_CTL_TC, 0, 0) };
            let (p, l) = collect_hpd_pins(tc_trigger, reg, map, |pin, val| {
                matches!(
                    pin,
                    HPD_PORT_TC1
                        | HPD_PORT_TC2
                        | HPD_PORT_TC3
                        | HPD_PORT_TC4
                        | HPD_PORT_TC5
                        | HPD_PORT_TC6
                ) && val & ICP_TC_HPD_LONG_DETECT(pin) != 0
            });
            pins |= p;
            longs |= l;
        }
        if pins != 0 {
            unsafe { crate::intel_hotplug_upstream::intel_hpd_irq_handler(display, pins, longs) };
        }
    }
    if unsafe { (*display).pch_type >= PCH_ICP } && pch_iir & SDE_GMBUS_ICP != 0 {
        unsafe { crate::intel_gmbus_upstream::intel_gmbus_irq_handler(display) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adlp_pin_tables_only_map_gen11_tc_and_icp_ddi_tc_pins() {
        assert_eq!(HPD_GEN11[HPD_PORT_A as usize], 0);
        assert_ne!(HPD_GEN11[HPD_PORT_TC1 as usize], 0);
        assert_eq!(
            HPD_ICP[HPD_PORT_A as usize],
            SDE_DDI_HOTPLUG_ICP(HPD_PORT_A)
        );
        assert_eq!(
            HPD_ICP[HPD_PORT_TC1 as usize],
            SDE_TC_HOTPLUG_ICP(HPD_PORT_TC1)
        );
        assert_eq!(
            HPD_ICP[crate::intel_display_limits_types_upstream::HPD_PORT_E as usize],
            0
        );
    }

    #[test]
    fn trigger_decoder_accumulates_distinct_pins_and_long_pulses() {
        let mut map = [0; HPD_NUM_PINS as usize];
        map[1] = 1 << 5;
        map[2] = 1 << 6;
        let (pins, longs) = collect_hpd_pins(1 << 5 | 1 << 6, 1 << 6, &map, |pin, value| {
            pin == 2 && value & (1 << 6) != 0
        });
        assert_eq!(pins, (1 << 1) | (1 << 2));
        assert_eq!(longs, 1 << 2);
    }
}
