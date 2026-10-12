// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Port of Linux 7.2.3
// arch/x86/mm/pat/set_memory.c: change_page_attr_set_clr() for cache bits,
// cpa_flush() for TLB and cache write-back, and set_memory_{uc,wc,wb,wt}()
// with their memtype reservation ordering. Huge-leaf splitting is done by
// the page-table layer when a sub-range is protected (see `DirectMap`).
//! Changing the cache attribute of direct-map pages.

use alloc::vec::Vec;

use axerrno::{AxError, AxResult};
use axhal::paging::MappingFlags;

use super::{CacheMode, CacheTables, MemtypeError, MemtypeState, ReservePolicy, pat::strip_cache};

const PAGE: usize = 4096;
const LINE: usize = 64;

/// One leaf of the direct map: `size` bytes at `va`, mapping `pa`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Leaf {
    pub va: usize,
    pub pa: usize,
    pub size: usize,
    pub flags: MappingFlags,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpaError {
    /// The address is not mapped by a leaf.
    NotMapped,
    /// The page-table update was refused.
    Protect,
    /// The request cannot be expressed, or the range is not linear.
    Invalid,
    Memtype(MemtypeError),
}

impl From<CpaError> for AxError {
    fn from(error: CpaError) -> Self {
        match error {
            CpaError::NotMapped | CpaError::Invalid => AxError::InvalidInput,
            CpaError::Protect => AxError::BadState,
            CpaError::Memtype(e) => e.into(),
        }
    }
}

/// The page-table operations `set_memory_*` needs.
pub trait DirectMap {
    /// The leaf covering `va`, or `None` if `va` is unmapped.
    fn leaf(&self, va: usize) -> Option<Leaf>;
    /// Replace the flags of `[va, va + len)`. A sub-range of a huge leaf must
    /// split that leaf so only the requested pages change.
    fn protect(&mut self, va: usize, len: usize, flags: MappingFlags) -> Result<(), CpaError>;
    /// Invalidate translations for `[va, va + len)` on every CPU.
    fn flush_tlb(&mut self, va: usize, len: usize);
    /// Write back and invalidate the cache lines of `[va, va + len)`.
    fn flush_cache(&mut self, va: usize, len: usize);
}

/// Physical address of `va` under the linear map that `leaf` describes.
fn phys_of(map: &impl DirectMap, va: usize) -> Result<usize, CpaError> {
    let leaf = map.leaf(va).ok_or(CpaError::NotMapped)?;
    Ok(leaf.pa + (va - leaf.va))
}

/// One pending flag change, kept so a failed batch can be rolled back.
#[derive(Clone, Copy)]
struct Change {
    va: usize,
    len: usize,
    old: MappingFlags,
    new: MappingFlags,
}

/// `change_page_attr_set_clr` for the cache bits of `[va, va + pages * 4K)`.
/// All leaves are resolved before anything is changed; if a protect fails,
/// the earlier protects of this call are undone.
pub fn change_cache(
    map: &mut impl DirectMap,
    tables: &CacheTables,
    va: usize,
    pages: usize,
    mode: CacheMode,
) -> Result<(), CpaError> {
    if pages == 0 || !va.is_multiple_of(PAGE) {
        return Err(CpaError::Invalid);
    }
    let len = pages.checked_mul(PAGE).ok_or(CpaError::Invalid)?;
    let end = va.checked_add(len).ok_or(CpaError::Invalid)?;
    let cache_flags = tables.flags_for(mode).ok_or(CpaError::Invalid)?;
    let pa0 = phys_of(map, va)?;

    let mut changes: Vec<Change> = Vec::new();
    let mut cursor = va;
    while cursor < end {
        let leaf = map.leaf(cursor).ok_or(CpaError::NotMapped)?;
        // The direct map is linear: a leaf must continue the physical run.
        if leaf.pa + (cursor - leaf.va) != pa0 + (cursor - va) {
            return Err(CpaError::Invalid);
        }
        let leaf_end = leaf.va.checked_add(leaf.size).ok_or(CpaError::Invalid)?;
        let run_end = leaf_end.min(end);
        let new = strip_cache(leaf.flags) | cache_flags;
        if new != leaf.flags {
            changes.push(Change {
                va: cursor,
                len: run_end - cursor,
                old: leaf.flags,
                new,
            });
        }
        cursor = run_end;
    }
    if changes.is_empty() {
        return Ok(());
    }

    for (index, change) in changes.iter().enumerate() {
        if map.protect(change.va, change.len, change.new).is_err() {
            for undo in changes[..index].iter().rev() {
                // Restoring flags that were just in place cannot fail here.
                let _ = map.protect(undo.va, undo.len, undo.old);
            }
            return Err(CpaError::Protect);
        }
    }

    // cpa_flush(): translations first, then the cache lines that the old
    // attribute may have left dirty or stale.
    map.flush_tlb(va, len);
    for change in &changes {
        map.flush_cache(change.va, change.len);
    }
    Ok(())
}

