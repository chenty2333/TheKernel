// SPDX-License-Identifier: MIT
// Copyright © 2022-2023 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_driver.c and intel_display_driver.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_int, c_void};
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_upstream::intel_display_device_present;
use tk_drm_upstream::drm_atomic_types::DrmAtomicCommit;
use tk_drm_upstream::drm_device_types::DrmDevice;
use tk_drm_upstream::drm_mode_config_types::DrmModeConfigFuncs;
use tk_drm_upstream::drm_modeset_helper_vtables_types::DrmModeConfigHelperFuncs;
use tk_drm_upstream::drm_modeset_lock_types::DrmModesetAcquireCtx;
use tk_drm_upstream::drm_print_types::DrmPrinter;

#[inline]
fn has_display(display: *mut IntelDisplay) -> bool {
    !display.is_null() && unsafe { intel_display_device_present(display) }
}

#[inline]
fn is_err<T>(ptr: *const T) -> bool {
    let addr = ptr as usize;
    addr >= (!4095usize)
}

#[inline]
fn ptr_err<T>(ptr: *const T) -> i32 {
    ptr as isize as i32
}

const EPROBE_DEFER: i32 = 517;

// upstream: intel_display_driver.c intel_display_driver_probe_defer()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_probe_defer(pdev: *mut c_void) -> bool {
    if pdev.is_null() {
        return false;
    }
    if unsafe { crate::display_pending_stubs::kapi_display::vga_switcheroo_client_probe_defer(pdev) } {
        return true;
    }
    let privacy_screen = unsafe { crate::display_pending_stubs::kapi_display::drm_privacy_screen_get(pdev, core::ptr::null()) };
    if is_err(privacy_screen) && ptr_err(privacy_screen) == -EPROBE_DEFER {
        return true;
    }
    if !is_err(privacy_screen) && !privacy_screen.is_null() {
        unsafe { crate::display_pending_stubs::kapi_display::drm_privacy_screen_put(privacy_screen) };
    }
    false
}

// upstream: intel_display_driver.c intel_display_driver_init_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_init_hw(display: *mut IntelDisplay) {
    if !has_display(display) {
        return;
    }
    unsafe {
        crate::display_pending_stubs::b3e::intel_cdclk_read_hw(display);
        crate::intel_display_wa_upstream::intel_display_wa_apply(display);
    }
}

static INTEL_MODE_FUNCS: DrmModeConfigFuncs = DrmModeConfigFuncs {
    fb_create: Some(crate::display_pending_stubs::b4::intel_user_framebuffer_create),
    get_format_info: Some(crate::display_pending_stubs::b4::intel_fb_get_format_info),
    mode_valid: Some(crate::display_pending_stubs::b7::intel_mode_valid),
    atomic_check: Some(crate::display_pending_stubs::b4::intel_atomic_check),
    atomic_commit: Some(crate::display_pending_stubs::b4::intel_atomic_commit),
    atomic_state_alloc: Some(crate::display_pending_stubs::b4::intel_atomic_state_alloc),
    atomic_state_clear: Some(crate::display_pending_stubs::b4::intel_atomic_state_clear),
    atomic_state_free: Some(crate::display_pending_stubs::b4::intel_atomic_state_free),
};

static INTEL_MODE_CONFIG_FUNCS: DrmModeConfigHelperFuncs = DrmModeConfigHelperFuncs {
    atomic_commit_tail: None,
    atomic_commit_setup: Some(crate::display_pending_stubs::kapi_display::drm_dp_mst_atomic_setup_commit),
};

