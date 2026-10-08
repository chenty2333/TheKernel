//! Opt-in VT-d setup and a shared DMA domain. Legacy PCI drivers see explicit
//! identity entries; virtio/NVMe use disjoint translated IOVAs when TE is enabled.
use alloc::vec::Vec;
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU8, Ordering},
};

use axalloc::{UsageKind, global_allocator};
use axhal::mem::{PhysAddr, phys_ram_ranges, phys_to_virt, virt_to_phys};
use kspin::SpinNoIrq;
use tk_acpica::Engine;
use tk_vtd::{
    DmarTable, Error,
    context::{ctx_id_entry_init, dmar_ensure_ctx_page},
    idpgtbl::{dmar_map_buf_locked, dmar_unmap_buf_locked},
    iova::IovaAllocator,
    pgtbl::{PAGE_SIZE, PageMemory, SecondLevel},
    qi::{self, QiIo, QiQueue},
    reg::{
        ContextEntry, DMAR_CAP_MGAW, DMAR_CAP_RWBF, DMAR_CAP_SAGAW, DMAR_CAP_SAGAW_4LVL,
        DMAR_CAP_SPS, DMAR_CAP_SPS_2M, DMAR_CTX2_AW_4LVL, DMAR_ECAP_QI, DMAR_IECTL_IM,
        DMAR_IECTL_REG, DMAR_PTE_R, DMAR_PTE_W, RootEntry,
    },
    utils::{self, RegisterIo},
};

const MODE_UNKNOWN: u8 = 0;
const MODE_IDENTITY: u8 = 1;
const MODE_ENABLED: u8 = 2;
const MODE_FAILED: u8 = 3;
const MMIO_BYTES: usize = 0x1000;
const CAP: usize = 0x08;
const ECAP: usize = 0x10;
const GCMD: usize = 0x18;
const GSTS: usize = 0x1c;
const FSTS: usize = 0x34;
const GSTS_TES: u32 = 1 << 31;
const GSTS_QIES: u32 = 1 << 26;
const GSTS_IRES: u32 = 1 << 25;
const FSTS_PPF: u32 = 1 << 1;
const FSTS_PFO: u32 = 1;
const QI_ORDER: u32 = 2;
const QI_BYTES: usize = (1 << QI_ORDER) * PAGE_SIZE as usize;
const QI_PAGES: usize = (1 << QI_ORDER) + 1;
const IDENTITY_PAGE_SIZE: u64 = 1 << 21;
const IOVA_END: u64 = 1 << 36;

static MODE: AtomicU8 = AtomicU8::new(MODE_UNKNOWN);
static MANAGER: SpinNoIrq<Option<Manager>> = SpinNoIrq::new(None);

struct DmaBlock {
    physical: u64,
    virtual_address: NonNull<u8>,
    pages: usize,
}
// SAFETY: ownership is unique and page memory is accessed through the manager lock.
unsafe impl Send for DmaBlock {}

fn allocate_dma(pages: usize) -> Result<DmaBlock, Error> {
    let virtual_address = global_allocator()
        .alloc_pages(pages, PAGE_SIZE as usize, UsageKind::Dma)
        .map_err(|_| Error::OutOfMemory)?;
    let physical = virt_to_phys(virtual_address.into()).as_usize() as u64;
    let Some(virtual_address) = NonNull::new(virtual_address as *mut u8) else {
        global_allocator().dealloc_pages(virtual_address, pages, UsageKind::Dma);
        return Err(Error::OutOfMemory);
    };
    // SAFETY: allocation is uniquely owned and page aligned.
    unsafe { core::ptr::write_bytes(virtual_address.as_ptr(), 0, pages * PAGE_SIZE as usize) };
    Ok(DmaBlock {
        physical,
        virtual_address,
        pages,
    })
}

struct KernelPageMemory {
    pages: Vec<DmaBlock>,
}
// SAFETY: pages are uniquely owned and manager access is serialized; the
// physical-to-virtual mapping is only returned for pages owned by this object.
unsafe impl PageMemory for KernelPageMemory {
    fn alloc_page(&mut self) -> Result<u64, Error> {
        let page = allocate_dma(1)?;
        let physical = page.physical;
        self.pages.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
        self.pages.push(page);
        Ok(physical)
    }
    fn page_mut(&mut self, physical: u64) -> Option<NonNull<[u64; 512]>> {
        if !physical.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let address = phys_to_virt(PhysAddr::from_usize(usize::try_from(physical).ok()?));
        let pointer = address.as_mut_ptr().cast::<[u64; 512]>();
        NonNull::new(pointer)
    }
    unsafe fn free_page(&mut self, physical: u64) {
        if let Some(index) = self.pages.iter().position(|page| page.physical == physical) {
            let page = self.pages.swap_remove(index);
            global_allocator().dealloc_pages(
                page.virtual_address.as_ptr() as usize,
                page.pages,
                UsageKind::Dma,
            );
        }
    }
}