/// The state that memtype and the page table must change together.
pub struct CpaContext<'a> {
    pub state: &'a mut MemtypeState,
    pub ram: &'a [(usize, usize)],
    pub pat: bool,
    pub tables: CacheTables,
}

fn set_with_memtype(
    ctx: &mut CpaContext<'_>,
    map: &mut impl DirectMap,
    va: usize,
    pages: usize,
    mode: CacheMode,
) -> Result<(), CpaError> {
    let pa = phys_of(map, va)? as u64;
    let end = pa + (pages * PAGE) as u64;
    // Linux takes the memtype first; a failed page-table update releases it.
    let granted = ctx
        .state
        .reserve(ctx.ram, ctx.pat, pa, end, mode, ReservePolicy::Exact)
        .map_err(CpaError::Memtype)?;
    debug_assert_eq!(granted, mode);
    if let Err(error) = change_cache(map, &ctx.tables, va, pages, mode) {
        let _ = ctx.state.free(ctx.ram, ctx.pat, pa, end);
        return Err(error);
    }
    Ok(())
}

/// `set_memory_uc()`: strong-ish uncached (UC-) for `pages` direct-map pages.
pub fn set_memory_uc(ctx: &mut CpaContext<'_>, map: &mut impl DirectMap, va: usize, pages: usize) -> Result<(), CpaError> {
    set_with_memtype(ctx, map, va, pages, CacheMode::UcMinus)
}

/// `set_memory_wc()`: write-combining, through PAT slot 1.
pub fn set_memory_wc(ctx: &mut CpaContext<'_>, map: &mut impl DirectMap, va: usize, pages: usize) -> Result<(), CpaError> {
    set_with_memtype(ctx, map, va, pages, CacheMode::Wc)
}

/// `set_memory_wt()`: write-through. Only expressible with PAT slot 7 under
/// the full palette, which `MappingFlags` cannot encode, so it is refused and
/// the reservation released.
pub fn set_memory_wt(ctx: &mut CpaContext<'_>, map: &mut impl DirectMap, va: usize, pages: usize) -> Result<(), CpaError> {
    set_with_memtype(ctx, map, va, pages, CacheMode::Wt)
}

/// `set_memory_wb()`: restore write-back, then release the memtype.
pub fn set_memory_wb(ctx: &mut CpaContext<'_>, map: &mut impl DirectMap, va: usize, pages: usize) -> Result<(), CpaError> {
    let pa = phys_of(map, va)? as u64;
    change_cache(map, &ctx.tables, va, pages, CacheMode::Wb)?;
    ctx.state
        .free(ctx.ram, ctx.pat, pa, pa + (pages * PAGE) as u64)
        .map_err(CpaError::Memtype)
}