// upstream: intel_display_driver.c intel_mode_config_init()
unsafe fn intel_mode_config_init(display: *mut IntelDisplay) {
    let mode_config = unsafe { core::ptr::addr_of_mut!((*(*display).drm).mode_config) };

    unsafe {
        tk_drm_upstream::drm_mode_config_upstream::drmm_mode_config_init((*display).drm);
        tk_linux_kapi::list::INIT_LIST_HEAD(core::ptr::addr_of_mut!((*display).global.obj_list));
        tk_linux_kapi::list::INIT_LIST_HEAD(core::ptr::addr_of_mut!((*display).pipe_list));

        (*mode_config).min_width = 0;
        (*mode_config).min_height = 0;
        (*mode_config).preferred_depth = 24;
        (*mode_config).prefer_shadow = 1;
        (*mode_config).funcs = core::ptr::addr_of!(INTEL_MODE_FUNCS) as *mut _;
        (*mode_config).helper_private = core::ptr::addr_of!(INTEL_MODE_CONFIG_FUNCS) as *mut _;
        (*mode_config).async_page_flip = true;

        let ver = (*display).info.__runtime_info.ip.ver;
        if ver >= 7 {
            (*mode_config).max_width = 16384;
            (*mode_config).max_height = 16384;
        } else if ver >= 4 {
            (*mode_config).max_width = 8192;
            (*mode_config).max_height = 8192;
        } else if ver == 3 {
            (*mode_config).max_width = 4096;
            (*mode_config).max_height = 4096;
        } else {
            (*mode_config).max_width = 2048;
            (*mode_config).max_height = 2048;
        }

        crate::display_pending_stubs::b4::intel_cursor_mode_config_init(display);
    }
}

// upstream: intel_display_driver.c intel_mode_config_cleanup()
unsafe fn intel_mode_config_cleanup(display: *mut IntelDisplay) {
    unsafe {
        crate::display_pending_stubs::b3d::intel_atomic_global_obj_cleanup(display);
        tk_drm_upstream::drm_mode_config_upstream::drm_mode_config_cleanup((*display).drm);
    }
}

// upstream: intel_display_driver.c intel_plane_possible_crtcs_init()
unsafe fn intel_plane_possible_crtcs_init(_display: *mut IntelDisplay) {}

// upstream: intel_display_driver.c intel_display_driver_early_probe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_early_probe(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe { crate::display_pending_stubs::b3b::intel_pch_detect(display) };
    if !has_display(display) {
        return;
    }

    unsafe {
        tk_linux_kapi::locks::spin_lock_init(&mut (*display).fb_tracking.lock);
        tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*display).backlight.lock));
        tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*display).audio.mutex));
        tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*display).wm.wm_mutex));
        tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*display).pps.mutex));
        tk_linux_kapi::mutex::mutex_init(core::ptr::addr_of_mut!((*display).hdcp.hdcp_mutex));

        crate::display_pending_stubs::b3d::intel_display_irq_init(display);
        crate::display_pending_stubs::b5::intel_dkl_phy_init(display);
        crate::intel_color_upstream::intel_color_init_hooks(display);
        crate::display_pending_stubs::b3e::intel_init_cdclk_hooks(display);
        crate::display_pending_stubs::opt::intel_audio_hooks_init(display);
        crate::display_pending_stubs::b5::intel_dpll_init_clock_hook(display);
        crate::display_pending_stubs::b7::intel_init_display_hooks(display);
        crate::display_pending_stubs::opt::intel_fdi_init_hook(display);
        crate::display_pending_stubs::b3c::intel_dmc_wl_init(display);
    }
}

const WQ_UNBOUND: u32 = 1 << 1;
const WQ_HIGHPRI: u32 = 1 << 4;
const WQ_PERCPU: u32 = 1 << 8;
const __WQ_ORDERED: u32 = 1 << 17;
const WQ_UNBOUND_MAX_ACTIVE: c_int = 512;
const ENOMEM: c_int = 12;

#[inline]
unsafe fn alloc_ordered_workqueue(fmt: *const c_char, flags: u32) -> *mut c_void {
    tk_linux_kapi::workqueue::alloc_workqueue(fmt, WQ_UNBOUND | __WQ_ORDERED | flags, 1)
}

#[inline]
fn current_task_ptr() -> *mut c_void {
    axhal::percpu::current_task_ptr::<()>().cast_mut().cast()
}

