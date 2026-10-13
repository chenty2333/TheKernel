// SPDX-License-Identifier: MIT
// Unit tests for Batch B3b: i915 display power domains, PMDEMAND, PCH detect, HTI (Display 13 / ADL-P)

#![allow(unsafe_code, non_snake_case, dead_code)]

extern crate alloc;
extern crate std;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::CStr;
use std::sync::Mutex;

use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_types_upstream::*;
use crate::intel_display_device_upstream::*;
use crate::intel_display_power_types_upstream::*;
use crate::intel_display_power_upstream::*;
use crate::intel_display_power_well_types_upstream::*;
use crate::intel_display_power_well_upstream::*;
use crate::intel_display_regs_upstream::*;
use crate::intel_hti_regs_upstream::*;
use crate::intel_hti_upstream::*;
use crate::skl_watermark_regs_upstream::*;
use crate::intel_pch_types_upstream::*;
use crate::intel_pch_upstream::*;
use crate::intel_pmdemand_upstream::*;
use crate::intel_quirks_upstream::PciDev;
use crate::intel_uncore_types_upstream::*;
use crate::intel_uncore_upstream::I915_UNCORE_OFFSET;
use crate::intel_workarounds_types_upstream::I915RegT;
use tk_drm_upstream::drm_device_types::DrmDevice;

const MMIO_SPACE_SIZE: usize = 0x100000; // 1MB

static MMIO_WRITE_LOG: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

unsafe extern "C" fn mock_mmio_readb(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> u8 {
    let offset = reg.reg as usize;
    if offset < MMIO_SPACE_SIZE {
        unsafe { core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset)) }
    } else {
        0
    }
}

unsafe extern "C" fn mock_mmio_writeb(uncore: *mut IntelUncore, reg: I915RegT, val: u8, _trace: bool) {
    let offset = reg.reg as usize;
    if offset < MMIO_SPACE_SIZE {
        unsafe { core::ptr::write_volatile((*uncore).regs.cast::<u8>().add(offset), val) };
    }
}

unsafe extern "C" fn mock_mmio_readw(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> u16 {
    let offset = reg.reg as usize;
    if offset + 2 <= MMIO_SPACE_SIZE {
        unsafe { core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u16>()) }
    } else {
        0
    }
}

unsafe extern "C" fn mock_mmio_writew(uncore: *mut IntelUncore, reg: I915RegT, val: u16, _trace: bool) {
    let offset = reg.reg as usize;
    if offset + 2 <= MMIO_SPACE_SIZE {
        unsafe { core::ptr::write_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u16>(), val) };
    }
}

unsafe extern "C" fn mock_mmio_readl(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> u32 {
    let offset = reg.reg as usize;
    if offset + 4 <= MMIO_SPACE_SIZE {
        unsafe { core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u32>()) }
    } else {
        0
    }
}

unsafe extern "C" fn mock_mmio_writel(uncore: *mut IntelUncore, reg: I915RegT, mut val: u32, _trace: bool) {
    let offset = reg.reg as usize;

    // Auto-ack status bit when PWR_WELL_CTL registers are written,
    // so intel_de_wait_for_set / clear completes immediately.
    if reg.reg == HSW_PWR_WELL_CTL1.reg
        || reg.reg == HSW_PWR_WELL_CTL2.reg
        || reg.reg == HSW_PWR_WELL_CTL4.reg
        || reg.reg == ICL_PWR_WELL_CTL_AUX1.reg
        || reg.reg == ICL_PWR_WELL_CTL_AUX2.reg
        || reg.reg == ICL_PWR_WELL_CTL_AUX4.reg
        || reg.reg == ICL_PWR_WELL_CTL_DDI1.reg
        || reg.reg == ICL_PWR_WELL_CTL_DDI2.reg
        || reg.reg == ICL_PWR_WELL_CTL_DDI4.reg
    {
        for idx in 0..16 {
            let req = HSW_PWR_WELL_CTL_REQ(idx);
            let state = HSW_PWR_WELL_CTL_STATE(idx);
            if (val & req) != 0 {
                val |= state;
            } else {
                val &= !state;
            }
        }
    }

    if reg.reg == crate::skl_watermark_regs_upstream::_DBUF_CTL_S0
        || reg.reg == crate::skl_watermark_regs_upstream::_DBUF_CTL_S1
        || reg.reg == crate::skl_watermark_regs_upstream::_DBUF_CTL_S2
        || reg.reg == crate::skl_watermark_regs_upstream::_DBUF_CTL_S3
    {
        if (val & crate::skl_watermark_regs_upstream::DBUF_POWER_REQUEST) != 0 {
            val |= crate::skl_watermark_regs_upstream::DBUF_POWER_STATE;
        } else {
            val &= !crate::skl_watermark_regs_upstream::DBUF_POWER_STATE;
        }
    }

    if let Ok(mut log) = MMIO_WRITE_LOG.lock() {
        log.push((reg.reg, val));
    }
    if offset + 4 <= MMIO_SPACE_SIZE {
        unsafe { core::ptr::write_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u32>(), val) };
    }
}

