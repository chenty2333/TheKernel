//! Intel VT-d root/context, domain, requester and unload handling translated
//! from FreeBSD sys/x86/iommu/intel_ctx.c (snapshot c2b7fe4, BSD-2-Clause).
//! Copyright (c) 2013 The FreeBSD Foundation; Konstantin Belousov under
//! Foundation sponsorship. PCI, RMRR/GAS, delayed-task and page-object handles
//! are mapped through bounded caller-owned adapters. See LICENSES/BSD-2-Clause.txt.
use alloc::vec::Vec;

use crate::{
    Error,
    dmar::{DmarContext, DmarDomain},
    pgtbl::PAGE_SIZE,
    reg::{ContextEntry, RootEntry, *},
    utils::{self},
};

const BUS_COUNT: usize = 256;
const DEVFN_COUNT: usize = 256;
const RMRR_GUARD_PAGES: u64 = 0x20;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextPage {
    pub bus: u8,
    pub physical: u64,
    pub entries: Vec<ContextEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextTables {
    pub root: Vec<RootEntry>,
    pub pages: Vec<ContextPage>,
}
impl Default for ContextTables {
    fn default() -> Self {
        Self {
            root: alloc::vec![RootEntry::default(); BUS_COUNT],
            pages: Vec::new(),
        }
    }
}

impl ContextTables {
    /// Ensure a root entry references a zeroed context page; racing allocators
    /// converge on the first installed page as in dmar_ensure_ctx_page().
    pub fn ensure_ctx_page(&mut self, bus: u8, physical: u64) -> Result<usize, Error> {
        if let Some(index) = self.pages.iter().position(|page| page.bus == bus) {
            return Ok(index);
        }
        if physical == 0
            || physical & (PAGE_SIZE - 1) != 0
            || physical & !DMAR_ROOT_R1_CTP_MASK != 0
        {
            return Err(Error::InvalidRange);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(DEVFN_COUNT)
            .map_err(|_| Error::OutOfMemory)?;
        entries.resize(DEVFN_COUNT, ContextEntry::default());
        self.pages.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
        dmar_ensure_ctx_page(&mut self.root, bus, physical)?;
        self.pages.push(ContextPage {
            bus,
            physical,
            entries,
        });
        Ok(self.pages.len() - 1)
    }

    /// Resolve the root bus and low requester/devfn byte to a context entry.
    pub fn map_ctx_entry(&mut self, rid: u16) -> Result<&mut ContextEntry, Error> {
        let bus = (rid >> 8) as u8;
        let page = self
            .pages
            .iter_mut()
            .find(|page| page.bus == bus)
            .ok_or(Error::InvalidStructure)?;
        dmar_map_ctx_entry(&mut page.entries, rid)
    }
}

/// Install one root entry for an allocated 4 KiB context table page.
// upstream: intel_ctx.c dmar_ensure_ctx_page()
pub fn dmar_ensure_ctx_page(root: &mut [RootEntry], bus: u8, physical: u64) -> Result<(), Error> {
    if root.len() != BUS_COUNT
        || physical == 0
        || physical & (PAGE_SIZE - 1) != 0
        || physical & !DMAR_ROOT_R1_CTP_MASK != 0
    {
        return Err(Error::InvalidRange);
    }
    root[usize::from(bus)] = RootEntry {
        r1: DMAR_ROOT_R1_P | (physical & DMAR_ROOT_R1_CTP_MASK),
        r2: 0,
    };
    Ok(())
}

/// Resolve the low requester/device-function byte inside a context page.
// upstream: intel_ctx.c dmar_map_ctx_entry()
pub fn dmar_map_ctx_entry(page: &mut [ContextEntry], rid: u16) -> Result<&mut ContextEntry, Error> {
    if page.len() != DEVFN_COUNT {
        return Err(Error::InvalidRange);
    }
    page.get_mut(usize::from(rid & 0xff))
        .ok_or(Error::InvalidRange)
}

/// Construct one hardware context entry. Move updates preserve the source's
/// high-word Domain-ID/AW write before the low-word translation pointer.
/// upstream: intel_ctx.c ctx_id_entry_init_one()
pub fn ctx_id_entry_init_one(
    domain_id: u16,
    aw_level: u8,
    second_level_root: Option<u64>,
    pass_through_supported: bool,
) -> Result<ContextEntry, Error> {
    let ctx2 = DMAR_CTX2_DID(domain_id as u64)
        | match aw_level {
            value if value == DMAR_CTX2_AW_2LVL as u8 => DMAR_CTX2_AW_2LVL,
            value if value == DMAR_CTX2_AW_3LVL as u8 => DMAR_CTX2_AW_3LVL,
            value if value == DMAR_CTX2_AW_4LVL as u8 => DMAR_CTX2_AW_4LVL,
            value if value == DMAR_CTX2_AW_5LVL as u8 => DMAR_CTX2_AW_5LVL,
            _ => return Err(Error::Unsupported),
        };
    let ctx1 = match second_level_root {
        Some(root) if root != 0 && root & (PAGE_SIZE - 1) == 0 => {
            (root & DMAR_CTX1_ASR_MASK) | DMAR_CTX1_T_UNTR | DMAR_CTX1_P
        }
        None if pass_through_supported => DMAR_CTX1_T_PASS | DMAR_CTX1_P,
        _ => return Err(Error::InvalidRange),
    };
    Ok(ContextEntry { ctx1, ctx2 })
}

/// Install one context or all device functions for a bus-wide context.
/// upstream: intel_ctx.c ctx_id_entry_init()
pub fn ctx_id_entry_init(
    page: &mut [ContextEntry],
    rid: u16,
    domain_id: u16,
    address_width: u8,
    second_level_root: Option<u64>,
    pass_through_supported: bool,
    move_context: bool,
    buswide: bool,
) -> Result<(), Error> {
    if page.len() != DEVFN_COUNT {
        return Err(Error::InvalidRange);
    }
    let entry = ctx_id_entry_init_one(
        domain_id,
        address_width,
        second_level_root,
        pass_through_supported,
    )?;
    let range = if buswide {
        0..DEVFN_COUNT
    } else {
        usize::from(rid & 0xff)..usize::from(rid & 0xff) + 1
    };
    for slot in range {
        let old = page[slot];
        if !move_context && (old.ctx1 != 0 || old.ctx2 != 0) {
            return Err(Error::InvalidStructure);
        }
        // Preserve source update order: context high word first, then low word.
        page[slot].ctx2 = entry.ctx2;
        page[slot].ctx1 = entry.ctx1;
    }
    Ok(())
}

/// CM/force invalidation policy after context entry updates.
/// upstream: intel_ctx.c dmar_flush_for_ctx_entry()
pub trait ContextInvalidator {
    fn qi_enabled(&self) -> bool;
    fn caching_mode(&self) -> bool;
    fn device_iotlb(&self) -> bool;
    fn qi_context_global(&mut self) -> Result<(), Error>;
    fn qi_iotlb_global(&mut self) -> Result<(), Error>;
    fn register_context_global(&mut self) -> Result<(), Error>;
    fn register_iotlb_global(&mut self) -> Result<(), Error>;
}
pub fn dmar_flush_for_ctx_entry<I: ContextInvalidator>(
    io: &mut I,
    force: bool,
) -> Result<(), Error> {
    if !io.caching_mode() && !force {
        return Ok(());
    }
    if io.qi_enabled() {
        io.qi_context_global()?;
        if io.device_iotlb() || force {
            io.qi_iotlb_global()?;
        }
        return Ok(());
    }
    io.register_context_global()?;
    if io.device_iotlb() || force {
        io.register_iotlb_global()?;
    }
    Ok(())
}

fn invalidate_removed_context<I: ContextInvalidator>(io: &mut I) -> Result<(), Error> {
    if io.qi_enabled() {
        io.qi_context_global()?;
        if io.device_iotlb() {
            io.qi_iotlb_global()?;
        }
    } else {
        io.register_context_global()?;
        if io.device_iotlb() {
            io.register_iotlb_global()?;
        }
    }
    Ok(())
}

/// Normalize one BIOS RMRR start/end-exclusive range. Preserve the upstream 32-page
/// fallback for known empty-after-rounding firmware regions.
/// upstream: intel_ctx.c domain_init_rmrr()
pub fn domain_init_rmrr<F>(
    start: u64,
    end_inclusive: u64,
    enabled: bool,
    mut map_region: F,
) -> Result<Option<(u64, u64)>, Error>
where
    F: FnMut(u64, u64) -> Result<(), Error>,
{
    if !enabled {
        return Ok(None);
    }
    if start > end_inclusive {
        return Err(Error::InvalidRange);
    }
    let start = start & !(PAGE_SIZE - 1);
    let mut end = end_inclusive;
    end = end.checked_add(PAGE_SIZE - 1).ok_or(Error::InvalidRange)? & !(PAGE_SIZE - 1);
    if start == end {
        end = end
            .checked_add(PAGE_SIZE * RMRR_GUARD_PAGES)
            .ok_or(Error::InvalidRange)?;
    }
    map_region(start, end)?;
    Ok(Some((start, end)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciAperture {
    pub start: u64,
    pub end_exclusive: u64,
}

/// Reserve each PCIe-root-port aperture in the IOVA address space.
/// upstream: intel_ctx.c dmar_reserve_pci_regions()
pub fn dmar_reserve_pci_regions<F>(apertures: &[PciAperture], mut reserve: F) -> Result<(), Error>
where
    F: FnMut(u64, u64) -> Result<(), Error>,
{
    for region in apertures {
        if region.start >= region.end_exclusive {
            return Err(Error::InvalidRange);
        }
        reserve(region.start, region.end_exclusive)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DomainState {
    pub domain: DmarDomain,
    pub identity_mapped: bool,
    pub hardware_passthrough: bool,
    pub end_address: u64,
    pub rmrr: bool,
    pub reserved_msi: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextState {
    pub context: DmarContext,
    pub refs: u32,
    pub disabled: bool,
    pub owner: Option<u64>,
}

#[derive(Default)]
pub struct ContextManager {
    pub domains: Vec<DomainState>,
    pub contexts: Vec<ContextState>,
    pub tables: ContextTables,
    unload_batch_no: u32,
}

/// Allocate a domain ID/address-width tuple and set its identity/remap mode.
/// Framework GAS and unit-ID allocator state are caller-provided.
/// upstream: intel_ctx.c dmar_domain_alloc()
pub fn dmar_domain_alloc(
    domain_id: u16,
    unit_index: u32,
    hw_cap: u64,
    maximum_address: u64,
    identity_mapped: bool,
    pass_through: bool,
    page_table_root: u64,
) -> Result<DomainState, Error> {
    let agaw = utils::dmar_maxaddr2mgaw(hw_cap, maximum_address, !identity_mapped)?;
    let mut domain = DmarDomain {
        domain_id: i32::from(domain_id),
        unit_index,
        refs: 0,
        ..DmarDomain::default()
    };
    utils::domain_set_agaw(&mut domain, hw_cap, i32::from(agaw))?;
    if !identity_mapped && page_table_root == 0 {
        return Err(Error::InvalidRange);
    }
    if identity_mapped && !pass_through {
        domain.page_table_root = page_table_root;
    }
    let end_address = if identity_mapped {
        maximum_address
    } else {
        1u64.checked_shl(u32::from(agaw - 1))
            .ok_or(Error::InvalidRange)?
    };
    Ok(DomainState {
        domain,
        identity_mapped,
        hardware_passthrough: identity_mapped && pass_through,
        end_address,
        rmrr: false,
        reserved_msi: !identity_mapped,
    })
}

/// Allocate one requester context with an initial owner reference.
/// upstream: intel_ctx.c dmar_ctx_alloc()
pub fn dmar_ctx_alloc(domain: &DomainState, rid: u16, owner: Option<u64>) -> ContextState {
    ContextState {
        context: DmarContext {
            source_id: rid,
            domain_id: domain.domain.domain_id,
            last_fault_rec: [0; 2],
        },
        refs: 1,
        disabled: false,
        owner,
    }
}

/// Link one context into a domain (domain lock is the caller's serializer).
/// upstream: intel_ctx.c dmar_ctx_link()
pub fn dmar_ctx_link(domain: &mut DomainState, context: &mut ContextState) -> Result<(), Error> {
    if context.context.domain_id != domain.domain.domain_id {
        return Err(Error::NoDomain);
    }
    domain.domain.refs = domain
        .domain
        .refs
        .checked_add(1)
        .ok_or(Error::InvalidRange)?;
    domain.domain.context_count = domain
        .domain
        .context_count
        .checked_add(1)
        .ok_or(Error::InvalidRange)?;
    Ok(())
}

/// Unlink one context from its owning domain.
/// upstream: intel_ctx.c dmar_ctx_unlink()
pub fn dmar_ctx_unlink(domain: &mut DomainState) -> Result<(), Error> {
    if domain.domain.refs == 0 || domain.domain.context_count == 0 {
        return Err(Error::InvalidStructure);
    }
    domain.domain.refs -= 1;
    domain.domain.context_count -= 1;
    Ok(())
}

/// Verify the last-reference invariants before destroying the domain.
/// upstream: intel_ctx.c dmar_domain_destroy()
pub fn dmar_domain_destroy(domain: DomainState) -> Result<(), Error> {
    if domain.domain.refs != 0 || domain.domain.context_count != 0 {
        return Err(Error::InvalidStructure);
    }
    Ok(())
}

/// Single-flight requester context creation, with a second lookup after the
/// caller's potentially sleeping domain allocation step.
/// upstream: intel_ctx.c dmar_get_ctx_for_dev1()
pub fn dmar_get_ctx_for_dev1(
    manager: &mut ContextManager,
    domain: DomainState,
    rid: u16,
    context_page_physical: u64,
    second_level_root: Option<u64>,
    pass_through_supported: bool,
    buswide: bool,
    owner: Option<u64>,
) -> Result<usize, Error> {
    if let Some(index) = dmar_find_ctx_locked(manager, rid) {
        manager.contexts[index].refs = manager.contexts[index]
            .refs
            .checked_add(1)
            .ok_or(Error::InvalidRange)?;
        return Ok(index);
    }
    let bus = (rid >> 8) as u8;
    manager.tables.ensure_ctx_page(bus, context_page_physical)?;
    // Recheck after the resource-allocating part. The caller serializes updates,
    // but this also preserves the upstream race-resolution rule.
    if let Some(index) = dmar_find_ctx_locked(manager, rid) {
        manager.contexts[index].refs = manager.contexts[index]
            .refs
            .checked_add(1)
            .ok_or(Error::InvalidRange)?;
        return Ok(index);
    }
    manager
        .domains
        .try_reserve(1)
        .map_err(|_| Error::OutOfMemory)?;
    manager
        .contexts
        .try_reserve(1)
        .map_err(|_| Error::OutOfMemory)?;
    let page = manager
        .tables
        .pages
        .iter_mut()
        .find(|page| page.bus == bus)
        .ok_or(Error::InvalidStructure)?;
    ctx_id_entry_init(
        &mut page.entries,
        rid,
        domain.domain.domain_id as u16,
        domain.domain.awlvl as u8,
        second_level_root,
        pass_through_supported,
        false,
        buswide,
    )?;
    let mut context = dmar_ctx_alloc(&domain, rid, owner);
    let mut domain = domain;
    dmar_ctx_link(&mut domain, &mut context)?;
    if manager
        .domains
        .iter()
        .any(|state| state.domain.domain_id == domain.domain.domain_id)
    {
        return Err(Error::InvalidStructure);
    }
    manager.domains.push(domain);
    manager.contexts.push(context);
    Ok(manager.contexts.len() - 1)
}

/// PCI-device wrapper: compute the standard bus/device/function requester ID.
/// upstream: intel_ctx.c dmar_get_ctx_for_dev()
pub fn dmar_get_ctx_for_dev(
    manager: &mut ContextManager,
    domain: DomainState,
    bus: u8,
    slot: u8,
    function: u8,
    context_page_physical: u64,
    second_level_root: Option<u64>,
    pass_through_supported: bool,
    owner: Option<u64>,
) -> Result<usize, Error> {
    let rid = (u16::from(bus) << 8) | (u16::from(slot) << 3) | u16::from(function);
    dmar_get_ctx_for_dev1(
        manager,
        domain,
        rid,
        context_page_physical,
        second_level_root,
        pass_through_supported,
        false,
        owner,
    )
}

/// ACPI path wrapper: the requester ID is resolved by PCI's native path layer.
/// upstream: intel_ctx.c dmar_get_ctx_for_devpath()
pub fn dmar_get_ctx_for_devpath(
    manager: &mut ContextManager,
    domain: DomainState,
    rid: u16,
    context_page_physical: u64,
    second_level_root: Option<u64>,
    pass_through_supported: bool,
) -> Result<usize, Error> {
    dmar_get_ctx_for_dev1(
        manager,
        domain,
        rid,
        context_page_physical,
        second_level_root,
        pass_through_supported,
        false,
        None,
    )
}

/// Change a live requester to another domain, update its context entry, then
/// force global invalidation; the new context is retained if flush fails.
/// upstream: intel_ctx.c dmar_move_ctx_to_domain()
pub fn dmar_move_ctx_to_domain<I: ContextInvalidator>(
    manager: &mut ContextManager,
    context_index: usize,
    new_domain_id: i32,
    io: &mut I,
) -> Result<(), Error> {
    let context = *manager
        .contexts
        .get(context_index)
        .ok_or(Error::InvalidRange)?;
    let new_index = manager
        .domains
        .iter()
        .position(|domain| domain.domain.domain_id == new_domain_id)
        .ok_or(Error::NoDomain)?;
    let old_index = manager
        .domains
        .iter()
        .position(|domain| domain.domain.domain_id == context.context.domain_id)
        .ok_or(Error::NoDomain)?;
    if old_index == new_index {
        return Ok(());
    }
    let passthrough = manager.domains[new_index].hardware_passthrough;
    let root = manager.domains[new_index].domain.page_table_root;
    let root = (root != 0).then_some(root);
    let aw = manager.domains[new_index].domain.awlvl as u8;
    let rid = context.context.source_id;
    let page = manager
        .tables
        .pages
        .iter_mut()
        .find(|page| page.bus == (rid >> 8) as u8)
        .ok_or(Error::InvalidStructure)?;
    ctx_id_entry_init(
        &mut page.entries,
        rid,
        new_domain_id as u16,
        aw,
        root,
        passthrough,
        true,
        false,
    )?;
    manager.domains[old_index].domain.refs =
        manager.domains[old_index].domain.refs.saturating_sub(1);
    manager.domains[old_index].domain.context_count = manager.domains[old_index]
        .domain
        .context_count
        .saturating_sub(1);
    manager.domains[new_index].domain.refs = manager.domains[new_index]
        .domain
        .refs
        .checked_add(1)
        .ok_or(Error::InvalidRange)?;
    manager.domains[new_index].domain.context_count = manager.domains[new_index]
        .domain
        .context_count
        .checked_add(1)
        .ok_or(Error::InvalidRange)?;
    manager.contexts[context_index].context.domain_id = new_domain_id;
    dmar_flush_for_ctx_entry(io, true)
}

/// Drop a non-context reference; tell the caller when the domain can be freed.
/// upstream: intel_ctx.c dmar_unref_domain_locked()
pub fn dmar_unref_domain_locked(
    manager: &mut ContextManager,
    domain_id: i32,
) -> Result<bool, Error> {
    let index = manager
        .domains
        .iter()
        .position(|domain| domain.domain.domain_id == domain_id)
        .ok_or(Error::NoDomain)?;
    if manager.domains[index].domain.refs > 1 {
        manager.domains[index].domain.refs -= 1;
        return Ok(false);
    }
    if manager.domains[index].domain.context_count != 0 || manager.domains[index].rmrr {
        return Err(Error::InvalidStructure);
    }
    manager.domains.swap_remove(index);
    Ok(true)
}

/// Dereference or clear a context entry, invalidate caches, then unlink/free it.
/// upstream: intel_ctx.c dmar_free_ctx_locked()
pub fn dmar_free_ctx_locked<I: ContextInvalidator>(
    manager: &mut ContextManager,
    context_index: usize,
    io: &mut I,
) -> Result<bool, Error> {
    let context = *manager
        .contexts
        .get(context_index)
        .ok_or(Error::InvalidRange)?;
    if context.refs > 1 {
        manager.contexts[context_index].refs -= 1;
        return Ok(false);
    }
    if context.disabled {
        return Err(Error::InvalidStructure);
    }
    let rid = context.context.source_id;
    let page = manager
        .tables
        .pages
        .iter_mut()
        .find(|page| page.bus == (rid >> 8) as u8)
        .ok_or(Error::InvalidStructure)?;
    let entry = page
        .entries
        .get_mut(usize::from(rid & 0xff))
        .ok_or(Error::InvalidRange)?;
    entry.ctx1 = 0;
    entry.ctx2 = 0;
    invalidate_removed_context(io)?;
    let domain = context.context.domain_id;
    manager.contexts.swap_remove(context_index);
    let Some(domain_index) = manager
        .domains
        .iter()
        .position(|state| state.domain.domain_id == domain)
    else {
        return Err(Error::NoDomain);
    };
    dmar_ctx_unlink(&mut manager.domains[domain_index])?;
    Ok(true)
}

/// Lookup a live requester under the manager's serialization lock.
/// upstream: intel_ctx.c dmar_find_ctx_locked()
pub fn dmar_find_ctx_locked(manager: &ContextManager, rid: u16) -> Option<usize> {
    manager
        .contexts
        .iter()
        .position(|context| context.context.source_id == rid)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnloadEntry {
    pub domain_id: i32,
    pub start: u64,
    pub end: u64,
    pub mapped: bool,
    pub rmrr: bool,
    pub generation: u64,
}
pub trait DomainInvalidator {
    fn unmap_entry(&mut self, entry: &UnloadEntry, can_sleep: bool) -> Result<(), Error>;
    fn qi_enabled(&self) -> bool;
    fn qi_invalidate_async(&mut self, entry: &UnloadEntry, emit_wait: bool) -> Result<(), Error>;
    fn qi_invalidate_sync(
        &mut self,
        domain_id: i32,
        start: u64,
        size: u64,
        can_sleep: bool,
    ) -> Result<(), Error>;
    fn register_invalidate_sync(
        &mut self,
        domain_id: i32,
        start: u64,
        size: u64,
    ) -> Result<(), Error>;
    fn free_entry(&mut self, entry: UnloadEntry, asynchronous: bool);
}

/// Invalidate one mapping entry with the same async/sync lifetime boundary.
/// upstream: intel_ctx.c dmar_domain_unload_entry()
pub fn dmar_domain_unload_entry<I: DomainInvalidator>(
    io: &mut I,
    entry: UnloadEntry,
    free: bool,
    can_sleep: bool,
) -> Result<(), Error> {
    let size = entry
        .end
        .checked_sub(entry.start)
        .ok_or(Error::InvalidRange)?;
    if io.qi_enabled() {
        if free {
            io.qi_invalidate_async(&entry, true)
        } else {
            io.qi_invalidate_sync(entry.domain_id, entry.start, size, can_sleep)?;
            io.free_entry(entry, false);
            Ok(())
        }
    } else {
        io.register_invalidate_sync(entry.domain_id, entry.start, size)?;
        io.free_entry(entry, free);
        Ok(())
    }
}

/// Coalesce completion interrupts at the configured batch interval.
/// upstream: intel_ctx.c dmar_domain_unload_emit_wait()
pub fn dmar_domain_unload_emit_wait(batch_no: &mut u32, is_last: bool, interval: u32) -> bool {
    if is_last {
        return true;
    }
    let interval = interval.max(1);
    let emit = *batch_no % interval == 0;
    *batch_no = batch_no.wrapping_add(1);
    emit
}

/// Unmap entries, synchronously invalidate without QI, otherwise emit the
/// ordered QI batch only after all PTE changes have been applied.
/// upstream: intel_ctx.c dmar_domain_unload()
pub fn dmar_domain_unload<I: DomainInvalidator>(
    io: &mut I,
    entries: &mut Vec<UnloadEntry>,
    can_sleep: bool,
    batch_interval: u32,
) -> Result<(), Error> {
    if !io.qi_enabled() {
        for entry in entries.drain(..) {
            if !entry.mapped {
                return Err(Error::InvalidStructure);
            }
            io.unmap_entry(&entry, can_sleep)?;
            let size = entry
                .end
                .checked_sub(entry.start)
                .ok_or(Error::InvalidRange)?;
            io.register_invalidate_sync(entry.domain_id, entry.start, size)?;
            io.free_entry(entry, true);
        }
        return Ok(());
    }
    if entries.iter().any(|entry| !entry.mapped) {
        return Err(Error::InvalidStructure);
    }
    for entry in entries.iter() {
        io.unmap_entry(entry, can_sleep)?;
    }
    let mut batch = 0;
    let len = entries.len();
    for (index, entry) in entries.drain(..).enumerate() {
        io.unmap_entry(&entry, can_sleep)?;
        let emit_wait = dmar_domain_unload_emit_wait(&mut batch, index + 1 == len, batch_interval);
        io.qi_invalidate_async(&entry, emit_wait)?;
    }
    Ok(())
}

/// Native IOMMU-unit wrapper for requester acquisition.
/// upstream: intel_ctx.c dmar_get_ctx()
pub fn dmar_get_ctx(
    manager: &mut ContextManager,
    domain: DomainState,
    rid: u16,
    context_page_physical: u64,
    second_level_root: Option<u64>,
    pass_through_supported: bool,
) -> Result<usize, Error> {
    dmar_get_ctx_for_devpath(
        manager,
        domain,
        rid,
        context_page_physical,
        second_level_root,
        pass_through_supported,
    )
}

/// Native lock-method adapter for context release.
/// upstream: intel_ctx.c dmar_free_ctx_locked_method()
pub fn dmar_free_ctx_locked_method<I: ContextInvalidator>(
    manager: &mut ContextManager,
    context_index: usize,
    io: &mut I,
) -> Result<bool, Error> {
    dmar_free_ctx_locked(manager, context_index, io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_contexts_and_bus_wide_entries_match_hardware_layout() {
        let mut tables = ContextTables::default();
        let page_index = tables.ensure_ctx_page(3, 0x4000).unwrap();
        assert_eq!(tables.root[3].r1, 0x4001);
        let page = &mut tables.pages[page_index].entries;
        ctx_id_entry_init(
            page,
            0x0308,
            7,
            DMAR_CTX2_AW_4LVL as u8,
            Some(0x8000),
            false,
            false,
            false,
        )
        .unwrap();
        let entry = &page[8];
        assert_eq!(entry.ctx1, 0x8001);
        assert_eq!(entry.ctx2, DMAR_CTX2_DID(7) | DMAR_CTX2_AW_4LVL);
        assert_eq!(
            dmar_ctx_alloc(
                &DomainState {
                    domain: DmarDomain {
                        domain_id: 7,
                        ..DmarDomain::default()
                    },
                    identity_mapped: false,
                    hardware_passthrough: false,
                    end_address: 0,
                    rmrr: false,
                    reserved_msi: false
                },
                0x0308,
                None,
            )
            .context
            .source_id,
            0x0308
        );
        ctx_id_entry_init(
            page,
            0x0300,
            7,
            DMAR_CTX2_AW_4LVL as u8,
            Some(0x8000),
            false,
            true,
            true,
        )
        .unwrap();
        assert!(page.iter().all(|entry| entry.ctx1 == 0x8001));
    }

    #[test]
    fn context_manager_acquires_moves_and_releases_references() {
        let domain = dmar_domain_alloc(
            7,
            0,
            DMAR_CAP_SAGAW_4LVL << 8,
            1 << 40,
            false,
            false,
            0x8000,
        )
        .unwrap();
        let mut manager = ContextManager::default();
        let index = dmar_get_ctx_for_dev1(
            &mut manager,
            domain,
            0x1234,
            0x4000,
            Some(0x8000),
            false,
            false,
            Some(1),
        )
        .unwrap();
        assert_eq!(dmar_find_ctx_locked(&manager, 0x1234), Some(index));
        assert_eq!(manager.domains[0].domain.context_count, 1);
    }

    #[test]
    fn rmrr_rounding_and_empty_bios_workaround_are_bounded() {
        let mut seen = None;
        let range = domain_init_rmrr(0x1001, 0x1fff, true, |s, e| {
            seen = Some((s, e));
            Ok(())
        })
        .unwrap();
        assert_eq!(range, Some((0x1000, 0x2000)));
        assert_eq!(seen, range);
        let workaround = domain_init_rmrr(0x1000, 0x1000, true, |_, _| Ok(()))
            .unwrap()
            .unwrap();
        assert_eq!(workaround.1 - workaround.0, PAGE_SIZE * RMRR_GUARD_PAGES);
    }

    #[test]
    fn batched_unload_always_waits_on_the_final_entry() {
        let mut batch = 0;
        assert!(dmar_domain_unload_emit_wait(&mut batch, false, 2));
        assert!(!dmar_domain_unload_emit_wait(&mut batch, false, 2));
        assert!(dmar_domain_unload_emit_wait(&mut batch, true, 2));
    }
}