// upstream: intel_display_driver.c intel_display_driver_probe_noirq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_probe_noirq(display: *mut IntelDisplay) -> c_int {
    if display.is_null() {
        return 0;
    }
    if has_display(display) {
        let num_pipes = unsafe { (*display).info.__runtime_info.pipe_mask.count_ones() };
        let ret = unsafe { tk_drm_upstream::drm_vblank_upstream::drm_vblank_init((*display).drm, num_pipes) };
        if ret != 0 {
            return ret;
        }
    }

    unsafe {
        crate::display_pending_stubs::b3c::intel_bios_init(display);
        crate::display_pending_stubs::opt::intel_psr_dc5_dc6_wa_init(display);

        let mut ret = crate::display_pending_stubs::b3b::intel_display_power_init(display);
        if ret < 0 {
            crate::display_pending_stubs::b3c::intel_bios_driver_remove(display);
            return ret;
        }

        crate::display_pending_stubs::b3b::intel_pmdemand_init_early(display);
        crate::display_pending_stubs::b3b::intel_display_power_init_hw(display);

        if !has_display(display) {
            return 0;
        }

        (*display).hotplug.dp_wq = alloc_ordered_workqueue(c"intel-dp".as_ptr(), 0);
        if (*display).hotplug.dp_wq.is_null() {
            ret = -ENOMEM;
            crate::display_pending_stubs::b3c::intel_dmc_fini(display);
            crate::display_pending_stubs::b3b::intel_display_power_driver_remove(display);
            crate::display_pending_stubs::b3c::intel_bios_driver_remove(display);
            return ret;
        }

        (*display).wq.modeset = alloc_ordered_workqueue(c"i915_modeset".as_ptr(), 0);
        if (*display).wq.modeset.is_null() {
            ret = -ENOMEM;
            tk_linux_kapi::workqueue::destroy_workqueue((*display).hotplug.dp_wq);
            crate::display_pending_stubs::b3c::intel_dmc_fini(display);
            crate::display_pending_stubs::b3b::intel_display_power_driver_remove(display);
            crate::display_pending_stubs::b3c::intel_bios_driver_remove(display);
            return ret;
        }

        (*display).wq.flip = tk_linux_kapi::workqueue::alloc_workqueue(
            c"i915_flip".as_ptr(),
            WQ_HIGHPRI | WQ_UNBOUND,
            WQ_UNBOUND_MAX_ACTIVE,
        );
        if (*display).wq.flip.is_null() {
            ret = -ENOMEM;
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.modeset);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).hotplug.dp_wq);
            crate::display_pending_stubs::b3c::intel_dmc_fini(display);
            crate::display_pending_stubs::b3b::intel_display_power_driver_remove(display);
            crate::display_pending_stubs::b3c::intel_bios_driver_remove(display);
            return ret;
        }

        (*display).wq.cleanup = tk_linux_kapi::workqueue::alloc_workqueue(
            c"i915_cleanup".as_ptr(),
            WQ_HIGHPRI | WQ_PERCPU,
            0,
        );
        if (*display).wq.cleanup.is_null() {
            ret = -ENOMEM;
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.flip);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.modeset);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).hotplug.dp_wq);
            crate::display_pending_stubs::b3c::intel_dmc_fini(display);
            crate::display_pending_stubs::b3b::intel_display_power_driver_remove(display);
            crate::display_pending_stubs::b3c::intel_bios_driver_remove(display);
            return ret;
        }

        (*display).wq.unordered = tk_linux_kapi::workqueue::alloc_workqueue(
            c"display_unordered".as_ptr(),
            WQ_PERCPU,
            0,
        );
        if (*display).wq.unordered.is_null() {
            ret = -ENOMEM;
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.cleanup);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.flip);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.modeset);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).hotplug.dp_wq);
            crate::display_pending_stubs::b3c::intel_dmc_fini(display);
            crate::display_pending_stubs::b3b::intel_display_power_driver_remove(display);
            crate::display_pending_stubs::b3c::intel_bios_driver_remove(display);
            return ret;
        }

        crate::display_pending_stubs::b3c::intel_dmc_init(display);
        intel_mode_config_init(display);

        let cleanup_wqs = |ret: c_int| -> c_int {
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.unordered);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.cleanup);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.flip);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.modeset);
            tk_linux_kapi::workqueue::destroy_workqueue((*display).hotplug.dp_wq);
            crate::display_pending_stubs::b3c::intel_dmc_fini(display);
            crate::display_pending_stubs::b3b::intel_display_power_driver_remove(display);
            crate::display_pending_stubs::b3c::intel_bios_driver_remove(display);
            ret
        };

        ret = crate::display_pending_stubs::b3e::intel_cdclk_init(display);
        if ret != 0 { return cleanup_wqs(ret); }
        ret = crate::intel_color_upstream::intel_color_init(display);
        if ret != 0 { return cleanup_wqs(ret); }
        ret = crate::display_pending_stubs::b3d::intel_dbuf_init(display);
        if ret != 0 { return cleanup_wqs(ret); }
        ret = crate::display_pending_stubs::b3d::intel_dbuf_bw_init(display);
        if ret != 0 { return cleanup_wqs(ret); }
        ret = crate::display_pending_stubs::b3d::intel_bw_init(display);
        if ret != 0 { return cleanup_wqs(ret); }
        ret = crate::display_pending_stubs::b3b::intel_pmdemand_init(display);
        if ret != 0 { return cleanup_wqs(ret); }

        crate::intel_quirks_upstream::intel_init_quirks(display);
        crate::display_pending_stubs::opt::intel_fbc_init(display);
    }

    0
}