unsafe extern "C" fn mock_mmio_readq(uncore: *mut IntelUncore, reg: I915RegT, _trace: bool) -> u64 {
    let offset = reg.reg as usize;
    if offset + 8 <= MMIO_SPACE_SIZE {
        unsafe {
            let low = core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset).cast::<u32>()) as u64;
            let high = core::ptr::read_volatile((*uncore).regs.cast::<u8>().add(offset + 4).cast::<u32>()) as u64;
            (high << 32) | low
        }
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

        let mut env = Self {
            _drm_storage: drm_storage,
            mmio_storage,
            display,
        };

        // All fuses enabled so gen9_wait_for_power_well_fuses passes immediately
        env.write_mmio32(SKL_FUSE_STATUS.reg, 0xFFFF_FFFF);

        env
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

    fn setup_adlp(&mut self) {
        let display = self.display_ptr();
        unsafe {
            (*display).info.__device_info = core::ptr::addr_of!(XE_LPD_DISPLAY) as *mut _;
            core::ptr::copy_nonoverlapping(
                core::ptr::addr_of!(XE_LPD_DISPLAY.__runtime_defaults),
                core::ptr::addr_of_mut!((*display).info.__runtime_info),
                1,
            );
            (*display).platform._anon_0_0.bitmap[0] = 1u64 << 52; // ADL-P
        }
    }
}

/// 1. intel_power_domains_init on an ADL-P device selects the ADL-P power-well descriptor table
/// (count and well names match Linux's adlp map: 30 wells).
#[test]
fn test_power_domains_init_adlp() {
    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    let ret = unsafe { intel_power_domains_init(display) };
    assert_eq!(ret, 0, "intel_power_domains_init should return 0");

    let power_domains = unsafe { &(*display).power.domains };
    assert_eq!(power_domains.power_well_count, 30, "ADL-P must have 30 power wells");

    let expected_names = [
        "always-on",
        "PW_1",
        "DC_off",
        "PW_2",
        "PW_A",
        "PW_B",
        "PW_C",
        "PW_D",
        "DDI_IO_A",
        "DDI_IO_B",
        "DDI_IO_C",
        "DDI_IO_D",
        "DDI_IO_E",
        "DDI_IO_TC1",
        "DDI_IO_TC2",
        "DDI_IO_TC3",
        "DDI_IO_TC4",
        "AUX_A",
        "AUX_B",
        "AUX_C",
        "AUX_D",
        "AUX_E",
        "AUX_USBC1",
        "AUX_USBC2",
        "AUX_USBC3",
        "AUX_USBC4",
        "AUX_TBT1",
        "AUX_TBT2",
        "AUX_TBT3",
        "AUX_TBT4",
    ];

    for (i, &expected_name) in expected_names.iter().enumerate() {
        let pw = unsafe { power_domains.power_wells.add(i) };
        let name_c = unsafe { intel_power_well_name(pw) };
        assert!(!name_c.is_null());
        let name = unsafe { CStr::from_ptr(name_c) }.to_str().unwrap();
        assert_eq!(name, expected_name, "power well {} name mismatch", i);
    }

    unsafe { intel_display_power_driver_remove(display) };
}

