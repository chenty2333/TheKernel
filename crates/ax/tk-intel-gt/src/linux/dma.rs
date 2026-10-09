// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! Original LinuxKPI DMA backend: native page allocation and requester-scoped
//! VT-d mappings. A device must be registered by its owner; no anonymous DMA
//! identity, guessed topology or fake successful map is supplied.
use alloc::collections::BTreeMap;
use core::{ffi::c_void, ptr};

use crate::{
    i915_gem_pages_upstream::{Scatterlist, sg_next, sg_page},
    linux::shmem,
};
#[derive(Clone, Copy)]
struct Device {
    requester: tk_vtd::PciRequester,
    iommu: bool,
    irq: usize,
}
static DEVICES: spin::Mutex<BTreeMap<usize, Device>> = spin::Mutex::new(BTreeMap::new());
pub fn register_device(
    device: *mut c_void,
    requester: tk_vtd::PciRequester,
    iommu: bool,
    irq: usize,
) -> Result<(), &'static str> {
    if device.is_null() || irq >= 256 {
        return Err("invalid DMA device");
    }
    let mut devices = DEVICES.lock();
    if devices.contains_key(&(device as usize)) {
        return Err("DMA device already owned");
    }
    devices.insert(
        device as usize,
        Device {
            requester,
            iommu,
            irq,
        },
    );
    Ok(())
}
pub fn device_iommu_mapped(dev: *mut c_void) -> bool {
    DEVICES.lock().get(&(dev as usize)).is_some_and(|d| d.iommu)
}
pub fn device_irq(dev: *mut c_void) -> usize {
    DEVICES
        .lock()
        .get(&(dev as usize))
        .expect("IRQ device has no owner")
        .irq
}
fn map(dev: *mut c_void, physical: u64, size: usize) -> Result<u64, ()> {
    let device = *DEVICES.lock().get(&(dev as usize)).ok_or(())?;
    #[cfg(target_os = "none")]
    {
        tk_vtd::platform_map_for(device.requester, physical, size).map_err(|_| ())
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (device, size);
        Ok(physical)
    } // hosted RAM-only model, never hardware DMA
}
fn unmap(dev: *mut c_void, address: u64, size: usize) -> Result<(), ()> {
    let device = *DEVICES.lock().get(&(dev as usize)).ok_or(())?;
    #[cfg(target_os = "none")]
    {
        tk_vtd::platform_unmap_for(device.requester, address, size).map_err(|_| ())
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (device, address, size);
        Ok(())
    }
}
pub unsafe fn dma_map_sg_attrs(
    dev: *mut c_void,
    mut sg: *mut Scatterlist,
    nents: u32,
    _direction: u32,
    _attrs: u64,
) -> u32 {
    let first = sg;
    let mut done = 0;
    while done < nents && !sg.is_null() {
        let address = unsafe { shmem::page_to_phys(sg_page(sg)) as u64 + (*sg).offset as u64 };
        match map(dev, address, unsafe { (*sg).length as usize }) {
            Ok(dma) => unsafe {
                (*sg).dma_address = dma;
                (*sg).dma_length = (*sg).length;
            },
            Err(()) => {
                unsafe { dma_unmap_sg_attrs(dev, first, done, _direction, _attrs) };
                return 0;
            }
        }
        done += 1;
        sg = unsafe { sg_next(sg) };
    }
    if done != nents {
        unsafe { dma_unmap_sg_attrs(dev, first, done, _direction, _attrs) };
        return 0;
    }
    done
}
pub unsafe fn dma_unmap_sg_attrs(
    dev: *mut c_void,
    mut sg: *mut Scatterlist,
    nents: u32,
    _direction: u32,
    _attrs: u64,
) {
    for _ in 0..nents {
        assert!(!sg.is_null());
        unsafe {
            unmap(dev, (*sg).dma_address, (*sg).dma_length as usize)
                .expect("DMA mapping retirement failed; retain pages");
            (*sg).dma_address = 0;
            (*sg).dma_length = 0;
            sg = sg_next(sg);
        }
    }
}
pub unsafe fn dma_alloc_coherent(
    dev: *mut c_void,
    size: usize,
    dma: *mut u64,
    _gfp: u32,
) -> *mut c_void {
    if size == 0
        || size & 4095 != 0
        || dma.is_null()
        || !DEVICES.lock().contains_key(&(dev as usize))
    {
        return ptr::null_mut();
    }
    #[cfg(target_os = "none")]
    let backing = match axalloc::global_allocator().alloc_pages(
        size / 4096,
        size.next_power_of_two(),
        axalloc::UsageKind::Dma,
    ) {
        Ok(p) => p as *mut u8,
        Err(_) => return ptr::null_mut(),
    };
    #[cfg(not(target_os = "none"))]
    let backing = unsafe {
        alloc::alloc::alloc_zeroed(
            core::alloc::Layout::from_size_align(size, size.next_power_of_two()).unwrap(),
        )
    };
    if backing.is_null() {
        return ptr::null_mut();
    }
    #[cfg(target_os = "none")]
    let physical = axhal::mem::virt_to_phys((backing as usize).into()).as_usize() as u64;
    #[cfg(not(target_os = "none"))]
    let physical = backing as u64;
    match map(dev, physical, size) {
        Ok(address) => {
            unsafe {
                backing.write_bytes(0, size);
                *dma = address
            };
            backing.cast()
        }
        Err(()) => {
            unsafe { free_backing(backing, size) };
            ptr::null_mut()
        }
    }
}
unsafe fn free_backing(backing: *mut u8, size: usize) {
    #[cfg(target_os = "none")]
    axalloc::global_allocator().dealloc_pages(
        backing as usize,
        size / 4096,
        axalloc::UsageKind::Dma,
    );
    #[cfg(not(target_os = "none"))]
    unsafe {
        alloc::alloc::dealloc(
            backing,
            core::alloc::Layout::from_size_align(size, size.next_power_of_two()).unwrap(),
        )
    };
}
pub unsafe fn dma_free_coherent(dev: *mut c_void, size: usize, address: *mut c_void, dma: u64) {
    unmap(dev, dma, size).expect("coherent DMA retirement failed; retain allocation");
    unsafe { free_backing(address.cast(), size) };
}
