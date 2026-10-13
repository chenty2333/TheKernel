// SPDX-License-Identifier: MIT
// Unit tests for Batch B3a: i915 display foundation (Display 13 / ADL-P)

#![allow(unsafe_code, non_snake_case, dead_code)]

extern crate alloc;
extern crate std;

use alloc::boxed::Box;
use alloc::vec::Vec;
use std::sync::Mutex;

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_types_upstream::*;
use crate::intel_display_device_upstream::*;
use crate::intel_display_params_types_upstream::IntelDisplayParams;
use crate::intel_display_params_upstream::*;
use crate::intel_display_regs_upstream::*;
use crate::intel_display_wa_upstream::*;
use crate::intel_display_clock_gating_upstream::*;
use crate::intel_de_upstream::*;
use crate::intel_quirks_upstream::*;
use crate::intel_uncore_types_upstream::*;
use crate::intel_uncore_upstream::I915_UNCORE_OFFSET;
use crate::intel_workarounds_types_upstream::I915RegT;
use tk_drm_upstream::drm_device_types::DrmDevice;

const MMIO_SPACE_SIZE: usize = 0x100000; // 1MB

static MMIO_WRITE_LOG: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

unsafe extern "C" fn mock_mmio_readb(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> u8 {
    let offset = reg.reg as usize;
    if offset < MMIO_SPACE_SIZE {
        core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset))
    } else {
        0
    }
}

unsafe extern "C" fn mock_mmio_writeb(uncore: *mut IntelUncore, reg: I915RegT, val: u8, _trace: bool) {
    let offset = reg.reg as usize;
    if offset < MMIO_SPACE_SIZE {
        core::ptr::write_volatile((*uncore).regs.cast::<u8>().add(offset), val);
    }
}

unsafe extern "C" fn mock_mmio_readw(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> u16 {
    let offset = reg.reg as usize;
    if offset + 2 <= MMIO_SPACE_SIZE {
        core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u16>())
    } else {
        0
    }
}

unsafe extern "C" fn mock_mmio_writew(uncore: *mut IntelUncore, reg: I915RegT, val: u16, _trace: bool) {
    let offset = reg.reg as usize;
    if offset + 2 <= MMIO_SPACE_SIZE {
        core::ptr::write_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u16>(), val);
    }
}

unsafe extern "C" fn mock_mmio_readl(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> u32 {
    let offset = reg.reg as usize;
    if offset + 4 <= MMIO_SPACE_SIZE {
        core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u32>())
    } else {
        0
    }
}

unsafe extern "C" fn mock_mmio_writel(uncore: *mut IntelUncore, reg: I915RegT, val: u32, _trace: bool) {
    let offset = reg.reg as usize;
    if let Ok(mut log) = MMIO_WRITE_LOG.lock() {
        log.push((reg.reg, val));
    }
    if offset + 4 <= MMIO_SPACE_SIZE {
        core::ptr::write_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u32>(), val);
    }
}

unsafe extern "C" fn mock_mmio_readq(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> u64 {
    let offset = reg.reg as usize;
    if offset + 8 <= MMIO_SPACE_SIZE {
        let low = core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u32>()) as u64;
        let high = core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset + 4).cast::<u32>()) as u64;
        (high << 32) | low
    } else {
        0
    }
}

struct MockEnvironment {
    _drm_storage: Box<[u8]>,
    mmio_storage: Box<[u8]>,
    display: Box<IntelDisplay>,
}

impl MockEnvironment {
    fn new() -> Self {
        let mut drm_storage = alloc::vec![0u8; I915_UNCORE_OFFSET + core::mem::size_of::<IntelUncore>() + 128].into_boxed_slice();
        let mut mmio_storage = alloc::vec![0u8; MMIO_SPACE_SIZE].into_boxed_slice();
        let mut display = Box::new(unsafe { core::mem::zeroed::<IntelDisplay>() });

        let uncore_ptr = unsafe {
            drm_storage.as_mut_ptr().add(I915_UNCORE_OFFSET).cast::<IntelUncore>()
        };

        unsafe {
            (*uncore_ptr).regs = mmio_storage.as_mut_ptr().cast();
            (*uncore_ptr).funcs.mmio_readb = Some(mock_mmio_readb);
            (*uncore_ptr).funcs.mmio_writeb = Some(mock_mmio_writeb);
            (*uncore_ptr).funcs.mmio_readw = Some(mock_mmio_readw);
            (*uncore_ptr).funcs.mmio_writew = Some(mock_mmio_writew);
            (*uncore_ptr).funcs.mmio_readl = Some(mock_mmio_readl);
            (*uncore_ptr).funcs.mmio_writel = Some(mock_mmio_writel);
            (*uncore_ptr).funcs.mmio_readq = Some(mock_mmio_readq);

            let drm_ptr = drm_storage.as_mut_ptr().cast::<DrmDevice>();
            display.drm = drm_ptr;
        }

        Self {
            _drm_storage: drm_storage,
            mmio_storage,
            display,
        }
    }