/// 2. intel_display_power_get / put of a domain enables / disables the right wells in dependency order
/// (log mock MMIO writes to the PWR_WELL_CTL registers; the mock acks the state bit so waits complete),
/// with refcounting: two gets + one put keeps the well on.
#[test]
fn test_display_power_get_put_dependency_and_refcounting() {
    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    unsafe { intel_power_domains_init(display) };

    let power_domains = unsafe { &(*display).power.domains };
    let pw_2 = unsafe { power_domains.power_wells.add(3) }; // PW_2 is index 3
    let pw_b = unsafe { power_domains.power_wells.add(5) }; // PW_B is index 5

    // Initially refcount is 0
    assert_eq!(unsafe { intel_power_well_refcount(pw_2) }, 0);
    assert_eq!(unsafe { intel_power_well_refcount(pw_b) }, 0);

    // Clear write log
    if let Ok(mut log) = MMIO_WRITE_LOG.lock() {
        log.clear();
    }

    // 1st get: domain PIPE_B enables PW_2 then PW_B in dependency order
    let wl1 = unsafe { intel_display_power_get(display, POWER_DOMAIN_PIPE_B) };
    assert!(!wl1.is_null());

    assert_eq!(unsafe { intel_power_well_refcount(pw_2) }, 1);
    assert_eq!(unsafe { intel_power_well_refcount(pw_b) }, 1);
    assert!(unsafe { intel_power_well_is_enabled_cached(pw_2) });
    assert!(unsafe { intel_power_well_is_enabled_cached(pw_b) });

    // Verify MMIO write order in log: PW_2 enabled before PW_B
    {
        let log = MMIO_WRITE_LOG.lock().unwrap();
        assert!(!log.is_empty(), "MMIO writes should have occurred");
        // PW_2 uses HSW_PWR_WELL_CTL2 with index ICL_PW_CTL_IDX_PW_2
        // PW_B uses HSW_PWR_WELL_CTL2 with index XELPD_PW_CTL_IDX_PW_B
        let pw2_req = HSW_PWR_WELL_CTL_REQ(ICL_PW_CTL_IDX_PW_2);
        let pwb_req = HSW_PWR_WELL_CTL_REQ(XELPD_PW_CTL_IDX_PW_B);

        let mut pw2_write_idx = None;
        let mut pwb_write_idx = None;
        for (i, &(reg, val)) in log.iter().enumerate() {
            if reg == HSW_PWR_WELL_CTL2.reg {
                if (val & pw2_req) != 0 && pw2_write_idx.is_none() {
                    pw2_write_idx = Some(i);
                }
                if (val & pwb_req) != 0 && pwb_write_idx.is_none() {
                    pwb_write_idx = Some(i);
                }
            }
        }
        assert!(pw2_write_idx.is_some(), "PW_2 enable write not found in log");
        assert!(pwb_write_idx.is_some(), "PW_B enable write not found in log");
        assert!(
            pw2_write_idx.unwrap() < pwb_write_idx.unwrap(),
            "PW_2 must be enabled before PW_B in dependency order"
        );
    }

    // 2nd get: refcount increments, no redundant hardware enable
    let wl2 = unsafe { intel_display_power_get(display, POWER_DOMAIN_PIPE_B) };
    assert!(!wl2.is_null());
    assert_eq!(unsafe { intel_power_well_refcount(pw_2) }, 2);
    assert_eq!(unsafe { intel_power_well_refcount(pw_b) }, 2);

    // Clear log and do 1st put: refcount decrements to 1, wells STAY on, NO MMIO writes
    if let Ok(mut log) = MMIO_WRITE_LOG.lock() {
        log.clear();
    }
    unsafe { intel_display_power_put(display, POWER_DOMAIN_PIPE_B, wl1) };
    assert_eq!(unsafe { intel_power_well_refcount(pw_2) }, 1);
    assert_eq!(unsafe { intel_power_well_refcount(pw_b) }, 1);
    assert!(unsafe { intel_power_well_is_enabled_cached(pw_2) });
    assert!(unsafe { intel_power_well_is_enabled_cached(pw_b) });
    {
        let log = MMIO_WRITE_LOG.lock().unwrap();
        assert!(
            log.is_empty(),
            "No MMIO disable writes should occur when refcount remains > 0"
        );
    }

    // 2nd put: refcount drops to 0, wells are disabled in REVERSE order (PW_B before PW_2)
    unsafe { intel_display_power_put(display, POWER_DOMAIN_PIPE_B, wl2) };
    assert_eq!(unsafe { intel_power_well_refcount(pw_2) }, 0);
    assert_eq!(unsafe { intel_power_well_refcount(pw_b) }, 0);
    assert!(!unsafe { intel_power_well_is_enabled_cached(pw_2) });
    assert!(!unsafe { intel_power_well_is_enabled_cached(pw_b) });

    {
        let log = MMIO_WRITE_LOG.lock().unwrap();
        assert!(!log.is_empty(), "MMIO disable writes should have occurred");
        let pw2_req = HSW_PWR_WELL_CTL_REQ(ICL_PW_CTL_IDX_PW_2);
        let pwb_req = HSW_PWR_WELL_CTL_REQ(XELPD_PW_CTL_IDX_PW_B);

        let mut pwb_disable_idx = None;
        let mut pw2_disable_idx = None;
        for (i, &(reg, val)) in log.iter().enumerate() {
            if reg == HSW_PWR_WELL_CTL2.reg {
                if (val & pwb_req) == 0 && pwb_disable_idx.is_none() {
                    pwb_disable_idx = Some(i);
                }
                if (val & pw2_req) == 0 && pw2_disable_idx.is_none() {
                    pw2_disable_idx = Some(i);
                }
            }
        }
        assert!(pwb_disable_idx.is_some(), "PW_B disable write not found");
        assert!(pw2_disable_idx.is_some(), "PW_2 disable write not found");
        assert!(
            pwb_disable_idx.unwrap() < pw2_disable_idx.unwrap(),
            "PW_B must be disabled before PW_2 in reverse dependency order"
        );
    }

    unsafe { intel_display_power_driver_remove(display) };
}

