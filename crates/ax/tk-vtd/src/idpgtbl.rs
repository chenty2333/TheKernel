//! Page-table domain, identity table and register-based flush paths translated
//! from FreeBSD sys/x86/iommu/intel_idpgtbl.c (snapshot c2b7fe4, BSD-2-Clause).
//! Copyright (c) 2013 The FreeBSD Foundation; Konstantin Belousov under
//! Foundation sponsorship. VM page objects, cache locks and sf_buf mappings are
//! represented by PageMemory/SecondLevel ownership. License is in LICENSES/.
use alloc::vec::Vec;

use crate::{
    Error,
    pgtbl::{PAGE_SIZE, PageMemory, SecondLevel},
    reg::*,
    utils::{self, RegisterIo},
};

const TWO_MIB: u64 = 1 << 21;
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
const ACCESS_BITS: u64 = DMAR_PTE_R | DMAR_PTE_W | DMAR_PTE_SNP | DMAR_PTE_TM;

/// Identity-table cache element: one owning cache reference plus active domains.
/// The caller serializes this object as FreeBSD serialized idpgtbl_lock does.
struct IdMapEntry<M: PageMemory> {
    maximum_address: u64,
    page_levels: u8,
    leaf_level: u8,
    references: usize,
    table: SecondLevel<M>,
}

pub struct IdMapCache<M: PageMemory> {
    entries: Vec<IdMapEntry<M>>,
}
impl<M: PageMemory> Default for IdMapCache<M> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}
impl<M: PageMemory> IdMapCache<M> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn root_physical(&self, index: usize) -> Option<u64> {
        self.entries
            .get(index)
            .map(|entry| entry.table.root_physical())
    }
    pub fn table_mut(&mut self, index: usize) -> Option<&mut SecondLevel<M>> {
        self.entries.get_mut(index).map(|entry| &mut entry.table)
    }
}

/// Build the next identity-map page-table level. SecondLevel's recursive table
/// walk allocates/zeros parent pages and fills the selected 2 MiB leaves.
/// upstream: intel_idpgtbl.c dmar_idmap_nextlvl()
fn dmar_idmap_nextlvl<M: PageMemory>(
    table: &mut SecondLevel<M>,
    maximum_address: u64,
    page_levels: u8,
    leaf_level: u8,
) -> Result<(), Error> {
    if page_levels != 4 || leaf_level != 3 {
        return Err(Error::Unsupported);
    }
    let end = maximum_address
        .checked_add(TWO_MIB - 1)
        .ok_or(Error::InvalidRange)?
        & !(TWO_MIB - 1);
    table.map_identity_2m(end)
}

/// Find or populate a compatible identity mapping; return an index plus root PA.
/// Identity lookup matches upstream by maximum address, page-level support, leaf.
/// upstream: intel_idpgtbl.c dmar_get_idmap_pgtbl()
pub fn dmar_get_idmap_pgtbl<M: PageMemory>(
    cache: &mut IdMapCache<M>,
    mut memory: Option<M>,
    hw_cap: u64,
    page_levels: u8,
    maximum_address: u64,
) -> Result<(usize, u64), Error> {
    if !utils::dmar_pglvl_supported(hw_cap, page_levels) {
        return Err(Error::Unsupported);
    }
    let leaf_level = page_levels - 1; // selected 2 MiB superpage leaf on this implementation
    if let Some(index) = cache.entries.iter().position(|entry| {
        entry.maximum_address >= maximum_address
            && entry.page_levels == page_levels
            && entry.leaf_level == leaf_level
    }) {
        let entry = &mut cache.entries[index];
        entry.references = entry.references.checked_add(1).ok_or(Error::InvalidRange)?;
        return Ok((index, entry.table.root_physical()));
    }
    let mut table = SecondLevel::new(memory.take().ok_or(Error::OutOfMemory)?)?;
    dmar_idmap_nextlvl(&mut table, maximum_address, page_levels, leaf_level)?;
    cache
        .entries
        .try_reserve(1)
        .map_err(|_| Error::OutOfMemory)?;
    let root = table.root_physical();
    cache.entries.push(IdMapEntry {
        maximum_address,
        page_levels,
        leaf_level,
        references: 2, // cache reference plus caller's reference
        table,
    });
    Ok((cache.entries.len() - 1, root))
}