struct Unit {
    mmio: usize,
    qi: DmaBlock,
    queue: QiQueue,
    gcmd: u32,
    root_physical: u64,
}
// SAFETY: MMIO mappings are stable for boot and mutable queue state is accessed
// only under MANAGER's lock.
unsafe impl Send for Unit {}

fn read32(unit: &Unit, offset: usize) -> u32 {
    // SAFETY: unit MMIO mapping spans a page and every caller uses a known dword offset.
    unsafe { ((unit.mmio + offset) as *const u32).read_volatile() }
}
fn write32(unit: &Unit, offset: usize, value: u32) {
    // SAFETY: unit MMIO mapping spans a page and every caller uses a known dword offset.
    unsafe { ((unit.mmio + offset) as *mut u32).write_volatile(value) }
}
fn read64(unit: &Unit, offset: usize) -> u64 {
    // SAFETY: the 64-bit capability/register is naturally aligned and page bounded.
    unsafe { ((unit.mmio + offset) as *const u64).read_volatile() }
}
fn write64(unit: &Unit, offset: usize, value: u64) {
    // SAFETY: the 64-bit capability/register is naturally aligned and page bounded.
    unsafe { ((unit.mmio + offset) as *mut u64).write_volatile(value) }
}

impl Unit {
    fn check_faults(&self) -> Result<(), Error> {
        let status = read32(self, FSTS);
        if status & (FSTS_PPF | FSTS_PFO) == 0 {
            return Ok(());
        }
        if status & FSTS_PPF != 0 {
            let index = ((status >> 8) & 0xff) as usize;
            let record_offset = (((read64(self, CAP) >> 24) & 0x1ff) as usize)
                .checked_mul(16)
                .and_then(|offset| offset.checked_add(index * 16));
            if let Some(offset) = record_offset.filter(|offset| offset + 16 <= MMIO_BYTES) {
                let info = read64(self, offset);
                let address = read64(self, offset + 8);
                error!(
                    "vtd: DMA fault sid={:#06x} reason={:#x} address={address:#x} record={index}",
                    info as u16,
                    (info >> 32) & 0xff
                );
            }
        }
        // W1C only the pending/overflow bits we observed. The fault record is
        // left to firmware diagnostics; the DMA service fails closed.
        write32(self, FSTS, status & (FSTS_PPF | FSTS_PFO));
        Err(Error::MapFailed)
    }

    fn invalidate_all(&mut self) -> Result<(), Error> {
        let mut io = UnitQiIo {
            mmio: self.mmio,
            qi_physical: self.qi.physical,
            qi_virtual: self.qi.virtual_address,
            gcmd: &mut self.gcmd,
        };
        qi::dmar_qi_invalidate_ctx_glob_locked(&mut io, &mut self.queue)?;
        qi::dmar_qi_invalidate_iotlb_glob_locked(&mut io, &mut self.queue)?;
        self.check_faults()
    }