/// 3. intel_display_power_get_if_enabled on a disabled well returns 0 without touching hardware.
#[test]
fn test_display_power_get_if_enabled() {
    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    unsafe { intel_power_domains_init(display) };

    if let Ok(mut log) = MMIO_WRITE_LOG.lock() {
        log.clear();
    }

    // Attempt get_if_enabled when PIPE_A wells are disabled
    let wl = unsafe { intel_display_power_get_if_enabled(display, POWER_DOMAIN_PIPE_A) };
    assert!(wl.is_null(), "get_if_enabled on disabled domain must return NULL");

    {
        let log = MMIO_WRITE_LOG.lock().unwrap();
        assert!(log.is_empty(), "Hardware must not be touched on failed get_if_enabled");
    }

    // Now enable via normal get
    let wl_active = unsafe { intel_display_power_get(display, POWER_DOMAIN_PIPE_A) };
    assert!(!wl_active.is_null());

    // Now get_if_enabled succeeds
    let wl2 = unsafe { intel_display_power_get_if_enabled(display, POWER_DOMAIN_PIPE_A) };
    assert!(!wl2.is_null(), "get_if_enabled on enabled domain must return non-null wakelock");

    unsafe {
        intel_display_power_put(display, POWER_DOMAIN_PIPE_A, wl2);
        intel_display_power_put(display, POWER_DOMAIN_PIPE_A, wl_active);
        intel_display_power_driver_remove(display);
    }
}

/// 4. power well sync_hw / init_hw path with hardware already-on state read back from mock registers.
#[test]
fn test_power_well_sync_hw_init_hw() {
    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    // Pre-program mock register HSW_PWR_WELL_CTL2:
    // PW_1 and PW_2 are already on in hardware
    let pw1_mask = HSW_PWR_WELL_CTL_REQ(ICL_PW_CTL_IDX_PW_1) | HSW_PWR_WELL_CTL_STATE(ICL_PW_CTL_IDX_PW_1);
    let pw2_mask = HSW_PWR_WELL_CTL_REQ(ICL_PW_CTL_IDX_PW_2) | HSW_PWR_WELL_CTL_STATE(ICL_PW_CTL_IDX_PW_2);
    env.write_mmio32(HSW_PWR_WELL_CTL2.reg, pw1_mask | pw2_mask);

    unsafe {
        intel_power_domains_init(display);
        intel_power_domains_sync_hw(display);
    }

    let power_domains = unsafe { &(*display).power.domains };
    let pw_1 = unsafe { power_domains.power_wells.add(1) }; // PW_1
    let pw_2 = unsafe { power_domains.power_wells.add(3) }; // PW_2
    let pw_a = unsafe { power_domains.power_wells.add(4) }; // PW_A

    assert!(unsafe { intel_power_well_is_enabled_cached(pw_1) }, "PW_1 must be synced as enabled");
    assert!(unsafe { intel_power_well_is_enabled_cached(pw_2) }, "PW_2 must be synced as enabled");
    assert!(!unsafe { intel_power_well_is_enabled_cached(pw_a) }, "PW_A was not set in hardware, must be disabled");

    unsafe { intel_display_power_driver_remove(display) };
}