/// `set_pages_array_wc()`: each page is changed separately, as Linux does
/// for page arrays that need not be contiguous.
pub fn set_pages_array_wc(ctx: &mut CpaContext<'_>, map: &mut impl DirectMap, pages: &[usize]) -> Result<(), CpaError> {
    for (done, &va) in pages.iter().enumerate() {
        if let Err(error) = set_memory_wc(ctx, map, va, 1) {
            for &undo in &pages[..done] {
                let _ = set_memory_wb(ctx, map, undo, 1);
            }
            return Err(error);
        }
    }
    Ok(())
}

/// `set_pages_array_wb()`.
pub fn set_pages_array_wb(ctx: &mut CpaContext<'_>, map: &mut impl DirectMap, pages: &[usize]) -> Result<(), CpaError> {
    for &va in pages {
        set_memory_wb(ctx, map, va, 1)?;
    }
    Ok(())
}

/// Flush `[va, va + len)` with CLFLUSHOPT-free `clflush`, one line at a time.
/// This is a per-CPU operation; the caller's [`DirectMap::flush_cache`] is
/// responsible for any cross-CPU step.
#[cfg(target_arch = "x86_64")]
pub fn clflush_range(va: usize, len: usize) {
    let start = va & !(LINE - 1);
    let end = va + len;
    let mut line = start;
    while line < end {
        // SAFETY: CLFLUSH only writes back and invalidates a line; it does not
        // read or write the byte. The address lies in the direct map.
        unsafe { core::arch::x86_64::_mm_clflush(line as *const u8) };
        line += LINE;
    }
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
}

/// The live kernel address space as a [`DirectMap`].
pub struct KernelDirectMap<'a> {
    aspace: &'a mut crate::AddrSpace,
}

impl DirectMap for KernelDirectMap<'_> {
    fn leaf(&self, va: usize) -> Option<Leaf> {
        let (pa, flags, size) = self
            .aspace
            .query_leaf(memory_addr::VirtAddr::from_usize(va))
            .ok()?;
        let size: usize = size.into();
        Some(Leaf {
            va: va & !(size - 1),
            pa: pa.as_usize(),
            size,
            flags,
        })
    }

    fn protect(&mut self, va: usize, len: usize, flags: MappingFlags) -> Result<(), CpaError> {
        self.aspace
            .protect(memory_addr::VirtAddr::from_usize(va), len, flags)
            .map_err(|_| CpaError::Protect)
    }

    fn flush_tlb(&mut self, _va: usize, _len: usize) {
        // Every CPU must drop the translations, not only this one.
        crate::synchronize_kernel_map_tlb().expect("kernel-map TLB synchronizer missing");
    }

    fn flush_cache(&mut self, va: usize, len: usize) {
        #[cfg(target_arch = "x86_64")]
        clflush_range(va, len);
        #[cfg(not(target_arch = "x86_64"))]
        let _ = (va, len);
    }
}

fn with_kernel<T>(f: impl FnOnce(&mut CpaContext<'_>, &mut KernelDirectMap<'_>) -> Result<T, CpaError>) -> AxResult<T> {
    if !crate::kernel_map_tlb_sync_installed() {
        return Err(AxError::BadState);
    }
    let mut state = super::STATE.lock();
    let mut aspace = crate::kernel_aspace().lock();
    let mut ctx = CpaContext {
        state: &mut state,
        ram: super::platform_ram(),
        pat: super::pat_enabled(),
        tables: super::tables(),
    };
    let mut map = KernelDirectMap { aspace: &mut aspace };
    Ok(f(&mut ctx, &mut map)?)
}

/// Linux `set_memory_uc()` on kernel direct-map pages at `va`.
pub fn set_memory_uc_kernel(va: usize, pages: usize) -> AxResult {
    with_kernel(|c, m| set_memory_uc(c, m, va, pages))
}

