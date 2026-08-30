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

/// An unpublished, writable and non-executable code allocation.
#[must_use = "dropping an unpublished code allocation rolls it back"]
pub(crate) struct WritableCode {
    arena: &'static ExecutableArena,
    first: usize,
    pages: usize,
    code: VirtAddr,
    direct: VirtAddr,
    len: usize,
    armed: bool,
}

/// Reserves and maps an unpublished NX code allocation.
pub(crate) fn prepare(size: usize) -> Result<WritableCode, MemoryError> {
    let arena = ARENA
        .get()
        .ok_or(MemoryError::Unavailable(AxError::BadState))?;
    let (first, pages, code) = arena.reserve(size).map_err(MemoryError::Unavailable)?;
    let direct = match global_allocator().alloc_pages(pages, PAGE_SIZE_4K, UsageKind::Global) {
        Ok(address) => VirtAddr::from_usize(address),
        Err(_) => {
            if !arena.release(first, pages) {
                return Err(MemoryError::Retained(AxError::BadState));
            }
            return Err(MemoryError::Unavailable(AxError::NoMemory));
        }
    };
    let mapped = pages * PAGE_SIZE_4K;
    unsafe { ptr::write_bytes(direct.as_mut_ptr(), 0, mapped) };
    if let Err(error) = axmm::kernel_aspace().lock().map_linear(
        code,
        virt_to_phys(direct),
        mapped,
        MappingFlags::READ | MappingFlags::WRITE,
    ) {
        // Mapping backends may have changed a page-table prefix before
        // reporting failure. Do not release either alias or physical pages
        // on that uncertainty: an NX retained allocation is safer than
        // reusing a page behind a stale partial mapping.
        return Err(MemoryError::Retained(error));
    }
    Ok(WritableCode {
        arena,
        first,
        pages,
        code,
        direct,
        len: size,
        armed: true,
    })
}

/// Changes permissions one page at a time and reports how many pages were
/// changed before the first failure. `AddrSpace::protect` accepts a range but
/// is allowed to stop after a partial page-table walk; keeping the cursor
/// explicit lets callers roll back exactly the pages already transitioned.
fn protect_pages_partial(
    base: VirtAddr,
    pages: usize,
    flags: MappingFlags,
) -> Result<(), (AxError, usize)> {
    let mut aspace = axmm::kernel_aspace().lock();
    for page in 0..pages {
        let address = base + page * PAGE_SIZE_4K;
        if let Err(error) = aspace.protect(address, PAGE_SIZE_4K, flags) {
            return Err((error, page));
        }
    }
    Ok(())
}

fn protect_pages(base: VirtAddr, pages: usize, flags: MappingFlags) -> AxResult<()> {
    protect_pages_partial(base, pages, flags).map_err(|(error, _)| error)
}

fn unmap_pages(base: VirtAddr, pages: usize) -> AxResult<()> {
    let mut aspace = axmm::kernel_aspace().lock();
    for page in 0..pages {
        aspace.unmap(base + page * PAGE_SIZE_4K, PAGE_SIZE_4K)?;
    }
    Ok(())
}

