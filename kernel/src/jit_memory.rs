//! Bounded W^X executable memory for verified kernel translators.
//!
//! The arena deliberately separates translation from publication.  A writer
//! receives an NX alias, fills it, and can only consume that owner into an RX
//! executable after the direct-map alias has been made read-only on every CPU.
//! Retirement performs the inverse transition with two global grace periods,
//! so no CPU can retain an executable alias while any writable alias exists.

use core::ptr;

use axalloc::{UsageKind, global_allocator};
use axerrno::{AxError, AxResult};
use axexec::{self, Alias, ExecBackend, Permissions};
use axhal::{mem::virt_to_phys, paging::MappingFlags};
use axsync::spin::SpinNoIrq;
use memory_addr::{PAGE_SIZE_4K, VirtAddr};
use spin::Once;

const ARENA_BYTES: usize = 16 * 1024 * 1024;
const ARENA_PAGES: usize = ARENA_BYTES / PAGE_SIZE_4K;
const ARENA_WORDS: usize = ARENA_PAGES / u64::BITS as usize;
const ARENA_END_GUARD_BYTES: usize = 32 * 1024 * 1024;

// The first mapping creates the shared lower page-table hierarchy before the
// first user address space copies the kernel top-level entry.  It is never
// handed to a translator.
const SENTINEL_PAGE: usize = 0;

static ARENA: Once<ExecutableArena> = Once::new();

/// Failure states for the optional executable-memory owner.
///
/// `Unavailable` means that no allocation was published. `Quarantined` means
/// that an alias transition could not be proven safe; the virtual slot and
/// physical pages are deliberately retained. `Retained` means that execute
/// was revoked, but physical retirement (unmap or slot release) could not be
/// completed. Both latter states intentionally sacrifice bounded capacity to
/// avoid reusing an allocation whose aliases have not been fully retired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemoryError {
    Unavailable(AxError),
    Quarantined(AxError),
    Retained(AxError),
}

struct ExecutableArena {
    base: VirtAddr,
    slots: SpinNoIrq<PageBitmap>,
    // This physical page is intentionally retained until shutdown.  Its code
    // alias is read-only/NX and exists solely to anchor page-table hierarchy.
    _sentinel_direct: VirtAddr,
}

#[derive(Clone, Eq, PartialEq)]
struct PageBitmap {
    words: [u64; ARENA_WORDS],
}

impl PageBitmap {
    const fn new() -> Self {
        let mut words = [0; ARENA_WORDS];
        words[SENTINEL_PAGE / u64::BITS as usize] |= 1_u64 << (SENTINEL_PAGE % u64::BITS as usize);
        Self { words }
    }

    fn occupied(&self, page: usize) -> bool {
        self.words[page / u64::BITS as usize] & (1_u64 << (page % u64::BITS as usize)) != 0
    }

