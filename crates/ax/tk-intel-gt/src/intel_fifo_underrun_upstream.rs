// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// ADL-P path translation of intel_fifo_underrun.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use tk_linux_kapi::locks::{spin_lock_irqsave, spin_unlock_irqrestore};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_device_types_upstream::IntelDisplayDeviceInfo,
    intel_display_device_upstream::intel_display_device_present,
    intel_display_irq_upstream::{bdw_disable_pipe_irq, bdw_enable_pipe_irq},
    intel_display_limits_types_upstream::Pipe,
    intel_display_types_upstream::IntelCrtc,
};

const GEN8_PIPE_FIFO_UNDERRUN: u32 = 1 << 31;

#[inline]
unsafe fn crtc_for_pipe(display: *mut IntelDisplay, pipe: Pipe) -> *mut IntelCrtc {
    unsafe { crate::intel_crtc_upstream::intel_crtc_for_pipe(display, pipe) }
}

#[inline]
fn previous_reporting(disabled: bool) -> bool {
    !disabled
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_init_fifo_underrun_reporting(
    display: *mut IntelDisplay,
    crtc: *mut IntelCrtc,
    enable: bool,
) {
    if display.is_null() || crtc.is_null() {
        return;
    }
    unsafe {
        (*crtc).cpu_fifo_underrun_disabled = !enable;
        // ADL-P's PCH_ADP has no PCH transcoder FIFO interrupt domain.
        if (*display).pch_type != crate::intel_pch_types_upstream::PCH_ADP
            && (*display).pch_type != crate::intel_pch_types_upstream::PCH_NOP
        {
            (*crtc).pch_fifo_underrun_disabled = !enable;
        }
    }
}

unsafe fn set_cpu_fifo_underrun_reporting_locked(
    display: *mut IntelDisplay,
    pipe: Pipe,
    enable: bool,
) -> bool {
    let crtc = unsafe { crtc_for_pipe(display, pipe) };
    if crtc.is_null() {
        return false;
    }
    let old = unsafe { previous_reporting((*crtc).cpu_fifo_underrun_disabled) };
    unsafe { (*crtc).cpu_fifo_underrun_disabled = !enable };
    if enable {
        unsafe { bdw_enable_pipe_irq(display, pipe, GEN8_PIPE_FIFO_UNDERRUN) };
    } else {
        unsafe { bdw_disable_pipe_irq(display, pipe, GEN8_PIPE_FIFO_UNDERRUN) };
    }
    old
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_set_cpu_fifo_underrun_reporting(
    display: *mut IntelDisplay,
    pipe: Pipe,
    enable: bool,
) -> bool {
    if !unsafe { intel_display_device_present(display) } {
        return false;
    }
    let mut flags = 0u64;
    unsafe { spin_lock_irqsave(&mut (*display).irq.lock, &mut flags) };
    let old = unsafe { set_cpu_fifo_underrun_reporting_locked(display, pipe, enable) };
    unsafe { spin_unlock_irqrestore(&mut (*display).irq.lock, flags) };
    old
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_set_pch_fifo_underrun_reporting(
    display: *mut IntelDisplay,
    pipe: Pipe,
    enable: bool,
) -> bool {
    let crtc = unsafe { crtc_for_pipe(display, pipe) };
    if crtc.is_null() {
        return false;
    }
    let mut flags = 0u64;
    unsafe { spin_lock_irqsave(&mut (*display).irq.lock, &mut flags) };
    let old = unsafe { previous_reporting((*crtc).pch_fifo_underrun_disabled) };
    unsafe { (*crtc).pch_fifo_underrun_disabled = !enable };
    unsafe { spin_unlock_irqrestore(&mut (*display).irq.lock, flags) };
    old
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cpu_fifo_underrun_irq_handler(
    display: *mut IntelDisplay,
    pipe: Pipe,
) {
    if !unsafe { intel_display_device_present(display) } {
        return;
    }
    let crtc = unsafe { crtc_for_pipe(display, pipe) };
    // The display IRQ can fire before CRTC objects have been registered.
    if crtc.is_null() {
        return;
    }
    if unsafe { intel_set_cpu_fifo_underrun_reporting(display, pipe, false) } {
        axlog::error!("CPU pipe {} FIFO underrun", pipe);
    }
    // FBC is an explicit compile-out feature in this bring-up batch.
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_pch_fifo_underrun_irq_handler(
    display: *mut IntelDisplay,
    pipe: Pipe,
) {
    if unsafe { intel_set_pch_fifo_underrun_reporting(display, pipe, false) } {
        axlog::error!("PCH transcoder {} FIFO underrun", pipe);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_check_cpu_fifo_underruns(display: *mut IntelDisplay) {
    if !unsafe { intel_display_device_present(display) } {
        return;
    }
    // The ADL-P/Gen8+ interrupt is per-pipe; Linux only polls legacy shared
    // status registers here, so there is no CPU-side register check on v13.
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_check_pch_fifo_underruns(_display: *mut IntelDisplay) {
    // PCH_ADP has no PCH transcoder underrun status source.
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fifo_reporting_state_is_disabled_after_first_irq() {
        assert!(previous_reporting(false));
        assert!(!previous_reporting(true));
    }

    #[test]
    fn first_cpu_underrun_report_disables_the_per_pipe_irq() {
        let mut display = unsafe { core::mem::zeroed::<IntelDisplay>() };
        let mut crtc = unsafe { core::mem::zeroed::<IntelCrtc>() };
        unsafe {
            display.info.__device_info = (&crate::intel_display_device_upstream::XE_LPD_DISPLAY
                as *const IntelDisplayDeviceInfo)
                .cast_mut();
            display.info.__runtime_info.pipe_mask = 1;
            tk_linux_kapi::locks::spin_lock_init(&mut display.irq.lock);
            tk_linux_kapi::list::INIT_LIST_HEAD(&mut display.pipe_list);
            tk_linux_kapi::list::INIT_LIST_HEAD(&mut crtc.pipe_head);
            crtc.pipe = 0;
            crtc.cpu_fifo_underrun_disabled = true;
            tk_linux_kapi::list::list_add_tail(&mut crtc.pipe_head, &mut display.pipe_list);

            assert!(!intel_set_cpu_fifo_underrun_reporting(
                &mut display,
                0,
                true
            ));
            assert!(!crtc.cpu_fifo_underrun_disabled);
            intel_cpu_fifo_underrun_irq_handler(&mut display, 0);
            assert!(crtc.cpu_fifo_underrun_disabled);
        }
    }
}
