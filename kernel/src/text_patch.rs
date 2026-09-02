//! Serialized W^X kernel-text mutation.
//!
//! Callers first reserve their probe metadata, then construct this transaction.
//! It changes one 4 KiB executable leaf to writable/non-executable, performs
//! a bounded volatile byte write, restores the exact original leaf flags, and
//! waits for the all-CPU TLB/icache rendezvous before releasing the gate.

use core::sync::atomic::{AtomicBool, Ordering};

use axerrno::{AxError, AxResult};
use axhal::paging::{MappingFlags, PageSize};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};

static TEXT_MUTATION_GATE: AtomicBool = AtomicBool::new(false);

pub(crate) struct TextPatchTransaction {
    page: VirtAddr,
    original_flags: MappingFlags,
    live: bool,
}

impl TextPatchTransaction {
    pub(crate) fn begin(address: usize) -> AxResult<Self> {
        if TEXT_MUTATION_GATE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AxError::WouldBlock);
        }
        let page = VirtAddr::from(address).align_down(PAGE_SIZE_4K);
        let result = (|| {
            let mut aspace = axmm::kernel_aspace().lock();
            let (_, flags, size) = aspace.query_leaf(page)?;
            if size != PageSize::Size4K
                || !flags.contains(MappingFlags::EXECUTE)
                || flags.contains(MappingFlags::WRITE)
            {
                return Err(AxError::PermissionDenied);
            }
            // Never expose an RWX leaf.  The write permission is temporary
            // and the original execute bit is restored only after the write.
            aspace.protect(
                page,
                PAGE_SIZE_4K,
                (flags | MappingFlags::WRITE) - MappingFlags::EXECUTE,
            )?;
            drop(aspace);
            let _ = crate::mm::synchronize_kernel_text_patch();
            Ok(Self {
                page,
                original_flags: flags,
                live: true,
            })
        })();
        if result.is_err() {
            TEXT_MUTATION_GATE.store(false, Ordering::Release);
        }
        result
    }

    pub(crate) fn replace_byte(&mut self, address: usize, value: u8) -> AxResult<u8> {
        if !self.live || VirtAddr::from(address).align_down(PAGE_SIZE_4K) != self.page {
            return Err(AxError::InvalidInput);
        }
        // SAFETY: begin changed exactly this leaf to writable and removed X;
        // address is constrained to that same leaf until commit/Drop.
        let previous = unsafe { core::ptr::read_volatile(address as *const u8) };
        unsafe { core::ptr::write_volatile(address as *mut u8, value) };
        Ok(previous)
    }

    pub(crate) fn commit(mut self) -> AxResult<()> {
        self.restore()?;
        self.live = false;
        TEXT_MUTATION_GATE.store(false, Ordering::Release);
        Ok(())
    }

    fn restore(&mut self) -> AxResult<()> {
        let mut aspace = axmm::kernel_aspace().lock();
        aspace.protect(self.page, PAGE_SIZE_4K, self.original_flags)?;
        drop(aspace);
        let _ = crate::mm::synchronize_kernel_text_patch();
        Ok(())
    }
}

impl Drop for TextPatchTransaction {
    fn drop(&mut self) {
        if self.live {
            let _ = self.restore();
            TEXT_MUTATION_GATE.store(false, Ordering::Release);
        }
    }
}