// upstream: intel_display_driver.c set_display_access()
unsafe fn set_display_access(
    display: *mut IntelDisplay,
    any_task_allowed: bool,
    allowed_task: *mut c_void,
) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    let mut ctx: DrmModesetAcquireCtx = unsafe { core::mem::zeroed() };
    unsafe {
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_acquire_init(core::ptr::addr_of_mut!(ctx), 0);
        loop {
            let err = tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_lock_all_ctx(
                (*display).drm,
                core::ptr::addr_of_mut!(ctx),
            );
            if err == -tk_linux_kapi::config::EDEADLK {
                tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_backoff(core::ptr::addr_of_mut!(ctx));
                continue;
            }
            if err == 0 {
                (*display).access.any_task_allowed = any_task_allowed;
                (*display).access.allowed_task = allowed_task;
            }
            break;
        }
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_drop_locks(core::ptr::addr_of_mut!(ctx));
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_acquire_fini(core::ptr::addr_of_mut!(ctx));
    }
}

// upstream: intel_display_driver.c intel_display_driver_enable_user_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_enable_user_access(display: *mut IntelDisplay) {
    unsafe {
        set_display_access(display, true, core::ptr::null_mut());
        crate::display_pending_stubs::b6::intel_hpd_enable_detection_work(display);
    }
}

// upstream: intel_display_driver.c intel_display_driver_disable_user_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_disable_user_access(display: *mut IntelDisplay) {
    unsafe {
        crate::display_pending_stubs::b6::intel_hpd_disable_detection_work(display);
        set_display_access(display, false, current_task_ptr());
    }
}

// upstream: intel_display_driver.c intel_display_driver_suspend_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_suspend_access(display: *mut IntelDisplay) {
    unsafe { set_display_access(display, false, core::ptr::null_mut()) };
}

// upstream: intel_display_driver.c intel_display_driver_resume_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_resume_access(display: *mut IntelDisplay) {
    unsafe { set_display_access(display, false, current_task_ptr()) };
}

// upstream: intel_display_driver.c intel_display_driver_check_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_check_access(display: *mut IntelDisplay) -> bool {
    if display.is_null() {
        return false;
    }
    let cur = current_task_ptr();
    unsafe {
        if (*display).access.any_task_allowed || (*display).access.allowed_task == cur {
            return true;
        }
    }
    false
}