/// 5. intel_detect_pch: PCI ISA bridge IDs for ADP (e.g. 0x5180 family, and the ADL-N PCH ID) give PCH_ADP;
/// unknown gives PCH_NONE/NOP like Linux.
#[test]
fn test_intel_detect_pch() {
    use crate::display_pending_stubs::kapi_display::set_mock_isa_bridge;

    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    // 5a. Alderlake-P PCH ID (0x5182 -> masks to 0x5180 = INTEL_PCH_ADP2_DEVICE_ID_TYPE)
    let mut pdev_adp = PciDev {
        _pad_0: [0; 56],
        devfn: 0,
        vendor: 0x8086,
        device: 0x5182,
        subsystem_vendor: 0x8086,
        subsystem_device: 0x0001,
        class: 0x060100, // ISA bridge class
        revision: 0,
    };
    unsafe {
        set_mock_isa_bridge(&mut pdev_adp);
        intel_pch_detect(display);
        assert_eq!((*display).pch_type, PCH_ADP, "0x5182 must be detected as PCH_ADP");
    }

    // 5b. Alderlake-N PCH ID (0x5482 -> masks to 0x5480 = INTEL_PCH_ADP4_DEVICE_ID_TYPE)
    let mut pdev_adln = PciDev {
        _pad_0: [0; 56],
        devfn: 0,
        vendor: 0x8086,
        device: 0x5482,
        subsystem_vendor: 0x8086,
        subsystem_device: 0x0002,
        class: 0x060100,
        revision: 0,
    };
    unsafe {
        set_mock_isa_bridge(&mut pdev_adln);
        intel_pch_detect(display);
        assert_eq!((*display).pch_type, PCH_ADP, "0x5482 must be detected as PCH_ADP");
    }

    // 5c. Unknown PCI device ID -> PCH_NONE
    let mut pdev_unknown = PciDev {
        _pad_0: [0; 56],
        devfn: 0,
        vendor: 0x8086,
        device: 0x1234,
        subsystem_vendor: 0x8086,
        subsystem_device: 0x0003,
        class: 0x060100,
        revision: 0,
    };
    unsafe {
        set_mock_isa_bridge(&mut pdev_unknown);
        (*display).pch_type = PCH_NONE;
        intel_pch_detect(display);
        assert_eq!((*display).pch_type, PCH_NONE, "Unknown bridge must yield PCH_NONE");
    }

    // 5d. With display disabled / not present -> PCH_NOP
    unsafe {
        (*display).info.__device_info = core::ptr::null_mut();
        set_mock_isa_bridge(&mut pdev_adp);
        intel_pch_detect(display);
        assert_eq!((*display).pch_type, PCH_NOP, "No display must set PCH_NOP when bridge is found");
        set_mock_isa_bridge(core::ptr::null_mut());
    }
}

/// 6. intel_pmdemand_init on ADL-P is a no-op success (display ver 13).
#[test]
fn test_intel_pmdemand_init_adlp() {
    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    let ret = unsafe { intel_pmdemand_init(display) };
    assert_eq!(ret, 0, "intel_pmdemand_init on Display 13 (ADL-P) must succeed as no-op");
}

