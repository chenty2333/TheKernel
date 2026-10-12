// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! Original LinuxKPI DMA backend: native page allocation and requester-scoped
//! VT-d mappings. A device must be registered by its owner; no anonymous DMA
//! identity, guessed topology or fake successful map is supplied.
#[cfg(any(target_os = "none", test))]
use alloc::vec::Vec;
use alloc::{collections::BTreeMap, sync::Arc};
use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use crate::{
    scatterlist::{sg_next, sg_page},
    types::Scatterlist,
    shmem,
};
/// A registered DMA identity is immutable until boot ends. IRQ assignment is
/// separate: PCI MSI setup precedes request_irq, but follows DMA admission.
struct Device {
    requester: tk_vtd::PciRequester,
    iommu: bool,
    irq: AtomicUsize,
    streaming_mask: AtomicU64,
    coherent_mask: AtomicU64,
    max_segment: AtomicUsize,
    #[cfg(target_os = "none")]
    identity: Option<IdentityDomain>,
}
#[cfg(target_os = "none")]
struct IdentityDomain {
    lease: Arc<tk_vtd::IdentityDmaLease>,
    // Repeated mappings of the same pages own separate VT-d tokens.
    mappings: spin::Mutex<BTreeMap<(u64, usize), Vec<tk_vtd::IdentityDmaMapping>>>,
}
static DEVICES: spin::Mutex<BTreeMap<usize, Arc<Device>>> = spin::Mutex::new(BTreeMap::new());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DmaMapError {
    DeviceNotOwned,
    InvalidPhysicalExtent,
    SegmentTooLarge,
    AddressMask,
    OwnershipAllocation,
    IdentityLease(tk_vtd::Error),
    Translation(tk_vtd::Error),
}

fn publish_device(device: *mut c_void, owner: Device) -> Result<(), &'static str> {
    if device.is_null() {
        return Err("invalid DMA device");
    }
    let mut devices = DEVICES.lock();
    if devices.contains_key(&(device as usize)) {
        return Err("DMA device already owned");
    }
    devices.insert(device as usize, Arc::new(owner));
    Ok(())
}

/// Register a requester whose context may use the platform's translated DMA
/// API. Display-shared iGPUs must instead use register_identity_device: changing
/// their requester context would invalidate the live physical GGTT scanout.
pub fn register_device(
    device: *mut c_void,
    requester: tk_vtd::PciRequester,
    iommu: bool,
    irq: usize,
) -> Result<(), &'static str> {
    if irq >= 256 {
        return Err("invalid IRQ vector");
    }
    publish_device(
        device,
        Device {
            requester,
            iommu,
            irq: AtomicUsize::new(irq),
            streaming_mask: AtomicU64::new(u64::MAX),
            coherent_mask: AtomicU64::new(u64::MAX),
            max_segment: AtomicUsize::new(u32::MAX as usize),
            #[cfg(target_os = "none")]
            identity: None,
        },
    )
}

/// Reuse the native display owner's verified shared-identity lease, without
/// installing a requester-specific VT-d context or claiming device isolation.
#[cfg(target_os = "none")]
pub fn register_identity_device(
    device: *mut c_void,
    lease: Arc<tk_vtd::IdentityDmaLease>,
) -> Result<(), &'static str> {
    publish_device(
        device,
        Device {
            requester: lease.requester(),
            iommu: true,
            irq: AtomicUsize::new(usize::MAX),
            streaming_mask: AtomicU64::new(u64::MAX),
            coherent_mask: AtomicU64::new(u64::MAX),
            max_segment: AtomicUsize::new(u32::MAX as usize),
            identity: Some(IdentityDomain {
                lease,
                mappings: spin::Mutex::new(BTreeMap::new()),
            }),
        },
    )
}

fn device(dev: *mut c_void) -> Result<Arc<Device>, ()> {
    DEVICES.lock().get(&(dev as usize)).cloned().ok_or(())
}

pub fn set_mask(dev: *mut c_void, mask: u64, coherent: bool) -> Result<(), ()> {
    // A DMA mask is a contiguous low-bit range, not merely a large integer.
    if mask < u32::MAX as u64 || mask & mask.wrapping_add(1) != 0 {
        return Err(());
    }
    let owner = device(dev)?;
    let target = if coherent {
        &owner.coherent_mask
    } else {
        &owner.streaming_mask
    };
    target.store(mask, Ordering::Release);
    Ok(())
}
pub fn set_max_segment(dev: *mut c_void, size: u32) -> Result<(), ()> {
    if size == 0 {
        return Err(());
    }
    device(dev)?
        .max_segment
        .store(size as usize, Ordering::Release);
    Ok(())
}
fn fits_mask(address: u64, size: usize, mask: u64) -> bool {
    size != 0
        && address
            .checked_add(size as u64 - 1)
            .is_some_and(|end| end <= mask)
}

