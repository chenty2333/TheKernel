// SPDX-License-Identifier: MIT
// Copyright 2014 Intel Corporation
// Source: Linux v7.2.3 drivers/gpu/drm/i915/i915_gmch.c (MIT).
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::ffi::{c_int, c_void};

use crate::{
    linux::{
        gem_memory::Resource,
        i915::{GRAPHICS_VER, IS_I915G, IS_I915GM, IS_VALLEYVIEW, IS_CHERRYVIEW},
        kernel_core::{PCI_BUS, drmm_add_action_or_reset},
    },
    linux_config::EIO,
    linux_i915_private::DrmI915Private,
};

// Byte offset of `struct { ... } gmch` in `struct drm_i915_private` (x86_64
// oracle, pahole on i915_layout_probe.o): pdev@0, mch_res@8 (64 bytes),
// mchbar_need_disable@72.
const I915_GMCH_OFFSET: usize = 2280;
const GMCH_MCH_RES_OFFSET: usize = 8;
const GMCH_MCHBAR_NEED_DISABLE_OFFSET: usize = 72;

// include/drm/intel/pci_config.h
const MCHBAR_I915: u32 = 0x44;
const MCHBAR_I965: u32 = 0x48;
const MCHBAR_SIZE: u64 = 4 * 4096;
const DEVEN: u32 = 0x54;
const DEVEN_MCHBAR_EN: u32 = 1 << 28;

const IORESOURCE_MEM: u64 = 0x0000_0200;

#[inline]
unsafe fn gmch_pdev(i915: *mut DrmI915Private) -> *mut *mut c_void {
    unsafe { (i915 as *mut u8).add(I915_GMCH_OFFSET).cast() }
}

#[inline]
unsafe fn gmch_mch_res(i915: *mut DrmI915Private) -> *mut Resource {
    unsafe { (i915 as *mut u8).add(I915_GMCH_OFFSET + GMCH_MCH_RES_OFFSET).cast() }
}

#[inline]
unsafe fn gmch_mchbar_need_disable(i915: *mut DrmI915Private) -> *mut bool {
    unsafe { (i915 as *mut u8).add(I915_GMCH_OFFSET + GMCH_MCHBAR_NEED_DISABLE_OFFSET).cast() }
}

unsafe extern "C" {
    fn to_pci_dev(dev: *mut c_void) -> *mut c_void;
}

// upstream: i915_gmch.c i915_gmch_bridge_release()
unsafe extern "C" fn i915_gmch_bridge_release(_drm: *mut c_void, bridge: *mut c_void) {
    unsafe { (PCI_BUS.require("PCI bus").pci_dev_put)(bridge) }
}

// upstream: i915_gmch.c i915_gmch_bridge_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gmch_bridge_setup(i915: *mut DrmI915Private) -> c_int {
    unsafe {
        let bus = PCI_BUS.require("PCI bus");
        let pdev = (bus.domain_host_bridge)(to_pci_dev((*i915).drm.dev));
        *gmch_pdev(i915) = pdev;
        if pdev.is_null() {
            drm_err!(&(*i915).drm, "bridge device not found\n");
            return -EIO;
        }

        drmm_add_action_or_reset(
            core::ptr::addr_of_mut!((*i915).drm).cast(),
            i915_gmch_bridge_release,
            pdev,
        )
    }
}

// upstream: i915_gmch.c mchbar_reg()
unsafe fn mchbar_reg(i915: *mut DrmI915Private) -> u32 {
    if unsafe { GRAPHICS_VER(i915) } >= 4 {
        MCHBAR_I965
    } else {
        MCHBAR_I915
    }
}