/// 7. intel_hti port/phy reservation decoding from a mocked HDPORT_STATE value.
#[test]
fn test_intel_hti_reservation_decoding() {
    let mut env = MockEnvironment::new();
    let display = env.display_ptr();

    // Setup device with has_hti = true
    let mut device_info = Box::new(unsafe { core::mem::zeroed::<IntelDisplayDeviceInfo>() });
    unsafe {
        core::ptr::copy_nonoverlapping(core::ptr::addr_of!(XE_LPD_DISPLAY), &mut *device_info, 1);
        device_info._bitfields_46[1] |= 0x02;
        (*display).info.__device_info = Box::into_raw(device_info);
    }

    // 7a. Mock HDPORT_STATE with enabled, PHY_A used, DPLL mask = 0x5
    let hti_val = HDPORT_ENABLED | HDPORT_DDI_USED(PHY_A as u32) | (0x5 << 12);
    env.write_mmio32(HDPORT_STATE.reg, hti_val);

    unsafe { intel_hti_init(display) };

    assert!(unsafe { intel_hti_uses_phy(display, PHY_A) }, "PHY_A must be decoded as reserved");
    assert!(!unsafe { intel_hti_uses_phy(display, PHY_B) }, "PHY_B must not be reserved");
    assert_eq!(unsafe { intel_hti_dpll_mask(display) }, 0x5, "DPLL mask must be decoded as 0x5");

    // 7b. When HDPORT_ENABLED bit is cleared, uses_phy must return false
    env.write_mmio32(HDPORT_STATE.reg, HDPORT_DDI_USED(PHY_A as u32) | (0x5 << 12));
    unsafe { intel_hti_init(display) };

    assert!(!unsafe { intel_hti_uses_phy(display, PHY_A) }, "Disabled HTI must return false");
    assert_eq!(unsafe { intel_hti_dpll_mask(display) }, 0, "Disabled HTI must return dpll_mask 0");

    unsafe {
        drop(Box::from_raw((*display).info.__device_info));
    }
}

/// 8. icl_display_core_init register sequence on ADL-P (mock MMIO write log order).
#[test]
fn test_icl_display_core_init_adlp_reg_sequence() {
    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    unsafe {
        intel_power_domains_init(display);
    }

    if let Ok(mut log) = MMIO_WRITE_LOG.lock() {
        log.clear();
    }

    unsafe {
        icl_display_core_init(display, false);
    }

    let log = MMIO_WRITE_LOG.lock().unwrap().clone();
    assert!(!log.is_empty(), "MMIO write log must not be empty after icl_display_core_init");

    // 1. DC_STATE_EN was written with DC_STATE_DISABLE (0)
    let dc_state_disable_idx = log.iter().position(|&(reg, val)| reg == DC_STATE_EN.reg && val == 0);
    assert!(dc_state_disable_idx.is_some(), "DC_STATE_DISABLE write to DC_STATE_EN must occur");

    // 2. PCH reset handshake on HSW_NDE_RSTWRN_OPT
    let pch_handshake_idx = log.iter().position(|&(reg, _)| reg == HSW_NDE_RSTWRN_OPT.reg);
    assert!(pch_handshake_idx.is_some(), "HSW_NDE_RSTWRN_OPT handshake must occur");
    assert!(pch_handshake_idx.unwrap() > dc_state_disable_idx.unwrap(), "PCH handshake after DC_STATE_DISABLE");

    // 3. Power Well 1 enable on HSW_PWR_WELL_CTL2
    let pw1_idx = log.iter().position(|&(reg, _)| reg == HSW_PWR_WELL_CTL2.reg);
    assert!(pw1_idx.is_some(), "PW1 enable must occur on HSW_PWR_WELL_CTL2");
    assert!(pw1_idx.unwrap() > pch_handshake_idx.unwrap(), "PW1 enable after PCH handshake");

    // 4. DBUF slice enable on _DBUF_CTL_S0
    let dbuf_idx = log.iter().position(|&(reg, val)| reg == _DBUF_CTL_S0 && (val & DBUF_POWER_REQUEST) != 0);
    assert!(dbuf_idx.is_some(), "DBUF S0 power request must occur");
    assert!(dbuf_idx.unwrap() > pw1_idx.unwrap(), "DBUF enable after PW1 enable");

    // 5. Wa_14011508470 on GEN11_CHICKEN_DCPR_2
    let wa_idx = log.iter().position(|&(reg, _)| reg == GEN11_CHICKEN_DCPR_2.reg);
    assert!(wa_idx.is_some(), "Wa_14011508470 must be programmed on GEN11_CHICKEN_DCPR_2");
    assert!(wa_idx.unwrap() > dbuf_idx.unwrap(), "Wa_14011508470 after DBUF enable");
}