    fn display_ptr(&mut self) -> *mut IntelDisplay {
        &mut *self.display
    }

    fn write_mmio32(&mut self, reg: u32, val: u32) {
        let offset = reg as usize;
        assert!(offset + 4 <= MMIO_SPACE_SIZE);
        unsafe {
            core::ptr::write_volatile(self.mmio_storage.as_mut_ptr().add(offset).cast::<u32>(), val);
        }
    }

    fn read_mmio32(&self, reg: u32) -> u32 {
        let offset = reg as usize;
        assert!(offset + 4 <= MMIO_SPACE_SIZE);
        unsafe {
            core::ptr::read_volatile(self.mmio_storage.as_ptr().add(offset).cast::<u32>())
        }
    }
}

#[test]
fn test_display_device_probe_adlp_adln() {
    let parent = unsafe { core::mem::zeroed::<crate::intel_parent_upstream::IntelDisplayParentInterface>() };

    // Test 1a: Alderlake-P (0x46A6) probe
    let mut pdev_adlp = PciDev {
        _pad_0: [0; 56],
        devfn: 0,
        vendor: 0x8086,
        device: 0x46A6,
        subsystem_vendor: 0x8086,
        subsystem_device: 0x0001,
        class: 0x030000,
        revision: 0,
    };

    let display_adlp = unsafe { intel_display_device_probe(&mut pdev_adlp, &parent) };
    assert!(!display_adlp.is_null());
    unsafe {
        assert_eq!((*display_adlp).info.__device_info, core::ptr::addr_of!(XE_LPD_DISPLAY) as *mut _);
        let runtime = &(*display_adlp).info.__runtime_info;
        assert_eq!(runtime.ip.ver, 13);
        assert_eq!(runtime.pipe_mask, 0x0f); // Pipes A, B, C, D
        assert_eq!(runtime.cpu_transcoder_mask, 0x6f); // Transcoders A-D, DSI 0, DSI 1
        assert_eq!(runtime.port_mask, 0x7b); // Ports A, B, TC1-TC4
        assert!(runtime.has_hdcp);
        assert!(runtime.has_dmc);
        assert!(runtime.has_dsc);
        assert_eq!(runtime.fbc_mask, 1);
        // ADL-P platform bit 52
        assert!((*display_adlp).platform._anon_0_0.bitmap[0] & (1u64 << 52) != 0);

        intel_display_device_remove(display_adlp);
    }

    // Test 1b: Alderlake-N (0x46D0, N305) probe
    let mut pdev_adln = PciDev {
        _pad_0: [0; 56],
        devfn: 0,
        vendor: 0x8086,
        device: 0x46D0,
        subsystem_vendor: 0x8086,
        subsystem_device: 0x0002,
        class: 0x030000,
        revision: 0,
    };

    let display_adln = unsafe { intel_display_device_probe(&mut pdev_adln, &parent) };
    assert!(!display_adln.is_null());
    unsafe {
        assert_eq!((*display_adln).info.__device_info, core::ptr::addr_of!(XE_LPD_DISPLAY) as *mut _);
        let runtime = &(*display_adln).info.__runtime_info;
        assert_eq!(runtime.ip.ver, 13);
        assert_eq!(runtime.pipe_mask, 0x0f);
        assert_eq!(runtime.cpu_transcoder_mask, 0x6f);
        assert_eq!(runtime.port_mask, 0x7b);
        assert!(runtime.has_hdcp);
        assert!(runtime.has_dmc);
        assert!(runtime.has_dsc);
        // ADL-P platform bit 52 and ADL-N subplatform bit 53
        assert!((*display_adln).platform._anon_0_0.bitmap[0] & (1u64 << 52) != 0);
        assert!((*display_adln).platform._anon_0_0.bitmap[0] & (1u64 << 53) != 0);

        intel_display_device_remove(display_adln);
    }
}

