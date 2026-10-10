// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/i915_pci.c. The complete
// MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::ffi::c_void;

/// `IORESOURCE_UNSET` from include/linux/ioport.h.
const IORESOURCE_UNSET: u64 = 0x2000_0000;

unsafe extern "C" {
    /// PCI BAR flags (`pci_resource_flags()`); owned by the LinuxKPI PCI layer.
    fn pci_resource_flags(pdev: *mut c_void, bar: c_int) -> u64;
    fn pci_resource_len(dev: *mut c_void, bar: u32) -> u64;
}

use core::ffi::c_int;

/// Source `i915_pci_resource_valid()` from i915_pci.c.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_pci_resource_valid(pdev: *mut c_void, bar: c_int) -> bool {
    if unsafe { pci_resource_flags(pdev, bar) } == 0 {
        return false;
    }

    if unsafe { pci_resource_flags(pdev, bar) } & IORESOURCE_UNSET != 0 {
        return false;
    }

    if unsafe { pci_resource_len(pdev, bar as u32) } == 0 {
        return false;
    }

    true
}