// upstream: intel_display_driver.c intel_display_driver_probe_nogem()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_probe_nogem(display: *mut IntelDisplay) -> c_int {
    if !has_display(display) {
        return 0;
    }
    unsafe {
        crate::display_pending_stubs::b4::intel_wm_init(display);
        crate::display_pending_stubs::b6::intel_panel_sanitize_ssc(display);
        crate::display_pending_stubs::b6::intel_pps_setup(display);
        crate::display_pending_stubs::b6::intel_gmbus_setup(display);

        let ret = crate::display_pending_stubs::b4::intel_crtc_init(display);
        if ret != 0 {
            intel_mode_config_cleanup(display);
            return ret;
        }

        intel_plane_possible_crtcs_init(display);
        crate::display_pending_stubs::b5::intel_dpll_init(display);
        crate::display_pending_stubs::opt::intel_fdi_pll_freq_update(display);
        intel_display_driver_init_hw(display);
        crate::display_pending_stubs::b5::intel_dpll_update_ref_clks(display);

        if (*display).cdclk.max_cdclk_freq == 0 {
            crate::display_pending_stubs::b3e::intel_update_max_cdclk(display);
        }

        crate::display_pending_stubs::b3b::intel_hti_init(display);
        crate::display_pending_stubs::b7::intel_setup_outputs(display);

        let ret = crate::display_pending_stubs::opt::intel_dp_tunnel_mgr_init(display);
        if ret != 0 {
            crate::display_pending_stubs::opt::intel_hdcp_component_fini(display);
            intel_mode_config_cleanup(display);
            return ret;
        }

        intel_display_driver_disable_user_access(display);
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_lock_all((*display).drm);
        crate::display_pending_stubs::b7::intel_modeset_setup_hw_state(
            display,
            (*(*display).drm).mode_config.acquire_ctx,
        );
        crate::display_pending_stubs::opt::intel_acpi_assign_connector_fwnodes(display);
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_unlock_all((*display).drm);

        crate::display_pending_stubs::b4::intel_initial_plane_config(display);
        crate::display_pending_stubs::b4::ilk_wm_sanitize(display);
    }
    0
}

// upstream: intel_display_driver.c intel_display_driver_probe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_probe(display: *mut IntelDisplay) -> c_int {
    if !has_display(display) {
        return 0;
    }
    unsafe {
        crate::display_pending_stubs::opt::intel_hdcp_component_init(display);
        crate::display_pending_stubs::opt::intel_flipq_init(display);
        let _ = crate::display_pending_stubs::b7::intel_initial_commit(display);
        crate::display_pending_stubs::opt::intel_overlay_setup(display);
        crate::display_pending_stubs::b6::intel_hpd_init(display);
        crate::display_pending_stubs::b4::skl_watermark_ipc_init(display);
    }
    0
}

// upstream: intel_display_driver.c intel_display_driver_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_register(display: *mut IntelDisplay) {
    if !has_display(display) {
        return;
    }
    unsafe {
        crate::display_pending_stubs::opt::intel_vga_register(display);
        crate::display_pending_stubs::opt::intel_opregion_register(display);
        crate::display_pending_stubs::opt::intel_acpi_video_register(display);
        crate::display_pending_stubs::opt::intel_audio_init(display);
        intel_display_driver_enable_user_access(display);
        crate::display_pending_stubs::opt::intel_audio_register(display);
        crate::display_pending_stubs::opt::intel_display_debugfs_register(display);
        crate::display_pending_stubs::kapi_display::drm_kms_helper_poll_init((*display).drm);
        crate::display_pending_stubs::b6::intel_hpd_poll_disable(display);
        crate::display_pending_stubs::b7::intel_fbdev_setup(display);
        crate::display_pending_stubs::opt::intel_register_dsm_handler();
    }
}

// upstream: intel_display_driver.c intel_display_driver_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_remove(display: *mut IntelDisplay) {
    if !has_display(display) {
        return;
    }
    unsafe {
        tk_linux_kapi::workqueue::flush_workqueue((*display).wq.flip);
        tk_linux_kapi::workqueue::flush_workqueue((*display).wq.modeset);
        tk_linux_kapi::workqueue::flush_workqueue((*display).wq.cleanup);
        tk_linux_kapi::workqueue::flush_workqueue((*display).wq.unordered);
        crate::display_pending_stubs::opt::intel_dp_mst_suspend(display);
    }
}

// upstream: intel_display_driver.c intel_display_driver_remove_noirq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_remove_noirq(display: *mut IntelDisplay) {
    if !has_display(display) {
        return;
    }
    unsafe {
        intel_display_driver_suspend_access(display);
        crate::display_pending_stubs::b6::intel_hpd_poll_fini(display);
        crate::display_pending_stubs::opt::intel_unregister_dsm_handler();
        tk_linux_kapi::workqueue::flush_workqueue((*display).wq.unordered);
        crate::display_pending_stubs::opt::intel_hdcp_component_fini(display);
        intel_mode_config_cleanup(display);
        crate::display_pending_stubs::opt::intel_dp_tunnel_mgr_cleanup(display);
        crate::display_pending_stubs::opt::intel_overlay_cleanup(display);
        crate::display_pending_stubs::b6::intel_gmbus_teardown(display);
        tk_linux_kapi::workqueue::destroy_workqueue((*display).hotplug.dp_wq);
        tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.flip);
        tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.modeset);
        tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.cleanup);
        tk_linux_kapi::workqueue::destroy_workqueue((*display).wq.unordered);
        crate::display_pending_stubs::opt::intel_fbc_cleanup(display);
    }
}