#[test]
fn test_display_device_runtime_fusing() {
    let mut env = MockEnvironment::new();
    let display = env.display_ptr();

    unsafe {
        (*display).info.__device_info = core::ptr::addr_of!(XE_LPD_DISPLAY) as *mut _;
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(XE_LPD_DISPLAY.__runtime_defaults),
            core::ptr::addr_of_mut!((*display).info.__runtime_info),
            1,
        );
        (*display).platform._anon_0_0.bitmap[0] = 1u64 << 52; // ADL-P
    }

    // Mock fuse register SKL_DFSM to disable pipe C, pipe D, HDCP, DMC
    env.write_mmio32(
        SKL_DFSM.reg,
        SKL_DFSM_PIPE_C_DISABLE | TGL_DFSM_PIPE_D_DISABLE | SKL_DFSM_DISPLAY_HDCP_DISABLE | ICL_DFSM_DMC_DISABLE,
    );

    unsafe { __intel_display_device_info_runtime_init(display) };

    unsafe {
        let runtime = &(*display).info.__runtime_info;
        assert_eq!(runtime.pipe_mask, 0x03); // Pipes C and D disabled, leaving A and B
        assert_eq!(runtime.cpu_transcoder_mask, 0x63); // Transcoders C and D disabled
        assert_eq!(runtime.fbc_mask, 0x01); // FBC A remains
        assert!(!runtime.has_hdcp, "HDCP should be fused off");
        assert!(!runtime.has_dmc, "DMC should be fused off");
        assert!(runtime.has_dsc, "DSC should remain present on display 13");
    }
}

#[test]
fn test_intel_de_helpers_mock_uncore() {
    let mut env = MockEnvironment::new();
    let display = env.display_ptr();
    let reg32 = I915RegT { reg: 0x42000 };

    // 8-bit read/write
    unsafe {
        intel_de_write8(display, reg32, 0xa5);
        assert_eq!(intel_de_read8(display, reg32), 0xa5);
    }

    // 16-bit read
    unsafe {
        env.write_mmio32(reg32.reg, 0xbeef);
        assert_eq!(intel_de_read16(display, reg32), 0xbeef);
    }

    // 32-bit read/write
    unsafe {
        intel_de_write(display, reg32, 0x12345678);
        assert_eq!(intel_de_read(display, reg32), 0x12345678);
    }

    // 64-bit 2x32 read
    let reg64 = I915RegT { reg: 0x42010 };
    unsafe {
        env.write_mmio32(reg64.reg, 0x11112222);
        env.write_mmio32(reg64.reg + 4, 0x33334444);
        assert_eq!(intel_de_read64_2x32(display, reg64), 0x3333444411112222);
    }

    // RMW
    unsafe {
        intel_de_write(display, reg32, 0x00ff00ff);
        let old = intel_de_rmw(display, reg32, 0x000000ff, 0x0000aa00);
        assert_eq!(old, 0x00ff00ff);
        assert_eq!(intel_de_read(display, reg32), 0x00ffaa00);
    }

    // Firmware / raw read/write/rmw
    let reg_fw = I915RegT { reg: 0x42020 };
    unsafe {
        intel_de_write_fw(display, reg_fw, 0xca11ab1e);
        assert_eq!(intel_de_read_fw(display, reg_fw), 0xca11ab1e);
        let old_fw = intel_de_rmw_fw(display, reg_fw, 0x1e, 0x2f);
        assert_eq!(old_fw, 0xca11ab1e);
        assert_eq!(intel_de_read_fw(display, reg_fw), 0xca11ab2f);
    }

    // Wait helpers: already satisfied -> return 0
    unsafe {
        intel_de_write(display, reg32, 0x80);
        assert_eq!(intel_de_wait_for_set_us(display, reg32, 0x80, 100), 0);
        assert_eq!(intel_de_wait_for_clear_us(display, reg32, 0x01, 100), 0);
    }

    // Wait helpers: unsatisfied -> timeout returns -ETIMEDOUT (-110)
    unsafe {
        intel_de_write(display, reg32, 0x00);
        let ret_set = intel_de_wait_for_set_us(display, reg32, 0x80, 2);
        assert_eq!(ret_set, -crate::linux_config::ETIMEDOUT);

        intel_de_write(display, reg32, 0x80);
        let ret_clear = intel_de_wait_for_clear_us(display, reg32, 0x80, 2);
        assert_eq!(ret_clear, -crate::linux_config::ETIMEDOUT);
    }
}