// upstream: i915_gmch.c intel_alloc_mchbar_resource()
// `pnp_range_reserved()` is only consulted under CONFIG_PNP, which is not set in
// this kernel's configuration, so the PnP reservation check is absent.
unsafe fn intel_alloc_mchbar_resource(i915: *mut DrmI915Private) -> c_int {
    unsafe {
        let bus = PCI_BUS.require("PCI bus");
        let pdev = *gmch_pdev(i915);
        let mchbar = mchbar_reg(i915);

        let mut temp_hi = 0u32;
        if GRAPHICS_VER(i915) >= 4 {
            (bus.read_config_dword)(pdev, mchbar + 4, &mut temp_hi);
        }
        let mut temp_lo = 0u32;
        (bus.read_config_dword)(pdev, mchbar, &mut temp_lo);

        let mut mch_res = gmch_mch_res(i915);
        (*mch_res).name = b"i915 MCHBAR\0".as_ptr().cast();
        (*mch_res).flags = IORESOURCE_MEM;

        let ret = (bus.bus_alloc_mem_resource)(pdev, mch_res.cast(), MCHBAR_SIZE, MCHBAR_SIZE);
        if ret != 0 {
            drm_dbg!(&(*i915).drm, "failed bus alloc: %d\n", ret);
            (*mch_res).start = 0;
            return ret;
        }
        mch_res = gmch_mch_res(i915);

        let start = (*mch_res).start;
        if GRAPHICS_VER(i915) >= 4 {
            (bus.write_config_dword)(pdev, mchbar + 4, (start >> 32) as u32);
        }
        (bus.write_config_dword)(pdev, mchbar, start as u32);
        0
    }
}

// upstream: i915_gmch.c i915_gmch_bar_setup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gmch_bar_setup(i915: *mut DrmI915Private) {
    unsafe {
        if IS_VALLEYVIEW(i915) || IS_CHERRYVIEW(i915) {
            return;
        }
        let bus = PCI_BUS.require("PCI bus");
        let pdev = *gmch_pdev(i915);

        *gmch_mchbar_need_disable(i915) = false;

        let mut temp = 0u32;
        let enabled = if IS_I915G(i915) || IS_I915GM(i915) {
            (bus.read_config_dword)(pdev, DEVEN, &mut temp);
            temp & DEVEN_MCHBAR_EN != 0
        } else {
            (bus.read_config_dword)(pdev, mchbar_reg(i915), &mut temp);
            temp & 1 != 0
        };
        if enabled {
            return;
        }

        if intel_alloc_mchbar_resource(i915) != 0 {
            return;
        }

        *gmch_mchbar_need_disable(i915) = true;
        if IS_I915G(i915) || IS_I915GM(i915) {
            (bus.write_config_dword)(pdev, DEVEN, temp | DEVEN_MCHBAR_EN);
        } else {
            (bus.read_config_dword)(pdev, mchbar_reg(i915), &mut temp);
            (bus.write_config_dword)(pdev, mchbar_reg(i915), temp | 1);
        }
    }
}

// upstream: i915_gmch.c i915_gmch_bar_teardown()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gmch_bar_teardown(i915: *mut DrmI915Private) {
    unsafe {
        let bus = PCI_BUS.require("PCI bus");
        let pdev = *gmch_pdev(i915);

        if *gmch_mchbar_need_disable(i915) {
            if IS_I915G(i915) || IS_I915GM(i915) {
                let mut deven_val = 0u32;
                (bus.read_config_dword)(pdev, DEVEN, &mut deven_val);
                deven_val &= !DEVEN_MCHBAR_EN;
                (bus.write_config_dword)(pdev, DEVEN, deven_val);
            } else {
                let mut mchbar_val = 0u32;
                (bus.read_config_dword)(pdev, mchbar_reg(i915), &mut mchbar_val);
                mchbar_val &= !1;
                (bus.write_config_dword)(pdev, mchbar_reg(i915), mchbar_val);
            }
        }

        let mch_res = gmch_mch_res(i915);
        if (*mch_res).start != 0 {
            (bus.release_resource)(mch_res.cast());
        }
    }
}
