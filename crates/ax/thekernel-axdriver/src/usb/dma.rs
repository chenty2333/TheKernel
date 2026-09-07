//! x86 coherent DMA backed by the same page allocator as VirtIO queues.

use core::{alloc::Layout, num::NonZeroUsize, ptr::NonNull, time::Duration};

use axalloc::{UsageKind, global_allocator};
use axhal::mem::virt_to_phys;
use crab_usb::KernelOp;
use dma_api_usb::{DmaAllocHandle, DmaConstraints, DmaDirection, DmaError, DmaMapHandle, DmaOp};

pub(super) struct Kernel;
pub(super) static KERNEL: Kernel = Kernel;
const PAGE_SIZE: usize = 4096;

fn allocation(constraints: DmaConstraints, layout: Layout) -> Option<(NonNull<u8>, u64)> {
    let size = layout.size();
    if size == 0 || constraints.max_segment_size.is_some_and(|max| size > max) {
        return None;
    }
    let mut align = layout.align().max(constraints.align).max(PAGE_SIZE);
    if let Some(boundary) = constraints.boundary {
        if !boundary.is_power_of_two() || size > boundary {
            return None;
        }
        align = align.max(boundary);
    }
    if !align.is_power_of_two() {
        return None;
    }
    let pages = size.checked_add(PAGE_SIZE - 1)? / PAGE_SIZE;
    let address = global_allocator()
        .alloc_pages(pages, align, UsageKind::Dma)
        .ok()?;
    let physical = virt_to_phys(address.into()).as_usize() as u64;
    if physical
        .checked_add(size as u64 - 1)
        .is_none_or(|end| end > constraints.addr_mask)
        || !physical.is_multiple_of(align as u64)
    {
        global_allocator().dealloc_pages(address, pages, UsageKind::Dma);
        return None;
    }
    // Page allocation owns the complete physically contiguous, direct-mapped range.
    unsafe { core::ptr::write_bytes(address as *mut u8, 0, pages * PAGE_SIZE) };
    Some((NonNull::new(address as *mut u8)?, physical))
}

fn release(address: NonNull<u8>, size: usize) {
    global_allocator().dealloc_pages(
        address.as_ptr() as usize,
        size.div_ceil(PAGE_SIZE),
        UsageKind::Dma,
    );
}

impl KernelOp for Kernel {
    fn delay(&self, duration: Duration) {
        axhal::time::busy_wait(duration);
    }
}

impl DmaOp for Kernel {
    fn page_size(&self) -> usize {
        PAGE_SIZE
    }

    unsafe fn alloc_contiguous(
        &self,
        constraints: DmaConstraints,
        layout: Layout,
    ) -> Option<DmaAllocHandle> {
        let (address, physical) = allocation(constraints, layout)?;
        Some(unsafe { DmaAllocHandle::new(address, address, physical.into(), layout) })
    }

    unsafe fn dealloc_contiguous(&self, handle: DmaAllocHandle) {
        release(handle.allocation_ptr(), handle.size());
    }

    unsafe fn alloc_coherent(
        &self,
        constraints: DmaConstraints,
        layout: Layout,
    ) -> Option<DmaAllocHandle> {
        // PCI xHCI is cache coherent on the supported x86_64 platform.
        unsafe { self.alloc_contiguous(constraints, layout) }
    }

    unsafe fn dealloc_coherent(&self, handle: DmaAllocHandle) -> Result<(), DmaError> {
        unsafe { self.dealloc_contiguous(handle) };
        Ok(())
    }

    unsafe fn map_streaming(
        &self,
        constraints: DmaConstraints,
        addr: NonNull<u8>,
        size: NonZeroUsize,
        _direction: DmaDirection,
    ) -> Result<DmaMapHandle, DmaError> {
        // Retain a contiguous bounce allocation rather than assuming arbitrary
        // caller buffers have contiguous physical pages or USB alignment.
        let layout = Layout::from_size_align(size.get(), constraints.align.max(1))?;
        let (bounce, physical) = allocation(constraints, layout).ok_or(DmaError::NoMemory)?;
        Ok(unsafe { DmaMapHandle::new(addr, physical.into(), layout, Some(bounce)) })
    }

    unsafe fn unmap_streaming(&self, handle: DmaMapHandle) {
        if let Some(bounce) = handle.bounce_ptr() {
            release(bounce, handle.size());
        }
    }
}