// upstream: intel_display_driver.c intel_display_driver_remove_nogem()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_remove_nogem(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        crate::display_pending_stubs::b3c::intel_dmc_fini(display);
        crate::display_pending_stubs::b3b::intel_display_power_driver_remove(display);
        crate::display_pending_stubs::b3c::intel_bios_driver_remove(display);
    }
}

// upstream: intel_display_driver.c intel_display_driver_unregister()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_unregister(display: *mut IntelDisplay) {
    if !has_display(display) {
        return;
    }
    unsafe {
        crate::display_pending_stubs::opt::intel_unregister_dsm_handler();
        crate::display_pending_stubs::kapi_display::drm_client_dev_unregister((*display).drm);
        crate::display_pending_stubs::kapi_display::drm_kms_helper_poll_fini((*display).drm);
        intel_display_driver_disable_user_access(display);
        crate::display_pending_stubs::opt::intel_audio_deinit(display);
        crate::display_pending_stubs::kapi_display::drm_atomic_helper_shutdown((*display).drm);
        crate::display_pending_stubs::kapi_display::acpi_video_unregister();
        crate::display_pending_stubs::opt::intel_opregion_unregister(display);
        crate::display_pending_stubs::opt::intel_vga_unregister(display);
    }
}

// upstream: intel_display_driver.c intel_display_driver_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_suspend(display: *mut IntelDisplay) -> c_int {
    if !has_display(display) {
        return 0;
    }
    unsafe extern "C" {
        fn drm_atomic_helper_suspend(dev: *mut DrmDevice) -> *mut DrmAtomicCommit;
    }
    let state = unsafe { drm_atomic_helper_suspend((*display).drm) };
    if is_err(state) {
        return -1;
    }
    unsafe {
        (*display).restore.modeset_state = state;
        crate::display_pending_stubs::opt::intel_dp_mst_suspend(display);
    }
    0
}

// upstream: intel_display_driver.c __intel_display_driver_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_display_driver_resume(
    display: *mut IntelDisplay,
    state: *mut DrmAtomicCommit,
    ctx: *mut DrmModesetAcquireCtx,
) -> c_int {
    unsafe {
        crate::display_pending_stubs::b7::intel_modeset_setup_hw_state(display, ctx);
    }
    if state.is_null() {
        return 0;
    }
    unsafe { crate::display_pending_stubs::kapi_display::drm_atomic_helper_commit_duplicated_state(state, ctx) }
}

// upstream: intel_display_driver.c intel_display_driver_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_driver_resume(display: *mut IntelDisplay) {
    if !has_display(display) {
        return;
    }
    let state = unsafe { (*display).restore.modeset_state };
    unsafe {
        crate::display_pending_stubs::opt::intel_dp_mst_resume(display);
        (*display).restore.modeset_state = core::ptr::null_mut();
    }

    let mut ctx: DrmModesetAcquireCtx = unsafe { core::mem::zeroed() };
    unsafe {
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_acquire_init(core::ptr::addr_of_mut!(ctx), 0);
        loop {
            let ret = tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_lock_all_ctx(
                (*display).drm,
                core::ptr::addr_of_mut!(ctx),
            );
            if ret != -tk_linux_kapi::config::EDEADLK {
                break;
            }
            tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_backoff(core::ptr::addr_of_mut!(ctx));
        }

        let _ = __intel_display_driver_resume(display, state, core::ptr::addr_of_mut!(ctx));
        crate::display_pending_stubs::b4::skl_watermark_ipc_update(display);
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_drop_locks(core::ptr::addr_of_mut!(ctx));
        tk_drm_upstream::drm_modeset_lock_upstream::drm_modeset_acquire_fini(core::ptr::addr_of_mut!(ctx));

        if !state.is_null() {
            crate::display_pending_stubs::kapi_display::drm_atomic_commit_put(state);
        }
    }
}