pub fn set_device_irq(dev: *mut c_void, irq: usize) -> Result<(), &'static str> {
    if irq >= 256 {
        return Err("invalid IRQ vector");
    }
    let owner = device(dev).map_err(|_| "IRQ device has no DMA owner")?;
    match owner
        .irq
        .compare_exchange(usize::MAX, irq, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => Ok(()),
        Err(existing) if existing == irq => Ok(()),
        Err(_) => Err("DMA device IRQ already owned"),
    }
}
pub fn device_iommu_mapped(dev: *mut c_void) -> bool {
    device(dev).is_ok_and(|d| d.iommu)
}
pub fn device_irq(dev: *mut c_void) -> usize {
    let irq = device(dev)
        .expect("IRQ device has no DMA owner")
        .irq
        .load(Ordering::Acquire);
    assert!(irq < 256, "DMA device has no assigned IRQ");
    irq
}

/// Exact physical page extent, including partial first/last scatterlist pages.
fn page_extent(physical: u64, size: usize) -> Result<core::ops::Range<u64>, ()> {
    if size == 0 {
        return Err(());
    }
    let end = physical.checked_add(size as u64).ok_or(())?;
    if end > (1u64 << 52) {
        return Err(());
    }
    Ok((physical & !4095)..(end.checked_add(4095).ok_or(())? & !4095))
}