    fn enable(&mut self, root_physical: u64) -> Result<(), Error> {
        let capability = read64(self, CAP);
        let extended = read64(self, ECAP);
        if extended & DMAR_ECAP_QI == 0
            || DMAR_CAP_SAGAW(capability) & DMAR_CAP_SAGAW_4LVL == 0
            || DMAR_CAP_SPS(capability) & DMAR_CAP_SPS_2M == 0
            || DMAR_CAP_MGAW(capability) < 39
        {
            return Err(Error::MapFailed);
        }
        self.gcmd = read32(self, GCMD);
        let status = read32(self, GSTS);
        if status & GSTS_IRES != 0 {
            utils::dmar_disable_ir(self)?;
        }
        if status & GSTS_QIES != 0 {
            let mut io = UnitQiIo {
                mmio: self.mmio,
                qi_physical: self.qi.physical,
                qi_virtual: self.qi.virtual_address,
                gcmd: &mut self.gcmd,
            };
            qi::dmar_disable_qi(&mut io)?;
        }
        if status & GSTS_TES != 0 {
            utils::dmar_disable_translation(self)?;
        }
        self.root_physical = root_physical;
        utils::dmar_load_root_entry_ptr(self)?;
        // Clear state retained by firmware while queued invalidation is off.
        utils::dmar_inv_ctx_glob(self)?;
        utils::dmar_inv_iotlb_glob(self)?;
        if capability & DMAR_CAP_RWBF != 0 {
            let _ = utils::dmar_flush_write_bufs(self);
        }
        self.queue = {
            let mut io = UnitQiIo {
                mmio: self.mmio,
                qi_physical: self.qi.physical,
                qi_virtual: self.qi.virtual_address,
                gcmd: &mut self.gcmd,
            };
            qi::dmar_init_qi(&mut io, QI_ORDER, QI_ORDER)?.ok_or(Error::Unsupported)?
        };
        self.invalidate_all()?;
        utils::dmar_enable_translation(self)?;
        self.check_faults()
    }
}

impl RegisterIo for Unit {
    fn read32(&mut self, offset: u64) -> u32 {
        usize::try_from(offset)
            .ok()
            .map_or(0, |offset| read32(self, offset))
    }
    fn write32(&mut self, offset: u64, value: u32) {
        if let Ok(offset) = usize::try_from(offset) {
            write32(self, offset, value);
        }
    }
    fn read64(&mut self, offset: u64) -> u64 {
        usize::try_from(offset)
            .ok()
            .map_or(0, |offset| read64(self, offset))
    }
    fn write64(&mut self, offset: u64, value: u64) {
        if let Ok(offset) = usize::try_from(offset) {
            write64(self, offset, value);
        }
    }
    fn hw_cap(&self) -> u64 {
        read64(self, CAP)
    }
    fn hw_ecap(&self) -> u64 {
        read64(self, ECAP)
    }
    fn hw_gcmd(&self) -> u32 {
        self.gcmd
    }
    fn set_hw_gcmd(&mut self, value: u32) {
        self.gcmd = value;
    }
    fn qi_enabled(&self) -> bool {
        self.queue.enabled
    }
    fn root_table_physical(&self) -> u64 {
        self.root_physical
    }
    fn interrupt_table_physical(&self) -> u64 {
        0
    }
    fn interrupt_entry_count(&self) -> u32 {
        0
    }
    fn x2apic_mode(&self) -> bool {
        true
    }
    fn now_ns(&mut self) -> u64 {
        axhal::time::monotonic_time_nanos()
    }
}

struct UnitQiIo<'a> {
    mmio: usize,
    qi_physical: u64,
    qi_virtual: NonNull<u8>,
    gcmd: &'a mut u32,
}

impl RegisterIo for UnitQiIo<'_> {
    fn read32(&mut self, offset: u64) -> u32 {
        usize::try_from(offset).ok().map_or(0, |offset| {
            // SAFETY: offsets are the known VT-d QI registers within the mapped MMIO page.
            unsafe { ((self.mmio + offset) as *const u32).read_volatile() }
        })
    }
    fn write32(&mut self, offset: u64, value: u32) {
        if let Ok(offset) = usize::try_from(offset) {
            // SAFETY: offsets are the known VT-d QI registers within the mapped MMIO page.
            unsafe { ((self.mmio + offset) as *mut u32).write_volatile(value) }
        }
    }
    fn read64(&mut self, offset: u64) -> u64 {
        usize::try_from(offset).ok().map_or(0, |offset| {
            // SAFETY: offsets are naturally aligned VT-d registers in the mapped MMIO page.
            unsafe { ((self.mmio + offset) as *const u64).read_volatile() }
        })
    }
    fn write64(&mut self, offset: u64, value: u64) {
        if let Ok(offset) = usize::try_from(offset) {
            // SAFETY: offsets are naturally aligned VT-d registers in the mapped MMIO page.
            unsafe { ((self.mmio + offset) as *mut u64).write_volatile(value) }
        }
    }
    fn hw_cap(&self) -> u64 {
        self.read64_const(CAP)
    }
    fn hw_ecap(&self) -> u64 {
        self.read64_const(ECAP)
    }
    fn hw_gcmd(&self) -> u32 {
        *self.gcmd
    }
    fn set_hw_gcmd(&mut self, value: u32) {
        *self.gcmd = value;
    }
    fn qi_enabled(&self) -> bool {
        true
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
        true
    }
    fn now_ns(&mut self) -> u64 {
        axhal::time::monotonic_time_nanos()
    }
}

