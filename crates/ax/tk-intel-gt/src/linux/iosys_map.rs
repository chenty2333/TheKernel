// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Exact Linux `iosys_map` tagged pointer ABI and basic accessors.

use core::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub union IosysMapAddr {
    pub vaddr_iomem: *mut c_void,
    pub vaddr: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IosysMap {
    pub addr: IosysMapAddr,
    pub is_iomem: bool,
}

const _: [(); 16] = [(); core::mem::size_of::<IosysMap>()];
const _: [(); 8] = [(); core::mem::align_of::<IosysMap>()];
const _: [(); 8] = [(); core::mem::offset_of!(IosysMap, is_iomem)];

impl IosysMap {
    pub const fn is_iomem(&self) -> bool {
        self.is_iomem
    }

    pub unsafe fn vaddr(&self) -> *mut c_void {
        unsafe { self.addr.vaddr }
    }

    pub unsafe fn set_vaddr(&mut self, address: *mut c_void) {
        self.addr.vaddr = address;
        self.is_iomem = false;
    }

    pub unsafe fn set_vaddr_iomem(&mut self, address: *mut c_void) {
        self.addr.vaddr_iomem = address;
        self.is_iomem = true;
    }
}
