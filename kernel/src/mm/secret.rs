//! Secret-memory frames have no permanent kernel direct-map alias.
//!
//! The only kernel access path is a short, CPU-pinned mapping in the reserved
//! secret window.  In particular, callers must never turn a `SecretFrame`
//! into `phys_to_virt(frame)`.

use core::ptr;

use axalloc::{UsageKind, global_allocator};
use axerrno::{AxError, AxResult};
use axhal::{
    mem::{phys_to_virt, virt_to_phys},
    paging::{MappingFlags, PreparedPageTableFrames},
};
use axsync::spin::SpinNoIrq;
use kernel_guard::NoPreempt;
use kspin::SpinNoIrqGuard;
use memory_addr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use super::tlb::synchronize_kernel_map_tlb;

static WINDOW_LOCKS: [SpinNoIrq<()>; axconfig::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(()) }; axconfig::plat::MAX_CPU_NUM];

/// One 4 KiB physical frame whose direct-map PTE has been removed.
#[derive(Debug)]
pub(crate) struct SecretFrame {
    physical: PhysAddr,
}

impl SecretFrame {
    pub(crate) fn allocate() -> AxResult<Self> {
        let direct = global_allocator()
            .alloc_pages(1, PAGE_SIZE_4K, UsageKind::VirtMem)
            .map(VirtAddr::from_usize)
            .map_err(|_| AxError::NoMemory)?;
        // A frame is cleared before its alias is revoked.  No stale allocator
        // contents can escape through a subsequently installed user mapping.
        unsafe { ptr::write_bytes(direct.as_mut_ptr(), 0, PAGE_SIZE_4K) };
        let physical = virt_to_phys(direct);
        if let Err(error) = remove_direct_alias(direct) {
            global_allocator().dealloc_pages(direct.as_usize(), 1, UsageKind::VirtMem);
            return Err(error);
        }
        Ok(Self { physical })
    }

    pub(crate) const fn physical(&self) -> PhysAddr {
        self.physical
    }

    pub(crate) fn copy_from(&self, source: &[u8], offset: usize) -> AxResult<()> {
        let end = offset
            .checked_add(source.len())
            .ok_or(AxError::InvalidInput)?;
        if end > PAGE_SIZE_4K {
            return Err(AxError::InvalidInput);
        }
        let window = SecretWindow::map(self.physical)?;
        unsafe {
            ptr::copy_nonoverlapping(
                source.as_ptr(),
                window.address().as_mut_ptr().add(offset),
                source.len(),
            )
        };
        Ok(())
    }

    pub(crate) fn copy_to(&self, destination: &mut [u8], offset: usize) -> AxResult<()> {
        let end = offset
            .checked_add(destination.len())
            .ok_or(AxError::InvalidInput)?;
        if end > PAGE_SIZE_4K {
            return Err(AxError::InvalidInput);
        }
        let window = SecretWindow::map(self.physical)?;
        unsafe {
            ptr::copy_nonoverlapping(
                window.address().as_ptr().add(offset),
                destination.as_mut_ptr(),
                destination.len(),
            )
        };
        Ok(())
    }
}

impl Drop for SecretFrame {
    fn drop(&mut self) {
        // Zero through the sole permitted alias before restoring the allocator's
        // direct mapping.  A failed restore deliberately leaks the frame: reuse
        // behind an uncertain alias would violate secret-memory isolation.
        let Ok(window) = SecretWindow::map(self.physical) else {
            return;
        };
        unsafe { ptr::write_bytes(window.address().as_mut_ptr(), 0, PAGE_SIZE_4K) };
        drop(window);
        let direct = phys_to_virt(self.physical);
        if axmm::kernel_aspace()
            .lock()
            .map_linear(
                direct,
                self.physical,
                PAGE_SIZE_4K,
                MappingFlags::READ | MappingFlags::WRITE,
            )
            .is_err()
        {
            return;
        }
        drop(synchronize_kernel_map_tlb());
        global_allocator().dealloc_pages(direct.as_usize(), 1, UsageKind::VirtMem);
    }
}

/// CPU-local temporary secret mapping.  Drop unmaps it before allowing task
/// migration or another user of this CPU's slot.
struct SecretWindow {
    address: VirtAddr,
    _pin: NoPreempt,
    _lock: SpinNoIrqGuard<'static, ()>,
}

impl SecretWindow {
    fn map(physical: PhysAddr) -> AxResult<Self> {
        let pin = NoPreempt::new();
        let cpu = axhal::percpu::this_cpu_id();
        let lock = WINDOW_LOCKS.get(cpu).ok_or(AxError::BadState)?.lock();
        // The platform config reserves MAX_CPU_NUM slots immediately below
        // the exclusive kernel-aspace end. Deriving it here keeps host tests
        // and every x86 platform on the same ABI without a second config API.
        let kernel = axmm::kernel_aspace();
        let aspace = kernel.lock();
        let base = aspace
            .base()
            .as_usize()
            .checked_add(aspace.size())
            .ok_or(AxError::BadState)?
            .checked_sub(axconfig::plat::MAX_CPU_NUM * PAGE_SIZE_4K)
            .ok_or(AxError::BadState)?;
        drop(aspace);
        let address = VirtAddr::from_usize(
            base.checked_add(cpu.checked_mul(PAGE_SIZE_4K).ok_or(AxError::BadState)?)
                .ok_or(AxError::BadState)?,
        );
        axmm::kernel_aspace().lock().map_linear(
            address,
            physical,
            PAGE_SIZE_4K,
            MappingFlags::READ | MappingFlags::WRITE,
        )?;
        drop(synchronize_kernel_map_tlb());
        Ok(Self {
            address,
            _pin: pin,
            _lock: lock,
        })
    }

    fn address(&self) -> VirtAddr {
        self.address
    }
}

impl Drop for SecretWindow {
    fn drop(&mut self) {
        if axmm::kernel_aspace()
            .lock()
            .unmap(self.address, PAGE_SIZE_4K)
            .is_ok()
        {
            drop(synchronize_kernel_map_tlb());
        }
    }
}

fn remove_direct_alias(direct: VirtAddr) -> AxResult<()> {
    // A direct map may use a 1G or 2M leaf. Reserve all demotion tables before
    // entering the shared-root mutation gate, then publish one 4K unmap.
    let mut tables = PreparedPageTableFrames::try_max().map_err(|_| AxError::NoMemory)?;
    let result: AxResult = axhal::paging::with_active_kernel_page_table(|table| {
        let mut cursor = table.cursor();
        cursor
            .demote_leaf_to_4k_prepared(direct, &mut tables)
            .map_err(AxError::from)?;
        Ok(())
    });
    result?;
    // Keep axmm's kernel-area registry in lockstep with the shared PTE root.
    // The raw cursor above only performs the prerequisite huge-leaf demotion;
    // the managed unmap records the missing 4K direct-map interval so Drop's
    // managed map_linear restoration can safely return the frame to alloc.
    axmm::kernel_aspace().lock().unmap(direct, PAGE_SIZE_4K)?;
    drop(synchronize_kernel_map_tlb());
    Ok(())
}