    fn set(&mut self, page: usize, occupied: bool) {
        let mask = 1_u64 << (page % u64::BITS as usize);
        let word = &mut self.words[page / u64::BITS as usize];
        if occupied {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    fn try_reserve(&mut self, pages: usize) -> Option<usize> {
        if pages == 0 || pages > ARENA_PAGES - 1 {
            return None;
        }
        let mut run_start = 0;
        let mut run_len = 0;
        for page in 0..ARENA_PAGES {
            if self.occupied(page) {
                run_len = 0;
                continue;
            }
            if run_len == 0 {
                run_start = page;
            }
            run_len += 1;
            if run_len == pages {
                for reserved in run_start..run_start + pages {
                    self.set(reserved, true);
                }
                return Some(run_start);
            }
        }
        None
    }

    fn release(&mut self, start: usize, pages: usize) -> bool {
        let Some(end) = start.checked_add(pages) else {
            return false;
        };
        if start == SENTINEL_PAGE || end > ARENA_PAGES {
            return false;
        }
        // Validate the complete range before mutating the bitmap. If an
        // internal owner bug reaches this boundary, leaving every page
        // occupied is safer than partially releasing a run for reuse.
        if (start..end).any(|page| !self.occupied(page)) {
            return false;
        }
        for page in start..end {
            self.set(page, false);
        }
        true
    }
}

/// Establishes the executable arena before any user page table is created.
pub(crate) fn init() -> AxResult<()> {
    ARENA.try_call_once(ExecutableArena::try_new).map(|_| ())
}

impl ExecutableArena {
    fn try_new() -> AxResult<Self> {
        let kernel = axmm::kernel_aspace();
        let (kernel_base, kernel_size) = {
            let aspace = kernel.lock();
            (aspace.base().as_usize(), aspace.size())
        };
        let kernel_end = kernel_base
            .checked_add(kernel_size)
            .ok_or(AxError::BadState)?;
        let arena_end = kernel_end
            .checked_sub(ARENA_END_GUARD_BYTES)
            .ok_or(AxError::BadState)?
            & !(PAGE_SIZE_4K - 1);
        let arena_base = arena_end
            .checked_sub(ARENA_BYTES)
            .ok_or(AxError::BadState)?;
        let base = VirtAddr::from_usize(arena_base);
        if !kernel.lock().contains_range(base, ARENA_BYTES) {
            return Err(AxError::BadState);
        }

        let direct = global_allocator()
            .alloc_pages(1, PAGE_SIZE_4K, UsageKind::Global)
            .map(VirtAddr::from_usize)
            .map_err(|_| AxError::NoMemory)?;
        // A stale allocator page must never become observable through the
        // hierarchy anchor.
        unsafe { ptr::write_bytes(direct.as_mut_ptr(), 0, PAGE_SIZE_4K) };
        let sentinel = base + SENTINEL_PAGE * PAGE_SIZE_4K;
        // Keep the anchor page if the mapping backend reports failure:
        // a backend may have installed a page-table prefix before the
        // error. The optional arena is unavailable, so retaining one
        // page is safer than returning it behind a stale mapping.
        kernel.lock().map_linear(
            sentinel,
            virt_to_phys(direct),
            PAGE_SIZE_4K,
            MappingFlags::READ,
        )?;

        Ok(Self {
            base,
            slots: SpinNoIrq::new(PageBitmap::new()),
            _sentinel_direct: direct,
        })
    }

    fn reserve(&self, size: usize) -> AxResult<(usize, usize, VirtAddr)> {
        if size == 0 {
            return Err(AxError::InvalidInput);
        }
        let mapped = size
            .checked_add(PAGE_SIZE_4K - 1)
            .ok_or(AxError::InvalidInput)?
            / PAGE_SIZE_4K
            * PAGE_SIZE_4K;
        let pages = mapped / PAGE_SIZE_4K;
        let first = self
            .slots
            .lock()
            .try_reserve(pages)
            .ok_or(AxError::NoMemory)?;
        Ok((first, pages, self.base + first * PAGE_SIZE_4K))
    }

    fn release(&self, first: usize, pages: usize) -> bool {
        self.slots.lock().release(first, pages)
    }
}

fn protect_pages(base: VirtAddr, pages: usize, flags: MappingFlags) -> AxResult<()> {
    let mut aspace = axmm::kernel_aspace().lock();
    for page in 0..pages {
        aspace.protect(base + page * PAGE_SIZE_4K, PAGE_SIZE_4K, flags)?;
    }
    Ok(())
}

fn unmap_pages(base: VirtAddr, pages: usize) -> AxResult<()> {
    let mut aspace = axmm::kernel_aspace().lock();
    for page in 0..pages {
        aspace.unmap(base + page * PAGE_SIZE_4K, PAGE_SIZE_4K)?;
    }
    Ok(())
}

/// Kernel page-table implementation of the generic dual-alias lifecycle.
/// The allocation owns the direct-map backing while `Mapping` identifies
/// either that alias or its final arena address.
#[derive(Clone, Copy)]
struct Backend {
    arena: &'static ExecutableArena,
}

struct Allocation {
    first: usize,
    pages: usize,
    code: VirtAddr,
    direct: VirtAddr,
}

#[derive(Clone, Copy)]
struct Mapping {
    address: VirtAddr,
    pages: usize,
    final_alias: bool,
}

impl ExecBackend for Backend {
    type Allocation = Allocation;
    type Mapping = Mapping;
    type Error = AxError;

    fn allocate(&self, len: usize) -> AxResult<Allocation> {
        let (first, pages, code) = self.arena.reserve(len)?;
        let direct = match global_allocator().alloc_pages(pages, PAGE_SIZE_4K, UsageKind::Global) {
            Ok(address) => VirtAddr::from_usize(address),
            Err(_) => {
                if !self.arena.release(first, pages) {
                    return Err(AxError::BadState);
                }
                return Err(AxError::NoMemory);
            }
        };
        unsafe { ptr::write_bytes(direct.as_mut_ptr(), 0, pages * PAGE_SIZE_4K) };
        Ok(Allocation {
            first,
            pages,
            code,
            direct,
        })
    }

    fn map(&self, allocation: &Allocation, alias: Alias, _: Permissions) -> AxResult<Mapping> {
        match alias {
            Alias::Direct => Ok(Mapping {
                address: allocation.direct,
                pages: allocation.pages,
                final_alias: false,
            }),
            Alias::Final => {
                axmm::kernel_aspace().lock().map_linear(
                    allocation.code,
                    virt_to_phys(allocation.direct),
                    allocation.pages * PAGE_SIZE_4K,
                    MappingFlags::READ | MappingFlags::WRITE,
                )?;
                Ok(Mapping {
                    address: allocation.code,
                    pages: allocation.pages,
                    final_alias: true,
                })
            }
        }
    }

    fn protect(&self, mapping: &mut Mapping, permissions: Permissions) -> AxResult<()> {
        let flags = match (permissions.write, permissions.execute) {
            (true, false) => MappingFlags::READ | MappingFlags::WRITE,
            (false, true) => MappingFlags::READ | MappingFlags::EXECUTE,
            (false, false) => MappingFlags::READ,
            (true, true) => return Err(AxError::BadState),
        };
        // The backend is deliberately page granular. A failure after a
        // prefix is reported as an error; axexec quarantines the complete
        // owner rather than making an unsafe best-effort reuse decision.
        protect_pages(mapping.address, mapping.pages, flags)
    }

    fn writable_bytes<'a>(&self, mapping: &'a mut Mapping, len: usize) -> AxResult<&'a mut [u8]> {
        if mapping.final_alias {
            return Err(AxError::BadState);
        }
        Ok(unsafe { core::slice::from_raw_parts_mut(mapping.address.as_mut_ptr(), len) })
    }