impl UnitQiIo<'_> {
    fn read64_const(&self, offset: usize) -> u64 {
        // SAFETY: offsets are naturally aligned capability registers in the mapped MMIO page.
        unsafe { ((self.mmio + offset) as *const u64).read_volatile() }
    }
}

impl QiIo for UnitQiIo<'_> {
    fn qi_queue_physical(&self) -> u64 {
        self.qi_physical
    }
    fn qi_wait_sequence_physical(&self) -> u64 {
        self.qi_physical + QI_BYTES as u64
    }
    fn qi_interrupt_entry_count(&self) -> u32 {
        0
    }
    fn qi_interrupt_enabled(&self) -> bool {
        false
    }
    fn qi_store_descriptor(&mut self, byte_offset: u32, low: u64, high: u64) {
        if byte_offset as usize + 16 > QI_BYTES {
            return;
        }
        // SAFETY: queue has a dedicated 16 KiB aligned ring and descriptors are 16 bytes.
        unsafe {
            let slot = self
                .qi_virtual
                .as_ptr()
                .add(byte_offset as usize)
                .cast::<u64>();
            slot.write_volatile(low);
            slot.add(1).write_volatile(high);
        }
    }
    fn qi_hardware_sequence(&mut self) -> u64 {
        // SAFETY: the final page is reserved for the writeback sequence word.
        let sequence = unsafe {
            self.qi_virtual
                .as_ptr()
                .add(QI_BYTES)
                .cast::<u64>()
                .read_volatile()
        };
        core::sync::atomic::fence(Ordering::Acquire);
        sequence
    }
    fn qi_advance_waiter_count(&mut self, _delta: i32) {}
    fn qi_is_cold(&self) -> bool {
        false
    }
    fn qi_wait_for_progress(&mut self, _nowait: bool) {
        core::hint::spin_loop();
    }
    fn qi_drain_tlb_flushes(&mut self) {}
    fn qi_enqueue_completion_task(&mut self) {}
    fn qi_wake_sequence_waiters(&mut self) {}
    fn qi_common_init(&mut self, queue_bytes: u32, descriptor_bytes: u32) -> Result<(), Error> {
        if queue_bytes as usize != QI_BYTES || descriptor_bytes != 16 {
            return Err(Error::InvalidRange);
        }
        Ok(())
    }
    fn qi_common_fini(&mut self) {}
    fn qi_enable_interrupt(&mut self) {
        // Without an APIC-vector handler, use the completion writeback and keep
        // the interrupt source masked.
        let value = self.read32(DMAR_IECTL_REG) | DMAR_IECTL_IM as u32;
        self.write32(DMAR_IECTL_REG, value);
    }
    fn qi_disable_interrupt(&mut self) {
        let value = self.read32(DMAR_IECTL_REG) | DMAR_IECTL_IM as u32;
        self.write32(DMAR_IECTL_REG, value);
    }
    fn qi_queue_supported_by_tunable(&self) -> bool {
        true
    }
    fn qi_set_enabled(&mut self, _enabled: bool) {}
    fn qi_set_queue_bytes(&mut self, _bytes: u32) {}
    fn qi_release_queue(&mut self) {}
    fn qi_referenced_irte_count(&self) -> u32 {
        0
    }
    fn qi_clear_wait_completion(&mut self) {
        // SAFETY: the final page is reserved for the writeback sequence word.
        unsafe {
            self.qi_virtual
                .as_ptr()
                .add(QI_BYTES)
                .cast::<u64>()
                .write_volatile(0)
        }
    }
}

struct Mapping {
    device_address: u64,
    length: usize,
    physical: u64,
    iova: u64,
    mapped_length: usize,
}

struct Manager {
    units: Vec<Unit>,
    root_table: DmaBlock,
    context_tables: Vec<DmaBlock>,
    page_table: SecondLevel<KernelPageMemory>,
    iovas: IovaAllocator,
    mappings: Vec<Mapping>,
}
// SAFETY: table pages, IOVA state, and MMIO queues are mutated only while the
// global manager lock is held.
unsafe impl Send for Manager {}