/// Drop the caller's identity-table reference and evict the cache-only object.
/// The pgtbl pages are returned only when the SecondLevel object is dropped.
/// upstream: intel_idpgtbl.c dmar_put_idmap_pgtbl()
pub fn dmar_put_idmap_pgtbl<M: PageMemory>(
    cache: &mut IdMapCache<M>,
    index: usize,
) -> Result<(), Error> {
    let Some(entry) = cache.entries.get_mut(index) else {
        return Err(Error::InvalidRange);
    };
    if entry.references < 2 {
        return Err(Error::InvalidRange);
    }
    entry.references -= 1;
    if entry.references == 1 {
        cache.entries.swap_remove(index);
    }
    Ok(())
}

/// Resolve page-table path and install a run of PTEs. Existing table-page
/// allocation/wiring maps to PageMemory's zeroed page ownership.
/// upstream: intel_idpgtbl.c dmar_pgtbl_map_pte()
pub fn dmar_pgtbl_map_pte<M: PageMemory>(
    table: &mut SecondLevel<M>,
    physical: u64,
    base: u64,
    size: usize,
    pte_flags: u64,
) -> Result<(), Error> {
    if pte_flags & ACCESS_BITS == 0 || pte_flags & !ACCESS_BITS != 0 {
        return Err(Error::InvalidRange);
    }
    table.map_with_flags(physical, base, size, pte_flags)
}

/// Lock-held portion of the upstream buffer mapping. Superpage selection maps
/// to the second-level implementation's large-leaf and page-walk helpers.
/// upstream: intel_idpgtbl.c dmar_map_buf_locked()
pub fn dmar_map_buf_locked<M: PageMemory>(
    table: &mut SecondLevel<M>,
    physical: u64,
    base: u64,
    size: usize,
    pte_flags: u64,
    page_levels: u8,
) -> Result<(), Error> {
    let end = base.checked_add(size as u64).ok_or(Error::InvalidRange)?;
    if size == 0
        || base & (PAGE_SIZE - 1) != 0
        || physical & (PAGE_SIZE - 1) != 0
        || !(size as u64).is_multiple_of(PAGE_SIZE)
        || end <= base
        || page_levels != 4
        || end > (1u64 << 48)
        || base >= (1u64 << 48)
        || pte_flags & (DMAR_PTE_R | DMAR_PTE_W) == 0
        || pte_flags & !ACCESS_BITS != 0
    {
        return Err(Error::InvalidRange);
    }
    let mut mapped = 0usize;
    while mapped < size {
        let result = dmar_pgtbl_map_pte(
            table,
            physical + mapped as u64,
            base + mapped as u64,
            PAGE_SIZE as usize,
            pte_flags,
        );
        if let Err(error) = result {
            if mapped != 0 {
                table.unmap(base, mapped)?;
            }
            return Err(error);
        }
        mapped += PAGE_SIZE as usize;
    }
    Ok(())
}

/// Framework mapping entry to page-table bridge. Access/SNP/TM bits are checked
/// against the discovered unit capabilities by the caller.
/// upstream: intel_idpgtbl.c dmar_map_buf()
pub fn dmar_map_buf<M: PageMemory, I: RegisterIo>(
    table: &mut SecondLevel<M>,
    io: &mut I,
    physical: u64,
    base: u64,
    size: usize,
    pte_flags: u64,
    page_levels: u8,
) -> Result<(), Error> {
    dmar_map_buf_locked(table, physical, base, size, pte_flags, page_levels)?;
    let flush = if io.hw_cap() & DMAR_CAP_CM != 0 {
        dmar_flush_iotlb_sync(io, 1, base, size as u64)
    } else if io.hw_cap() & DMAR_CAP_RWBF != 0 {
        utils::dmar_flush_write_bufs(io)
    } else {
        Ok(())
    };
    if let Err(error) = flush {
        table.unmap(base, size)?;
        return Err(error);
    }
    Ok(())
}