impl WritableCode {
    /// Virtual base of the unpublished image, used to resolve ET_REL
    /// section-relative relocations before W^X publication.
    pub(crate) fn code_address(&self) -> usize {
        self.code.as_usize()
    }
    /// Copies bytes into the unpublished NX alias.
    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> AxResult<()> {
        let end = offset
            .checked_add(bytes.len())
            .ok_or(AxError::InvalidInput)?;
        if end > self.len {
            return Err(AxError::InvalidInput);
        }
        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.code.as_mut_ptr().add(offset),
                bytes.len(),
            )
        };
        Ok(())
    }

    /// Retains this allocation in place after a lifecycle failure.
    ///
    /// The code alias remains NX and the arena slot remains occupied. This is
    /// deliberately a leak/quarantine: a failed permission transition is not
    /// evidence that the mapping can be safely unmapped or reused.
    fn quarantine(&mut self, error: MemoryError) -> MemoryError {
        // The direct alias is never made writable by this helper. Make a
        // best-effort attempt to revoke execute from every code page before
        // retaining the owner. If that attempt also fails, retaining the
        // complete allocation still prevents premature reuse.
        let _ = protect_pages(self.code, self.pages, MappingFlags::READ);
        drop(crate::mm::synchronize_tlb_and_icache());
        self.armed = false;
        error
    }

    /// Best-effort rollback of an unpublished allocation.
    ///
    /// On unmap or release failure the allocation is retained and its slot is
    /// never returned to the bitmap. The method consumes the owner so the
    /// `Drop` implementation cannot retry a partially completed teardown.
    pub(crate) fn abort(mut self, error: MemoryError) -> MemoryError {
        match self.cleanup_unpublished() {
            Ok(()) => error,
            Err(cleanup_error) => cleanup_error,
        }
    }

    fn cleanup_unpublished(&mut self) -> Result<(), MemoryError> {
        if !self.armed {
            return Ok(());
        }
        if let Err(error) = unmap_pages(self.code, self.pages) {
            // `unmap_pages` may have removed a prefix. The code alias was
            // never executable, so retaining all ownership is safe even when
            // the mapping topology is now partial.
            self.armed = false;
            return Err(MemoryError::Retained(error));
        }
        drop(crate::mm::synchronize_tlb_and_icache());
        unsafe { ptr::write_bytes(self.direct.as_mut_ptr(), 0, self.pages * PAGE_SIZE_4K) };
        global_allocator().dealloc_pages(self.direct.as_usize(), self.pages, UsageKind::Global);
        self.armed = false;
        if !self.arena.release(self.first, self.pages) {
            return Err(MemoryError::Retained(AxError::BadState));
        }
        Ok(())
    }

    /// Publishes the complete allocation under strict alias-aware W^X.
    pub(crate) fn publish(mut self, entry_offset: usize) -> Result<ExecutableCode, MemoryError> {
        if entry_offset >= self.len {
            return Err(self.abort(MemoryError::Unavailable(AxError::InvalidInput)));
        }
        if let Err((error, changed)) =
            protect_pages_partial(self.direct, self.pages, MappingFlags::READ)
        {
            if protect_pages(
                self.direct,
                changed,
                MappingFlags::READ | MappingFlags::WRITE,
            )
            .is_err()
            {
                return Err(self.quarantine(MemoryError::Quarantined(error)));
            }
            drop(crate::mm::synchronize_tlb_and_icache());
            return Err(self.abort(MemoryError::Unavailable(error)));
        }
        // No CPU may retain a writable direct-map translation before the code
        // alias becomes executable.
        drop(crate::mm::synchronize_tlb_and_icache());

        if let Err((error, changed)) = protect_pages_partial(
            self.code,
            self.pages,
            MappingFlags::READ | MappingFlags::EXECUTE,
        ) {
            // Do not make any direct-map page writable until every code page
            // that became executable has been revoked and the revocation is
            // globally visible.
            if protect_pages(self.code, changed, MappingFlags::READ).is_err() {
                return Err(self.quarantine(MemoryError::Quarantined(error)));
            }
            drop(crate::mm::synchronize_tlb_and_icache());
            if protect_pages(
                self.direct,
                self.pages,
                MappingFlags::READ | MappingFlags::WRITE,
            )
            .is_err()
            {
                return Err(self.quarantine(MemoryError::Quarantined(error)));
            }
            drop(crate::mm::synchronize_tlb_and_icache());
            return Err(self.abort(MemoryError::Unavailable(error)));
        }
        drop(crate::mm::synchronize_tlb_and_icache());

        self.armed = false;
        Ok(ExecutableCode {
            arena: self.arena,
            first: self.first,
            pages: self.pages,
            code: self.code,
            direct: self.direct,
            len: self.len,
            entry_offset,
            armed: true,
        })
    }
}

impl Drop for WritableCode {
    fn drop(&mut self) {
        let _ = self.cleanup_unpublished();
    }
}