fn map(dev: *mut c_void, physical: u64, size: usize, coherent: bool) -> Result<u64, DmaMapError> {
    let device = device(dev).map_err(|_| DmaMapError::DeviceNotOwned)?;
    let pages = page_extent(physical, size).map_err(|_| DmaMapError::InvalidPhysicalExtent)?;
    let mask = if coherent {
        &device.coherent_mask
    } else {
        &device.streaming_mask
    }
    .load(Ordering::Acquire);
    if !coherent && size > device.max_segment.load(Ordering::Acquire) {
        return Err(DmaMapError::SegmentTooLarge);
    }
    #[cfg(target_os = "none")]
    {
        if let Some(identity) = &device.identity {
            if !fits_mask(physical, size, mask) {
                return Err(DmaMapError::AddressMask);
            }
            let mut physical_pages = Vec::new();
            physical_pages
                .try_reserve_exact(((pages.end - pages.start) / 4096) as usize)
                .map_err(|_| DmaMapError::OwnershipAllocation)?;
            physical_pages.extend(pages.step_by(4096));
            let mut mappings = identity.mappings.lock();
            let tokens = mappings.entry((physical, size)).or_default();
            // Reserve storage before admission so allocation failure cannot
            // lose an active token and allow the caller to release its pages.
            tokens
                .try_reserve(1)
                .map_err(|_| DmaMapError::OwnershipAllocation)?;
            let token = match identity.lease.map_pages(&physical_pages) {
                Ok(token) => token,
                Err(tk_vtd::Error::Quarantined) => {
                    panic!("i915: identity DMA admission quarantined; retain backing")
                }
                Err(error) => return Err(DmaMapError::IdentityLease(error)),
            };
            tokens.push(token);
            return Ok(physical);
        }
        match tk_vtd::platform_map_for(device.requester, physical, size) {
            Ok(address) if fits_mask(address, size, mask) => Ok(address),
            Ok(address) => {
                tk_vtd::platform_unmap_for(device.requester, address, size)
                    .expect("i915: out-of-mask DMA retirement failed; retain backing");
                Err(DmaMapError::AddressMask)
            }
            Err(tk_vtd::Error::Quarantined) => {
                panic!("i915: VT-d mapping quarantined; refusing to release DMA pages");
            }
            Err(error) => Err(DmaMapError::Translation(error)),
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (device, pages);
        if fits_mask(physical, size, mask) {
            Ok(physical)
        } else {
            Err(DmaMapError::AddressMask)
        }
    } // hosted RAM-only model, never hardware DMA
}
fn unmap(dev: *mut c_void, address: u64, size: usize) -> Result<(), ()> {
    let device = device(dev)?;
    #[cfg(target_os = "none")]
    {
        if let Some(identity) = &device.identity {
            let mut mappings = identity.mappings.lock();
            return retire_mapping(&mut mappings, (address, size), |token| {
                identity.lease.unmap_pages(token).map_err(|_| ())
            });
        }
        tk_vtd::platform_unmap_for(device.requester, address, size).map_err(|_| ())
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (device, address, size);
        Ok(())
    }
}
/// Retirement must succeed before removing the last ownership token. A
/// failed unmap keeps the complete batch, even with duplicate DMA mappings.
#[cfg(any(target_os = "none", test))]
fn retire_mapping<T>(
    mappings: &mut BTreeMap<(u64, usize), Vec<T>>,
    key: (u64, usize),
    retire: impl FnOnce(&T) -> Result<(), ()>,
) -> Result<(), ()> {
    let tokens = mappings.get_mut(&key).ok_or(())?;
    retire(tokens.last().ok_or(())?)?;
    tokens.pop();
    if tokens.is_empty() {
        mappings.remove(&key);
    }
    Ok(())
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
        let length = unsafe { (*sg).length as usize };
        match map(dev, address, length, false) {
            Ok(dma) => unsafe {
                (*sg).dma_address = dma;
                (*sg).dma_length = (*sg).length;
            },
            Err(error) => {
                #[cfg(target_os = "none")]
                axlog::warn!(
                    "i915 dma_map_sg_attrs rejected dev={dev:p} segment={done}/{nents} \
                     physical={address:#x} length={length} error={error:?}"
                );
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
/// Linux `dma_map_sgtable(dev, sgt, dir, attrs)`: maps the table's original
/// entries and records the mapped count in `sgt->nents`. Returns 0 or a
/// negative errno. An unregistered device or exhausted translation space
/// fails the mapping with -ENOMEM (retryable), as Linux reports it.
///
/// # Safety
/// `sgt` must describe a valid, unmapped scatterlist.
#[unsafe(export_name = "dma_map_sgtable")]
pub unsafe extern "C" fn dma_map_sgtable(
    dev: *mut c_void,
    sgt: *mut crate::types::SgTable,
    direction: i32,
    attrs: u64,
) -> i32 {
    if sgt.is_null() {
        return -crate::config::EINVAL;
    }
    let (sgl, orig) = unsafe { ((*sgt).sgl, (*sgt).orig_nents) };
    if sgl.is_null() || orig == 0 {
        return -crate::config::EINVAL;
    }
    let mapped = unsafe { dma_map_sg_attrs(dev, sgl, orig, direction as u32, attrs) };
    if mapped == 0 {
        return -crate::config::ENOMEM;
    }
    unsafe { (*sgt).nents = mapped };
    0
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
    match map(dev, physical, size, true) {
        Ok(address) => {
            unsafe {
                backing.write_bytes(0, size);
                *dma = address
            };
            backing.cast()
        }
        Err(_) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_retirement_retains_every_duplicate_mapping_token() {
        let key = (0x4000, 4096);
        let mut mappings = BTreeMap::new();
        mappings.insert(key, alloc::vec![11, 12]);
        assert!(retire_mapping(&mut mappings, key, |_| Err(())).is_err());
        assert_eq!(mappings[&key], [11, 12]);
        assert!(retire_mapping(&mut mappings, (0x4000, 8192), |_| panic!("wrong extent")).is_err());
        retire_mapping(&mut mappings, key, |token| {
            assert_eq!(*token, 12);
            Ok(())
        })
        .unwrap();
        assert_eq!(mappings[&key], [11]);
        retire_mapping(&mut mappings, key, |_| Ok(())).unwrap();
        assert!(mappings.is_empty());
        assert!(retire_mapping(&mut mappings, key, |_| panic!("double unmap")).is_err());
    }

    #[test]
    fn scatterlist_extent_includes_partial_pages_and_rejects_overflow() {
        assert_eq!(page_extent(0x1fff, 2), Ok(0x1000..0x3000));
        assert_eq!(page_extent(0x2000, 4096), Ok(0x2000..0x3000));
        assert!(page_extent(0x1000, 0).is_err());
        assert!(page_extent(u64::MAX, 2).is_err());
        assert!(page_extent((1 << 52) - 1, 2).is_err());
    }
    #[test]
    fn irq_assignment_cannot_replace_an_owned_vector() {
        let mut identity = 0u8;
        let dev = core::ptr::from_mut(&mut identity).cast();
        publish_device(
            dev,
            Device {
                requester: tk_vtd::PciRequester {
                    segment: 0,
                    bus: 0,
                    device: 2,
                    function: 0,
                },
                iommu: true,
                irq: AtomicUsize::new(usize::MAX),
                streaming_mask: AtomicU64::new(u64::MAX),
                coherent_mask: AtomicU64::new(u64::MAX),
                max_segment: AtomicUsize::new(u32::MAX as usize),
            },
        )
        .unwrap();
        assert!(set_device_irq(dev, 256).is_err());
        set_device_irq(dev, 80).unwrap();
        set_device_irq(dev, 80).unwrap();
        assert!(set_device_irq(dev, 81).is_err());
        assert_eq!(device_irq(dev), 80);
        assert!(set_mask(dev, 0xffff_0000, false).is_err());
        set_mask(dev, u32::MAX as u64, false).unwrap();
        assert_eq!(map(dev, 1 << 32, 4096, false), Err(DmaMapError::AddressMask));
        assert_eq!(map(dev, 1 << 32, 4096, true), Ok(1 << 32));
        set_max_segment(dev, 4096).unwrap();
        assert_eq!(
            map(dev, 0x2000, 8192, false),
            Err(DmaMapError::SegmentTooLarge)
        );
        assert_eq!(
            map(dev, u64::MAX, 4096, true),
            Err(DmaMapError::InvalidPhysicalExtent)
        );
        assert!(fits_mask(0xffff_f000, 4096, u32::MAX as u64));
        assert!(!fits_mask(0xffff_f000, 4097, u32::MAX as u64));
        DEVICES.lock().remove(&(dev as usize));
    }
}