#[test]
fn test_display_params_defaults() {
    let mut params: IntelDisplayParams = unsafe { core::mem::zeroed() };
    unsafe { intel_display_params_copy(&mut params) };

    assert_eq!(params.panel_use_ssc, -1);
    assert_eq!(params.vbt_sdvo_panel_type, -1);
    assert_eq!(params.enable_dc, -1);
    assert!(params.enable_dpt);
    assert!(params.enable_dsb);
    assert!(!params.enable_flipq);
    assert!(params.enable_sagv);
    assert_eq!(params.disable_power_well, -1);
    assert!(params.enable_ips);
    assert_eq!(params.invert_brightness, 0);
    assert_eq!(params.edp_vswing, 0);
    assert_eq!(params.enable_dpcd_backlight, -1);
    assert!(!params.load_detect_test);
    assert!(!params.force_reset_modeset_test);
    assert!(!params.disable_display);
    assert!(params.verbose_state_checks);
    assert!(!params.nuclear_pageflip);
    assert!(params.enable_dp_mst);
    assert_eq!(params.enable_fbc, -1);
    assert_eq!(params.enable_psr, -1);
    assert_eq!(params.enable_panel_replay, -1);
    assert!(!params.psr_safest_params);
    assert!(params.enable_psr2_sel_fetch);
    assert_eq!(params.enable_dmc_wl, -1);

    unsafe { intel_display_params_free(&mut params) };
}

#[test]
fn test_quirk_matching() {
    let mut env = MockEnvironment::new();
    let display = env.display_ptr();

    // Acer Aspire 5734Z: device 0x2a42, subvendor 0x1025, subdevice 0x0459
    let mut pdev = PciDev {
        _pad_0: [0; 56],
        devfn: 0,
        vendor: 0x8086,
        device: 0x2A42,
        subsystem_vendor: 0x1025,
        subsystem_device: 0x0459,
        class: 0x030000,
        revision: 0,
    };

    unsafe {
        (*(*display).drm).dev = core::ptr::addr_of_mut!(pdev).cast();
        intel_init_quirks(display);
        assert!(intel_has_quirk(display, IntelQuirkId::QuirkInvertBrightness));
        assert!(!intel_has_quirk(display, IntelQuirkId::QuirkBacklightPresent));
    }
}

#[test]
fn test_clock_gating_adlp_register_writes() {
    let mut env = MockEnvironment::new();
    let display = env.display_ptr();

    unsafe {
        (*display).platform._anon_0_0.bitmap[0] = 1u64 << 52; // Alderlake-P
    }

    env.write_mmio32(GEN9_CLKGATE_DIS_5.reg, 0);
    env.write_mmio32(GEN8_CHICKEN_DCPR_1.reg, DDI_CLOCK_REG_ACCESS);

    if let Ok(mut log) = MMIO_WRITE_LOG.lock() {
        log.clear();
    }

    // Call ADL-P clock gating init
    unsafe { intel_display_adlp_init_clock_gating(display) };

    // Verify ADL-P register modifications:
    // 1. GEN9_CLKGATE_DIS_5 has DPCE_GATING_DIS set
    assert_eq!(
        env.read_mmio32(GEN9_CLKGATE_DIS_5.reg) & DPCE_GATING_DIS,
        DPCE_GATING_DIS
    );

    // 2. GEN8_CHICKEN_DCPR_1 has DDI_CLOCK_REG_ACCESS cleared
    assert_eq!(
        env.read_mmio32(GEN8_CHICKEN_DCPR_1.reg) & DDI_CLOCK_REG_ACCESS,
        0
    );

    // Verify log recorded writes to both registers
    let writes = MMIO_WRITE_LOG.lock().unwrap().clone();
    let wrote_clkgate = writes.iter().any(|&(reg, _)| reg == GEN9_CLKGATE_DIS_5.reg);
    let wrote_dcpr = writes.iter().any(|&(reg, _)| reg == GEN8_CHICKEN_DCPR_1.reg);
    assert!(wrote_clkgate, "expected write to GEN9_CLKGATE_DIS_5");
    assert!(wrote_dcpr, "expected write to GEN8_CHICKEN_DCPR_1");

    // Also verify intel_display_gen9_init_clock_gating writes CHICKEN_PAR1_1, GEN8_CHICKEN_DCPR_1, DISP_ARB_CTL
    if let Ok(mut log) = MMIO_WRITE_LOG.lock() {
        log.clear();
    }
    unsafe { intel_display_skl_init_clock_gating(display) };
    let writes_gen9 = MMIO_WRITE_LOG.lock().unwrap().clone();
    let wrote_chicken_par1 = writes_gen9.iter().any(|&(reg, _)| reg == CHICKEN_PAR1_1.reg);
    let wrote_dcpr_gen9 = writes_gen9.iter().any(|&(reg, _)| reg == GEN8_CHICKEN_DCPR_1.reg);
    let wrote_disp_arb_ctl = writes_gen9.iter().any(|&(reg, _)| reg == 0x45000);
    assert!(wrote_chicken_par1, "expected write to CHICKEN_PAR1_1");
    assert!(wrote_dcpr_gen9, "expected write to GEN8_CHICKEN_DCPR_1");
    assert!(wrote_disp_arb_ctl, "expected write to DISP_ARB_CTL (0x45000)");
}