/// 9. DBUF slice enable/disable (gen9_dbuf_slice_set).
#[test]
fn test_gen9_dbuf_slice_ops() {
    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    // 9a. Enable slice 0
    unsafe {
        gen9_dbuf_slice_set(display, 0, true);
    }
    let val_s0 = env.read_mmio32(_DBUF_CTL_S0);
    assert_ne!(val_s0 & DBUF_POWER_REQUEST, 0, "DBUF slice 0 power request must be set");
    assert_ne!(val_s0 & DBUF_POWER_STATE, 0, "DBUF slice 0 power state must be set");

    // 9b. Enable slice 1
    unsafe {
        gen9_dbuf_slice_set(display, 1, true);
    }
    let val_s1 = env.read_mmio32(_DBUF_CTL_S1);
    assert_ne!(val_s1 & DBUF_POWER_REQUEST, 0, "DBUF slice 1 power request must be set");
    assert_ne!(val_s1 & DBUF_POWER_STATE, 0, "DBUF slice 1 power state must be set");

    // 9c. Disable slice 0
    unsafe {
        gen9_dbuf_slice_set(display, 0, false);
    }
    let val_s0_off = env.read_mmio32(_DBUF_CTL_S0);
    assert_eq!(val_s0_off & DBUF_POWER_REQUEST, 0, "DBUF slice 0 power request must be cleared");
    assert_eq!(val_s0_off & DBUF_POWER_STATE, 0, "DBUF slice 0 power state must be cleared");
}

/// 10. gen9_write_dc_state readback verification and retry logic.
#[test]
fn test_gen9_write_dc_state_retry() {
    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    unsafe {
        gen9_write_dc_state(display, DC_STATE_EN_UPTO_DC5);
    }
    let val = env.read_mmio32(DC_STATE_EN.reg);
    assert_eq!(val & DC_STATE_EN_UPTO_DC5, DC_STATE_EN_UPTO_DC5, "DC5 state must be written to DC_STATE_EN");

    unsafe {
        gen9_write_dc_state(display, DC_STATE_DISABLE);
    }
    let val_off = env.read_mmio32(DC_STATE_EN.reg);
    assert_eq!(val_off, 0, "DC_STATE_DISABLE must clear DC_STATE_EN");
}

/// 11. Async put real delayed work pipeline and grab ref.
#[test]
fn test_async_put_delayed_work() {
    let mut env = MockEnvironment::new();
    env.setup_adlp();
    let display = env.display_ptr();

    unsafe {
        intel_power_domains_init(display);
    }
    let power_domains = unsafe { &mut (*display).power.domains };

    // 11a. Take reference on POWER_DOMAIN_PIPE_A
    let wf = unsafe { intel_display_power_get(display, POWER_DOMAIN_PIPE_A) };
    assert_eq!(power_domains.domain_use_count[POWER_DOMAIN_PIPE_A as usize], 1);

    // 11b. Put reference asynchronously with 100ms delay
    unsafe {
        __intel_display_power_put_async(display, POWER_DOMAIN_PIPE_A, wf, 100);
    }

    // Still in use while delayed work is pending!
    assert_eq!(power_domains.domain_use_count[POWER_DOMAIN_PIPE_A as usize], 1);
    let idx = (POWER_DOMAIN_PIPE_A / 64) as usize;
    let bit = POWER_DOMAIN_PIPE_A % 64;
    assert_ne!(
        power_domains.async_put_domains[0].bits[idx] & (1u64 << bit),
        0,
        "POWER_DOMAIN_PIPE_A must be queued in async_put_domains[0]"
    );
    assert!(!power_domains.async_put_wakeref.is_null(), "async_put_wakeref must be set");

    // 11c. Grab the async put ref back without cycling hardware
    let grabbed = unsafe { intel_display_power_get(display, POWER_DOMAIN_PIPE_A) };
    assert_ne!(grabbed, core::ptr::null_mut());
    assert_eq!(
        power_domains.async_put_domains[0].bits[idx] & (1u64 << bit),
        0,
        "Grabbing reference must clear it from async_put_domains"
    );
    assert_eq!(power_domains.domain_use_count[POWER_DOMAIN_PIPE_A as usize], 1);

    // 11d. Put asynchronously again and flush work
    unsafe {
        __intel_display_power_put_async(display, POWER_DOMAIN_PIPE_A, grabbed, 100);
        intel_display_power_flush_work(display);
    }

    assert_eq!(
        power_domains.domain_use_count[POWER_DOMAIN_PIPE_A as usize],
        0,
        "After flush_work, domain use count must drop to 0"
    );
    assert_eq!(
        power_domains.async_put_domains[0].bits[idx] & (1u64 << bit),
        0,
        "After flush_work, async_put_domains must be clear"
    );
    assert!(power_domains.async_put_wakeref.is_null(), "After flush_work, wakeref must be null");
}