/// Published executable bytes whose borrow is the execution lifetime proof.
#[must_use = "dropping executable code retires and releases its pages"]
pub(crate) struct ExecutableCode {
    arena: &'static ExecutableArena,
    first: usize,
    pages: usize,
    code: VirtAddr,
    direct: VirtAddr,
    len: usize,
    entry_offset: usize,
    armed: bool,
}

impl ExecutableCode {
    /// Invokes a validated SysV x86_64 module entry point.  Module code is
    /// entered only through this owner, so its RX mapping remains pinned for
    /// the whole call and teardown cannot race the instruction fetches.
    pub(crate) fn execute_module_entry(&self, entry_offset: usize) -> i32 {
        debug_assert!(entry_offset < self.len);
        type Entry = extern "C" fn() -> i32;
        let entry = self.code.as_usize() + entry_offset;
        // SAFETY: the ET_REL loader validated that this offset denotes a
        // defined executable symbol in this published allocation. `self`
        // owns the RX mapping for the complete invocation.
        let function: Entry = unsafe { core::mem::transmute(entry) };
        function()
    }
    /// Executes the published SysV x86_64 entry while borrowing the code
    /// owner for the complete call. The only unsafe operation in this
    /// publisher is the typed conversion of the validated, W^X-protected
    /// entry address; callers cannot retain or invoke a raw address.
    pub(crate) fn execute(&self, data: &[u8]) -> u32 {
        debug_assert!(self.entry_offset < self.len);
        let Ok(length) = u32::try_from(data.len()) else {
            return 0;
        };
        type Entry = extern "C" fn(*const u8, u32) -> u32;
        let entry = self.code.as_usize() + self.entry_offset;
        // SAFETY: `publish` validates the entry offset and changes the
        // complete code alias to RX only after a global TLB/icache grace
        // period. `self` keeps the mapping live for this borrow and Drop
        // performs the inverse grace protocol after the call returns.
        let function: Entry = unsafe { core::mem::transmute(entry) };
        function(data.as_ptr(), length)
    }

    /// Retires an executable owner and reports any quarantine/retention state.
    ///
    /// The owner is marked disarmed before the first mutation. If retirement
    /// fails, `Drop` therefore cannot attempt a second, contradictory
    /// transition; the virtual slot and physical pages remain occupied.
    pub(crate) fn retire(mut self) -> Result<(), MemoryError> {
        self.retire_in_place()
    }

    fn retire_in_place(&mut self) -> Result<(), MemoryError> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;

        if let Err((error, _changed)) =
            protect_pages_partial(self.code, self.pages, MappingFlags::READ)
        {
            // Never restore execute after a failed revoke. Retry the full
            // range and, if that still fails, retain/quarantine the owner.
            if protect_pages(self.code, self.pages, MappingFlags::READ).is_err() {
                drop(crate::mm::synchronize_tlb_and_icache());
                let _ = unmap_pages(self.code, self.pages);
                drop(crate::mm::synchronize_tlb_and_icache());
                return Err(MemoryError::Quarantined(error));
            }
        }
        drop(crate::mm::synchronize_tlb_and_icache());

        // Execute has now been revoked globally. A partial direct-map
        // transition is safe to retain because no code alias is executable.
        if let Err(error) = protect_pages(
            self.direct,
            self.pages,
            MappingFlags::READ | MappingFlags::WRITE,
        ) {
            return Err(MemoryError::Retained(error));
        }
        drop(crate::mm::synchronize_tlb_and_icache());

        if let Err(error) = unmap_pages(self.code, self.pages) {
            return Err(MemoryError::Retained(error));
        }
        drop(crate::mm::synchronize_tlb_and_icache());

        let mapped = self.pages * PAGE_SIZE_4K;
        unsafe { ptr::write_bytes(self.direct.as_mut_ptr(), 0, mapped) };
        global_allocator().dealloc_pages(self.direct.as_usize(), self.pages, UsageKind::Global);
        if !self.arena.release(self.first, self.pages) {
            return Err(MemoryError::Retained(AxError::BadState));
        }
        Ok(())
    }
}

impl Drop for ExecutableCode {
    fn drop(&mut self) {
        let _ = self.retire_in_place();
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