    fn unmap(&self, mapping: Mapping) -> AxResult<()> {
        if mapping.final_alias {
            unmap_pages(mapping.address, mapping.pages)
        } else {
            Ok(())
        }
    }

    fn deallocate(&self, allocation: Allocation) {
        unsafe {
            ptr::write_bytes(
                allocation.direct.as_mut_ptr(),
                0,
                allocation.pages * PAGE_SIZE_4K,
            )
        };
        global_allocator().dealloc_pages(
            allocation.direct.as_usize(),
            allocation.pages,
            UsageKind::Global,
        );
        let _ = self.arena.release(allocation.first, allocation.pages);
    }

    fn synchronize_tlb(&self) -> AxResult<()> {
        drop(crate::mm::synchronize_tlb());
        Ok(())
    }
    fn synchronize_icache(&self) -> AxResult<()> {
        drop(crate::mm::synchronize_icache());
        Ok(())
    }
}

type RawWritable = axexec::Writable<Backend>;
enum RawPublished {
    Executable(axexec::PublishedExecutable<Backend>),
    Readonly(axexec::PublishedReadonly<Backend>),
}

pub(crate) struct WritableCode {
    raw: RawWritable,
    code: VirtAddr,
    len: usize,
}
pub(crate) struct ExecutableCode {
    raw: RawPublished,
    code: VirtAddr,
    len: usize,
    entry_offset: usize,
}

pub(crate) fn prepare(size: usize) -> Result<WritableCode, MemoryError> {
    let arena = ARENA
        .get()
        .ok_or(MemoryError::Unavailable(AxError::BadState))?;
    let raw = axexec::allocate(Backend { arena }, size).map_err(MemoryError::Unavailable)?;
    let code = raw.final_mapping().address;
    Ok(WritableCode {
        raw,
        code,
        len: size,
    })
}
pub(crate) fn prepare_module_data(size: usize) -> Result<WritableCode, MemoryError> {
    prepare(size)
}