/// Linux `set_memory_wc()` on kernel direct-map pages at `va`.
pub fn set_memory_wc_kernel(va: usize, pages: usize) -> AxResult {
    with_kernel(|c, m| set_memory_wc(c, m, va, pages))
}

/// Linux `set_memory_wt()` on kernel direct-map pages at `va`. Refused until
/// the PTE encoding can express PAT slot 7.
pub fn set_memory_wt_kernel(va: usize, pages: usize) -> AxResult {
    with_kernel(|c, m| set_memory_wt(c, m, va, pages))
}

/// Linux `set_memory_wb()` on kernel direct-map pages at `va`.
pub fn set_memory_wb_kernel(va: usize, pages: usize) -> AxResult {
    with_kernel(|c, m| set_memory_wb(c, m, va, pages))
}

/// Linux `set_pages_array_wc()` over single direct-map pages.
pub fn set_pages_array_wc_kernel(pages: &[usize]) -> AxResult {
    with_kernel(|c, m| set_pages_array_wc(c, m, pages))
}

/// Linux `set_pages_array_wb()` over single direct-map pages.
pub fn set_pages_array_wb_kernel(pages: &[usize]) -> AxResult {
    with_kernel(|c, m| set_pages_array_wb(c, m, pages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A mock direct map of leaves. A protect that covers part of a 2 MiB
    /// leaf splits it into 4 KiB leaves, as the real page table does.
    struct Mock {
        leaves: Vec<Leaf>,
        protects: usize,
        fail_protect_at: Option<usize>,
        tlb_flushes: usize,
        cache_flushes: Vec<(usize, usize)>,
    }

    impl Mock {
        fn huge(va: usize, pa: usize, size: usize, flags: MappingFlags) -> Self {
            Self {
                leaves: vec![Leaf { va, pa, size, flags }],
                protects: 0,
                fail_protect_at: None,
                tlb_flushes: 0,
                cache_flushes: Vec::new(),
            }
        }

        fn flags_at(&self, va: usize) -> MappingFlags {
            self.leaf(va).unwrap().flags
        }
    }

    impl DirectMap for Mock {
        fn leaf(&self, va: usize) -> Option<Leaf> {
            self.leaves
                .iter()
                .copied()
                .find(|l| va >= l.va && va < l.va + l.size)
        }

        fn protect(&mut self, va: usize, len: usize, flags: MappingFlags) -> Result<(), CpaError> {
            let index = self.protects;
            self.protects += 1;
            if self.fail_protect_at == Some(index) {
                return Err(CpaError::Protect);
            }
            let mut out = Vec::new();
            for leaf in self.leaves.drain(..) {
                let (ls, le) = (leaf.va, leaf.va + leaf.size);
                let (ps, pe) = (va.max(ls), (va + len).min(le));
                if ps >= pe {
                    out.push(leaf);
                    continue;
                }
                if ps == ls && pe == le {
                    out.push(Leaf { flags, ..leaf });
                    continue;
                }
                // Split into 4 KiB leaves and retag the covered ones.
                let mut page = ls;
                while page < le {
                    let covered = page >= va && page < va + len;
                    out.push(Leaf {
                        va: page,
                        pa: leaf.pa + (page - ls),
                        size: PAGE,
                        flags: if covered { flags } else { leaf.flags },
                    });
                    page += PAGE;
                }
            }
            out.sort_by_key(|l| l.va);
            self.leaves = out;
            Ok(())
        }

        fn flush_tlb(&mut self, _va: usize, _len: usize) {
            self.tlb_flushes += 1;
        }

        fn flush_cache(&mut self, va: usize, len: usize) {
            self.cache_flushes.push((va, len));
        }
    }

    const RAM_BASE: usize = 0x2000_0000;
    const RAM: &[(usize, usize)] = &[(RAM_BASE, 0x1000_0000)];
    const DIRECT: usize = 0xffff_8000_2000_0000;
    const RW: MappingFlags = MappingFlags::READ.union(MappingFlags::WRITE);

    fn ctx<'a>(state: &'a mut MemtypeState) -> CpaContext<'a> {
        CpaContext {
            state,
            ram: RAM,
            pat: true,
            tables: CacheTables::from_pat(super::super::PAT_LINUX_FULL),
        }
    }

    #[test]
    fn set_memory_wc_splits_a_huge_leaf_and_changes_only_the_pages_asked() {
        let mut map = Mock::huge(DIRECT, RAM_BASE, 0x20_0000, RW);
        let mut state = MemtypeState::new();
        let va = DIRECT + 0x10000;
        set_memory_wc(&mut ctx(&mut state), &mut map, va, 4).unwrap();

        assert_eq!(map.flags_at(va), RW | MappingFlags::WRITE_COMBINING);
        assert_eq!(map.flags_at(va + 3 * PAGE), RW | MappingFlags::WRITE_COMBINING);
        assert_eq!(map.flags_at(va + 4 * PAGE), RW);
        assert_eq!(map.flags_at(DIRECT), RW);
        assert_eq!(map.leaf(DIRECT).unwrap().size, PAGE);
        assert_eq!(map.tlb_flushes, 1);
        assert_eq!(map.cache_flushes, vec![(va, 4 * PAGE)]);
        assert_eq!(state.lookup(RAM, true, (RAM_BASE + 0x10000) as u64), CacheMode::Wc);
        assert_eq!(state.lookup(RAM, true, RAM_BASE as u64), CacheMode::Wb);
    }

    #[test]
    fn wc_on_ram_is_recorded_per_page_and_wb_releases_it() {
        let mut map = Mock::huge(DIRECT, RAM_BASE, 0x20_0000, RW);
        let mut state = MemtypeState::new();
        set_memory_wc(&mut ctx(&mut state), &mut map, DIRECT, 2).unwrap();
        assert_eq!(state.lookup(RAM, true, RAM_BASE as u64), CacheMode::Wc);
        assert_eq!(state.lookup(RAM, true, (RAM_BASE + PAGE) as u64), CacheMode::Wc);
        assert_eq!(state.lookup(RAM, true, (RAM_BASE + 2 * PAGE) as u64), CacheMode::Wb);

        set_memory_wb(&mut ctx(&mut state), &mut map, DIRECT, 2).unwrap();
        assert_eq!(map.flags_at(DIRECT), RW);
        assert_eq!(state.lookup(RAM, true, RAM_BASE as u64), CacheMode::Wb);
    }

    #[test]
    fn uc_marks_uncached_and_conflicting_wc_is_refused_without_touching_pages() {
        let mut map = Mock::huge(DIRECT, RAM_BASE, 0x20_0000, RW);
        let mut state = MemtypeState::new();
        set_memory_uc(&mut ctx(&mut state), &mut map, DIRECT, 1).unwrap();
        assert_eq!(map.flags_at(DIRECT), RW | MappingFlags::UNCACHED);

        let before = map.protects;
        let err = set_memory_wc(&mut ctx(&mut state), &mut map, DIRECT, 1).unwrap_err();
        assert_eq!(
            err,
            CpaError::Memtype(MemtypeError::Busy {
                tracked: CacheMode::UcMinus,
                requested: CacheMode::Wc,
            })
        );
        assert_eq!(map.protects, before);
        assert_eq!(map.flags_at(DIRECT), RW | MappingFlags::UNCACHED);
    }

    #[test]
    fn a_refused_protect_rolls_back_earlier_runs_and_releases_the_memtype() {
        // Two 4 KiB leaves, the second fails.
        let mut map = Mock {
            leaves: vec![
                Leaf { va: DIRECT, pa: RAM_BASE, size: PAGE, flags: RW },
                Leaf { va: DIRECT + PAGE, pa: RAM_BASE + PAGE, size: PAGE, flags: RW },
            ],
            protects: 0,
            fail_protect_at: Some(1),
            tlb_flushes: 0,
            cache_flushes: Vec::new(),
        };
        let mut state = MemtypeState::new();
        let err = set_memory_wc(&mut ctx(&mut state), &mut map, DIRECT, 2).unwrap_err();
        assert_eq!(err, CpaError::Protect);
        assert_eq!(map.flags_at(DIRECT), RW);
        assert_eq!(map.flags_at(DIRECT + PAGE), RW);
        assert_eq!(state.lookup(RAM, true, RAM_BASE as u64), CacheMode::Wb);
        assert_eq!(map.tlb_flushes, 0);
    }

    #[test]
    fn write_through_is_refused_because_mapping_flags_cannot_encode_it() {
        let mut map = Mock::huge(DIRECT, RAM_BASE, 0x20_0000, RW);
        let mut state = MemtypeState::new();
        assert_eq!(
            set_memory_wt(&mut ctx(&mut state), &mut map, DIRECT, 1),
            Err(CpaError::Invalid)
        );
        assert_eq!(state.lookup(RAM, true, RAM_BASE as u64), CacheMode::Wb);
        assert_eq!(map.flags_at(DIRECT), RW);
    }

    #[test]
    fn non_linear_or_unmapped_ranges_are_refused() {
        let mut map = Mock {
            leaves: vec![
                Leaf { va: DIRECT, pa: RAM_BASE, size: PAGE, flags: RW },
                Leaf { va: DIRECT + PAGE, pa: RAM_BASE + 8 * PAGE, size: PAGE, flags: RW },
            ],
            protects: 0,
            fail_protect_at: None,
            tlb_flushes: 0,
            cache_flushes: Vec::new(),
        };
        let t = CacheTables::from_pat(super::super::PAT_LINUX_FULL);
        assert_eq!(change_cache(&mut map, &t, DIRECT, 2, CacheMode::Wc), Err(CpaError::Invalid));
        assert_eq!(change_cache(&mut map, &t, DIRECT + 4 * PAGE, 1, CacheMode::Wc), Err(CpaError::NotMapped));
        assert_eq!(change_cache(&mut map, &t, DIRECT + 1, 1, CacheMode::Wc), Err(CpaError::Invalid));
        assert_eq!(change_cache(&mut map, &t, DIRECT, 0, CacheMode::Wc), Err(CpaError::Invalid));
    }

    #[test]
    fn pages_array_wc_is_all_or_nothing() {
        let mut map = Mock {
            leaves: vec![
                Leaf { va: DIRECT, pa: RAM_BASE, size: PAGE, flags: RW },
                Leaf { va: DIRECT + 2 * PAGE, pa: RAM_BASE + 2 * PAGE, size: PAGE, flags: RW },
            ],
            protects: 0,
            fail_protect_at: Some(1),
            tlb_flushes: 0,
            cache_flushes: Vec::new(),
        };
        let mut state = MemtypeState::new();
        let pages = [DIRECT, DIRECT + 2 * PAGE];
        assert!(set_pages_array_wc(&mut ctx(&mut state), &mut map, &pages).is_err());
        assert_eq!(state.lookup(RAM, true, RAM_BASE as u64), CacheMode::Wb);
        assert_eq!(map.flags_at(DIRECT), RW);
    }

    #[test]
    fn without_pat_uc_request_still_yields_uncached_leaves() {
        let mut map = Mock::huge(DIRECT, RAM_BASE, 0x20_0000, RW);
        let mut state = MemtypeState::new();
        let mut c = CpaContext {
            state: &mut state,
            ram: RAM,
            pat: false,
            tables: CacheTables::without_pat(),
        };
        set_memory_wc(&mut c, &mut map, DIRECT, 1).unwrap();
        assert_eq!(map.flags_at(DIRECT), RW | MappingFlags::UNCACHED);
    }
}