/// Clear one leaf and release the software reference; physical page-table
/// reclamation is deferred until the second-level context is quiesced.
/// upstream: intel_idpgtbl.c dmar_unmap_clear_pte()
pub fn dmar_unmap_clear_pte<M: PageMemory>(
    table: &mut SecondLevel<M>,
    base: u64,
    size: usize,
) -> Result<(), Error> {
    table.unmap(base, size)
}

/// Free an empty PDE on a deferred reclamation list; current page-tree pages
/// remain owned by SecondLevel until domain teardown, after hardware quiescence.
/// upstream: intel_idpgtbl.c dmar_free_pgtbl_pde()
pub fn dmar_free_pgtbl_pde<M: PageMemory>(
    table: &mut SecondLevel<M>,
    base: u64,
    size: usize,
) -> Result<(), Error> {
    dmar_unmap_clear_pte(table, base, size)
}

/// Lock-held full-range unmapping; partial unmaps are rejected by the native API.
/// upstream: intel_idpgtbl.c dmar_unmap_buf_locked()
pub fn dmar_unmap_buf_locked<M: PageMemory>(
    table: &mut SecondLevel<M>,
    base: u64,
    size: usize,
) -> Result<(), Error> {
    if size == 0 {
        return Ok(());
    }
    if base & (PAGE_SIZE - 1) != 0 || !(size as u64).is_multiple_of(PAGE_SIZE) {
        return Err(Error::InvalidRange);
    }
    dmar_free_pgtbl_pde(table, base, size)
}

/// Framework unmap callback; higher-level IOVA/DMA code performs the
/// completion invalidation before releasing backing pages.
/// upstream: intel_idpgtbl.c dmar_unmap_buf()
pub fn dmar_unmap_buf<M: PageMemory>(
    table: &mut SecondLevel<M>,
    base: u64,
    size: usize,
) -> Result<(), Error> {
    dmar_unmap_buf_locked(table, base, size)
}

/// Allocate root page and initialize the domain table metadata.
/// upstream: intel_idpgtbl.c dmar_domain_alloc_pgtbl()
pub fn dmar_domain_alloc_pgtbl<M: PageMemory>(
    memory: M,
    maximum_address_width: u8,
) -> Result<SecondLevel<M>, Error> {
    if maximum_address_width < 39 || maximum_address_width > 48 {
        return Err(Error::Unsupported);
    }
    SecondLevel::new(memory)
}

/// Quiesced-domain page-table destruction. SecondLevel drop releases owned pages.
/// upstream: intel_idpgtbl.c dmar_domain_free_pgtbl()
pub fn dmar_domain_free_pgtbl<M: PageMemory>(table: SecondLevel<M>) {
    drop(table);
}

/// Register-based IOTLB wait: issue IVT|DR|DW and poll IVT clear.
/// upstream: intel_idpgtbl.c dmar_wait_iotlb_flush()
pub fn dmar_wait_iotlb_flush<I: RegisterIo>(
    io: &mut I,
    request: u64,
    iro: u64,
) -> Result<u64, Error> {
    let register = iro + DMAR_IOTLB_REG_OFF;
    io.write64(
        register,
        DMAR_IOTLB_IVT | DMAR_IOTLB_DR | DMAR_IOTLB_DW | request,
    );
    loop {
        let result = io.read64(register);
        if result & DMAR_IOTLB_IVT == 0 {
            return Ok(result);
        }
        io.spin_wait();
    }
}