impl Manager {
    fn map(&mut self, physical: u64, length: usize) -> Result<u64, Error> {
        if length == 0 || physical.checked_add(length as u64).is_none() {
            return Err(Error::InvalidRange);
        }
        let offset = (physical & (PAGE_SIZE - 1)) as usize;
        let aligned_physical = physical & !(PAGE_SIZE - 1);
        let mapped_length = offset
            .checked_add(length)
            .and_then(|value| value.checked_add(PAGE_SIZE as usize - 1))
            .ok_or(Error::InvalidRange)?
            & !(PAGE_SIZE as usize - 1);
        self.mappings
            .try_reserve(1)
            .map_err(|_| Error::OutOfMemory)?;
        let iova = self.iovas.allocate(mapped_length)?;
        if let Err(error) = dmar_map_buf_locked(
            &mut self.page_table,
            aligned_physical,
            iova,
            mapped_length,
            DMAR_PTE_R | DMAR_PTE_W,
            4,
        ) {
            let _ = self.iovas.release(iova, mapped_length);
            return Err(error);
        }
        for unit in &mut self.units {
            unit.invalidate_all()?;
        }
        let device_address = iova + offset as u64;
        self.mappings.push(Mapping {
            device_address,
            length,
            physical: aligned_physical,
            iova,
            mapped_length,
        });
        Ok(device_address)
    }

    fn unmap(&mut self, device_address: u64, length: usize) -> Result<(), Error> {
        let index = self
            .mappings
            .iter()
            .position(|mapping| {
                mapping.device_address == device_address && mapping.length == length
            })
            .ok_or(Error::InvalidRange)?;
        let mapping = &self.mappings[index];
        dmar_unmap_buf_locked(&mut self.page_table, mapping.iova, mapping.mapped_length)?;
        for unit in &mut self.units {
            if let Err(error) = unit.invalidate_all() {
                // Restore the PTE before reporting failure. Callers must not
                // release the backing page while a stale IOTLB entry may exist.
                let _ = dmar_map_buf_locked(
                    &mut self.page_table,
                    mapping.physical,
                    mapping.iova,
                    mapping.mapped_length,
                    DMAR_PTE_R | DMAR_PTE_W,
                    4,
                );
                return Err(error);
            }
        }
        let mapping = self.mappings.swap_remove(index);
        self.iovas.release(mapping.iova, mapping.mapped_length)
    }
}

fn table(engine: &Engine) -> Result<Option<DmarTable>, Error> {
    let count = engine.table_count().map_err(|_| Error::InvalidTable)?;
    let mut found = None;
    for index in 0..count {
        let bytes = engine.table(index).map_err(|_| Error::InvalidTable)?;
        if bytes.get(..4) != Some(b"DMAR") {
            continue;
        }
        if found.is_some() {
            return Err(Error::InvalidTable);
        }
        found = Some(DmarTable::parse(&bytes)?);
    }
    Ok(found)
}

fn allocate_table_page() -> Result<DmaBlock, Error> {
    allocate_dma(1)
}