impl WritableCode {
    pub(crate) fn code_address(&self) -> usize {
        self.code.as_usize()
    }
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        self.raw.bytes_mut().expect("writable direct alias")
    }
    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> AxResult<()> {
        let end = offset
            .checked_add(bytes.len())
            .ok_or(AxError::InvalidInput)?;
        if end > self.len {
            return Err(AxError::InvalidInput);
        }
        self.bytes_mut()[offset..end].copy_from_slice(bytes);
        Ok(())
    }
    pub(crate) fn abort(self, error: MemoryError) -> MemoryError {
        match self.raw.abort() {
            Ok(()) => error,
            Err(failure) => {
                drop(failure.allocation);
                MemoryError::Retained(failure.error)
            }
        }
    }
    pub(crate) fn publish(self, entry_offset: usize) -> Result<ExecutableCode, MemoryError> {
        if entry_offset >= self.len {
            return Err(self.abort(MemoryError::Unavailable(AxError::InvalidInput)));
        }
        let code = self.code;
        let len = self.len;
        match self.raw.publish() {
            Ok(raw) => Ok(ExecutableCode {
                raw: RawPublished::Executable(raw),
                code,
                len,
                entry_offset,
            }),
            Err(failure) => {
                drop(failure.allocation);
                Err(MemoryError::Quarantined(failure.error))
            }
        }
    }
    pub(crate) fn publish_readonly(self) -> Result<ExecutableCode, MemoryError> {
        let code = self.code;
        let len = self.len;
        match self.raw.publish_readonly() {
            Ok(raw) => Ok(ExecutableCode {
                raw: RawPublished::Readonly(raw),
                code,
                len,
                entry_offset: 0,
            }),
            Err(failure) => {
                drop(failure.allocation);
                Err(MemoryError::Quarantined(failure.error))
            }
        }
    }
}

impl ExecutableCode {
    /// Whether an address belongs to this published executable allocation.
    ///
    /// Module symbol tables can contain data and rodata exports as well as
    /// code.  Consumers which intend to patch an instruction must use this
    /// ownership check instead of accepting an arbitrary exported address.
    pub(crate) fn contains_executable_address(&self, address: usize) -> bool {
        matches!(&self.raw, RawPublished::Executable(_))
            && address >= self.code.as_usize()
            && address
                .checked_sub(self.code.as_usize())
                .is_some_and(|offset| offset < self.len)
    }

    pub(crate) fn execute_module_entry(
        &self,
        entry_offset: usize,
        entry_size: usize,
    ) -> Option<i32> {
        let entry_end = entry_offset.checked_add(entry_size)?;
        if entry_size == 0 || entry_offset >= self.len || entry_end > self.len {
            return None;
        }
        let RawPublished::Executable(_) = self.raw else {
            return None;
        };
        let function: extern "C" fn() -> i32 =
            unsafe { core::mem::transmute(self.code.as_usize() + entry_offset) };
        Some(function())
    }
    pub(crate) fn execute(&self, data: &[u8]) -> u32 {
        if !matches!(self.raw, RawPublished::Executable(_)) || self.entry_offset >= self.len {
            return 0;
        }
        let Ok(length) = u32::try_from(data.len()) else {
            return 0;
        };
        let function: extern "C" fn(*const u8, u32) -> u32 =
            unsafe { core::mem::transmute(self.code.as_usize() + self.entry_offset) };
        function(data.as_ptr(), length)
    }
    pub(crate) fn retire(self) -> Result<(), MemoryError> {
        let result = match self.raw {
            RawPublished::Executable(raw) => raw.retire(),
            RawPublished::Readonly(raw) => raw.retire(),
        };
        result.map_err(|failure| {
            let error = failure.error;
            drop(failure.allocation);
            MemoryError::Retained(error)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_reserves_sentinel_and_reuses_exact_run() {
        let mut bitmap = PageBitmap::new();
        assert!(bitmap.occupied(SENTINEL_PAGE));
        let first = bitmap.try_reserve(3).unwrap();
        assert_eq!(first, 1);
        assert!(bitmap.occupied(1));
        assert!(bitmap.occupied(2));
        assert!(bitmap.occupied(3));
        bitmap.release(first, 3);
        assert_eq!(bitmap.try_reserve(3), Some(first));
    }

    #[test]
    fn bitmap_rejects_zero_and_oversized_requests_without_mutation() {
        let mut bitmap = PageBitmap::new();
        let before = bitmap.clone();
        assert_eq!(bitmap.try_reserve(0), None);
        assert_eq!(bitmap.try_reserve(ARENA_PAGES), None);
        assert!(bitmap == before);
    }

    #[test]
    fn arena_constants_are_page_and_bitmap_aligned() {
        assert_eq!(ARENA_BYTES % PAGE_SIZE_4K, 0);
        assert_eq!(ARENA_PAGES % u64::BITS as usize, 0);
        const _: () = assert!(ARENA_END_GUARD_BYTES >= ARENA_BYTES);
    }

    #[test]
    fn bitmap_rejects_double_release_without_releasing_other_pages() {
        let mut bitmap = PageBitmap::new();
        let first = bitmap.try_reserve(1).unwrap();
        assert!(bitmap.release(first, 1));
        assert!(!bitmap.release(first, 1));
        assert!(bitmap.occupied(SENTINEL_PAGE));
    }
}