/// Synchronous domain/page invalidation. Preserve CAP.PSI, 2 MiB cutoff,
/// calc_am chunking, actual-granularity fallback, and read/write draining.
/// upstream: intel_idpgtbl.c dmar_flush_iotlb_sync()
pub fn dmar_flush_iotlb_sync<I: RegisterIo>(
    io: &mut I,
    domain_id: u16,
    mut base: u64,
    mut size: u64,
) -> Result<(), Error> {
    if io.qi_enabled() {
        return Err(Error::InvalidStructure);
    }
    let iro = DMAR_ECAP_IRO(io.hw_ecap()) * 16;
    if io.hw_cap() & DMAR_CAP_PSI == 0 || size > TWO_MIB {
        let result = dmar_wait_iotlb_flush(
            io,
            DMAR_IOTLB_IIRG_DOM | DMAR_IOTLB_DID(domain_id as u64),
            iro,
        )?;
        if result & DMAR_IOTLB_IAIG_MASK == DMAR_IOTLB_IAIG_INVLD {
            return Err(Error::MapFailed);
        }
        return Ok(());
    }
    while size > 0 {
        let (am, isize) = utils::calc_am(io.hw_cap(), base, size);
        if isize == 0 {
            return Err(Error::InvalidRange);
        }
        io.write64(
            iro + DMAR_IVA_REG_OFF,
            DMAR_IVA_ADDR(base) | DMAR_IVA_AM(am as u64),
        );
        let result = dmar_wait_iotlb_flush(
            io,
            DMAR_IOTLB_IIRG_PAGE | DMAR_IOTLB_DID(domain_id as u64),
            iro,
        )?;
        if result & DMAR_IOTLB_IAIG_MASK == DMAR_IOTLB_IAIG_INVLD {
            return Err(Error::MapFailed);
        }
        if result & DMAR_IOTLB_IAIG_MASK != DMAR_IOTLB_IAIG_PAGE {
            break;
        }
        base = base.checked_add(isize).ok_or(Error::InvalidRange)?;
        size -= isize;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, collections::BTreeMap};
    use core::ptr::NonNull;

    use super::*;

    struct FakeMemory {
        next: u64,
        pages: BTreeMap<u64, Box<[u64; 512]>>,
    }
    unsafe impl PageMemory for FakeMemory {
        fn alloc_page(&mut self) -> Result<u64, Error> {
            let address = self.next;
            self.next += PAGE_SIZE;
            self.pages.insert(address, Box::new([0; 512]));
            Ok(address)
        }
        fn page_mut(&mut self, physical: u64) -> Option<NonNull<[u64; 512]>> {
            self.pages
                .get_mut(&physical)
                .map(|page| NonNull::from(page.as_mut()))
        }
        unsafe fn free_page(&mut self, physical: u64) {
            self.pages.remove(&physical);
        }
    }
    fn fake_memory() -> FakeMemory {
        FakeMemory {
            next: 0x1000,
            pages: BTreeMap::new(),
        }
    }

    struct FakeIo {
        regs: [u32; 128],
        cap: u64,
        ecap: u64,
        gcmd: u32,
        qi: bool,
        now: u64,
        writes: Vec<(u64, u64)>,
        flush_granularity: u64,
    }
    impl Default for FakeIo {
        fn default() -> Self {
            Self {
                regs: [0; 128],
                cap: 0,
                ecap: 0,
                gcmd: 0,
                qi: false,
                now: 0,
                writes: Vec::new(),
                flush_granularity: DMAR_IOTLB_IAIG_PAGE,
            }
        }
    }
    impl RegisterIo for FakeIo {
        fn read32(&mut self, o: u64) -> u32 {
            self.regs[o as usize / 4]
        }
        fn write32(&mut self, o: u64, v: u32) {
            self.regs[o as usize / 4] = v;
        }
        fn read64(&mut self, o: u64) -> u64 {
            u64::from(self.regs[o as usize / 4]) | u64::from(self.regs[o as usize / 4 + 1]) << 32
        }
        fn write64(&mut self, o: u64, v: u64) {
            self.writes.push((o, v));
            let actual = if o == 16 + DMAR_IOTLB_REG_OFF {
                self.flush_granularity
            } else {
                0
            };
            let value = (v & !(DMAR_IOTLB_IVT | DMAR_IOTLB_IAIG_MASK)) | actual;
            self.regs[o as usize / 4] = value as u32;
            self.regs[o as usize / 4 + 1] = (value >> 32) as u32;
        }
        fn hw_cap(&self) -> u64 {
            self.cap
        }
        fn hw_ecap(&self) -> u64 {
            self.ecap
        }
        fn hw_gcmd(&self) -> u32 {
            self.gcmd
        }
        fn set_hw_gcmd(&mut self, v: u32) {
            self.gcmd = v;
        }
        fn qi_enabled(&self) -> bool {
            self.qi
        }
        fn root_table_physical(&self) -> u64 {
            0
        }
        fn interrupt_table_physical(&self) -> u64 {
            0
        }
        fn interrupt_entry_count(&self) -> u32 {
            0
        }
        fn x2apic_mode(&self) -> bool {
            false
        }
        fn now_ns(&mut self) -> u64 {
            self.now += 1;
            self.now
        }
    }

    #[test]
    fn identity_tables_are_shared_while_a_domain_holds_a_reference() {
        let mut cache = IdMapCache::default();
        let (first, root) = dmar_get_idmap_pgtbl(
            &mut cache,
            Some(fake_memory()),
            DMAR_CAP_SAGAW_4LVL << 8 | DMAR_CAP_SPS_2M << 34,
            4,
            1 << 30,
        )
        .unwrap();
        let (second, same_root) = dmar_get_idmap_pgtbl(
            &mut cache,
            None,
            DMAR_CAP_SAGAW_4LVL << 8 | DMAR_CAP_SPS_2M << 34,
            4,
            1 << 29,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(root, same_root);
        assert_eq!(cache.len(), 1);
        dmar_put_idmap_pgtbl(&mut cache, first).unwrap();
        assert_eq!(cache.len(), 1);
        dmar_put_idmap_pgtbl(&mut cache, second).unwrap();
        assert!(cache.is_empty());
    }

    #[test]
    fn map_domain_respects_access_permissions_and_cleans_leaf() {
        let mut table = dmar_domain_alloc_pgtbl(fake_memory(), 48).unwrap();
        dmar_map_buf_locked(&mut table, 0x9000, 0x4000, 4096, DMAR_PTE_R, 4).unwrap();
        assert_eq!(table.translate(0x4000), Some(0x9000));
        dmar_unmap_buf(&mut table, 0x4000, 4096).unwrap();
        assert_eq!(table.translate(0x4000), None);
        assert_eq!(
            dmar_map_buf_locked(&mut table, 0x9000, 0x8000, 4096, 0, 4),
            Err(Error::InvalidRange)
        );
        dmar_pgtbl_map_pte(&mut table, 0xa000, 0x6000, 4096, DMAR_PTE_R).unwrap();
        assert_eq!(
            dmar_map_buf_locked(&mut table, 0xb000, 0x5000, 8192, DMAR_PTE_R, 4),
            Err(Error::MapFailed)
        );
        assert_eq!(table.translate(0x5000), None);
        assert_eq!(table.translate(0x6000), Some(0xa000));
    }

    #[test]
    fn iotlb_sync_uses_page_selective_registers_and_capability_fallback() {
        let mut io = FakeIo {
            ecap: 1 << 8,
            cap: DMAR_CAP_PSI | (9 << 48),
            ..Default::default()
        };
        assert_eq!(dmar_flush_iotlb_sync(&mut io, 7, 0x2000, 0x1000), Ok(()));
        assert_eq!(io.writes[0], (16, 0x2000));
        assert_eq!(io.writes[1].0, 24);
        io.writes.clear();
        io.cap = 0;
        assert_eq!(dmar_flush_iotlb_sync(&mut io, 7, 0x2000, 4096), Ok(()));
        assert_eq!(io.writes.len(), 1);
        assert_eq!(io.writes[0].1 & DMAR_IOTLB_IIRG_MASK, DMAR_IOTLB_IIRG_DOM);
    }
}