/// Discover DMAR before PCI probing; translate by default when a supported DMAR exists.
pub(super) fn init(engine: &Engine) -> Result<(), Error> {
    // Preserve an explicit escape hatch for platforms that need firmware-style
    // identity DMA. Supported DMAR units otherwise enable translation.
    if axhal::boot::command_line_value("intel_iommu") == Some("off") {
        MODE.store(MODE_IDENTITY, Ordering::Release);
        info!("vtd: translation disabled by intel_iommu=off");
        return Ok(());
    }
    MODE.store(MODE_FAILED, Ordering::Release);
    info!("vtd: parsing ACPI DMAR for default translation");
    let Some(dmar) = table(engine)? else {
        MODE.store(MODE_IDENTITY, Ordering::Release);
        info!("vtd: no DMAR table; admitting identity DMA");
        return Ok(());
    };
    if dmar.units.is_empty() {
        return Err(Error::InvalidStructure);
    }
    info!(
        "vtd: parsed DMAR units={} rmrr={}",
        dmar.units.len(),
        dmar.reserved_regions.len()
    );
    let mut page_table = SecondLevel::new(KernelPageMemory { pages: Vec::new() })?;
    let mut maximum = 0u64;
    for &(base, size) in phys_ram_ranges() {
        maximum = maximum.max(
            (base as u64)
                .checked_add(size as u64)
                .ok_or(Error::InvalidRange)?,
        );
    }
    for region in &dmar.reserved_regions {
        maximum = maximum.max(
            region
                .end_inclusive
                .checked_add(1)
                .ok_or(Error::InvalidRange)?,
        );
    }
    maximum = maximum
        .checked_add(IDENTITY_PAGE_SIZE - 1)
        .ok_or(Error::InvalidRange)?
        & !(IDENTITY_PAGE_SIZE - 1);
    if maximum == 0 || maximum > (1 << 48) {
        return Err(Error::InvalidRange);
    }
    page_table.map_identity_2m(maximum)?;

    let root = allocate_table_page()?;
    let second_level_root = page_table.root_physical();
    let mut context_tables = Vec::new();
    context_tables
        .try_reserve_exact(256)
        .map_err(|_| Error::OutOfMemory)?;
    for bus in 0..256u16 {
        let context = allocate_table_page()?;
        // SAFETY: root/context allocations are one page with 256 16-byte
        // hardware entries and remain owned by Manager while TE is active.
        let root_entries = unsafe {
            core::slice::from_raw_parts_mut(root.virtual_address.as_ptr().cast::<RootEntry>(), 256)
        };
        dmar_ensure_ctx_page(root_entries, bus as u8, context.physical)?;
        // SAFETY: `context` owns a page-aligned 4 KiB allocation, enough for
        // 256 hardware ContextEntry values; Manager retains it while active.
        let context_entries = unsafe {
            core::slice::from_raw_parts_mut(
                context.virtual_address.as_ptr().cast::<ContextEntry>(),
                256,
            )
        };
        ctx_id_entry_init(
            context_entries,
            bus << 8,
            1,
            DMAR_CTX2_AW_4LVL as u8,
            Some(second_level_root),
            false,
            false,
            true,
        )?;
        context_tables.push(context);
    }
    core::sync::atomic::fence(Ordering::Release);

    let mut units = Vec::new();
    units
        .try_reserve_exact(dmar.units.len())
        .map_err(|_| Error::OutOfMemory)?;
    for record in &dmar.units {
        let physical = usize::try_from(record.register_base).map_err(|_| Error::InvalidRange)?;
        let base = axmm::iomap(PhysAddr::from_usize(physical), MMIO_BYTES)
            .map_err(|_| Error::MapFailed)?
            .as_usize();
        let qi = allocate_dma(QI_PAGES)?;
        let mut unit = Unit {
            mmio: base,
            qi,
            queue: QiQueue::new(QI_BYTES as u32)?,
            gcmd: 0,
            root_physical: 0,
        };
        unit.enable(root.physical)?;
        units.push(unit);
    }
    let manager = Manager {
        units,
        root_table: root,
        context_tables,
        page_table,
        iovas: IovaAllocator::new(
            maximum
                .max(1 << 30)
                .checked_add((1 << 30) - 1)
                .ok_or(Error::InvalidRange)?
                & !((1 << 30) - 1),
            IOVA_END,
        )?,
        mappings: Vec::new(),
    };
    *MANAGER.lock() = Some(manager);
    MODE.store(MODE_ENABLED, Ordering::Release);
    info!(
        "vtd: DMAR units={} identity_end={maximum:#x} QI enabled; PCI DMA mapping active",
        dmar.units.len()
    );
    Ok(())
}

struct PlatformDma;
#[crate_interface::impl_interface]
impl tk_vtd::PlatformDma for PlatformDma {
    fn pci_dma_allowed() -> bool {
        matches!(MODE.load(Ordering::Acquire), MODE_IDENTITY | MODE_ENABLED)
    }
    fn map(physical: u64, length: usize) -> Result<u64, Error> {
        match MODE.load(Ordering::Acquire) {
            MODE_IDENTITY => {
                if length != 0 && physical.checked_add(length as u64).is_some() {
                    Ok(physical)
                } else {
                    Err(Error::InvalidRange)
                }
            }
            MODE_ENABLED => MANAGER
                .lock()
                .as_mut()
                .ok_or(Error::NoDomain)?
                .map(physical, length),
            _ => Err(Error::NoDomain),
        }
    }
    fn unmap(device_address: u64, length: usize) -> Result<(), Error> {
        match MODE.load(Ordering::Acquire) {
            MODE_IDENTITY => Ok(()),
            MODE_ENABLED => MANAGER
                .lock()
                .as_mut()
                .ok_or(Error::NoDomain)?
                .unmap(device_address, length),
            _ => Err(Error::NoDomain),
        }
    }
}
