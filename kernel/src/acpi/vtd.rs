//! Default-enabled VT-d setup and a shared DMA domain. Legacy PCI drivers see
//! explicit identity entries; virtio/NVMe use disjoint translated IOVAs.
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
    iova::IovaAllocator,
    pgtbl::{PAGE_SIZE, PageMemory, SecondLevel},
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
const RTADDR: usize = 0x20;
const IQH: usize = 0x80;
const IQT: usize = 0x88;
const IQA: usize = 0x90;
const CCMD: usize = 0x28;
const FSTS: usize = 0x34;
const GCMD_TE: u32 = 1 << 31;
const GCMD_SRTP: u32 = 1 << 30;
const GCMD_QIE: u32 = 1 << 26;
const GCMD_IRE: u32 = 1 << 25;
const GSTS_TES: u32 = 1 << 31;
const GSTS_RTPS: u32 = 1 << 30;
const GSTS_QIES: u32 = 1 << 26;
const GSTS_IRES: u32 = 1 << 25;
const ECAP_QI: u64 = 1 << 1;
const CAP_SAGAW_4LVL: u64 = 1 << 10;
const CAP_SPS_2M: u64 = 1 << 34;
const CAP_MGAW_MASK: u64 = 0x3f << 16;
const FSTS_PPF: u32 = 1 << 1;
const FSTS_PFO: u32 = 1;
const QI_PAGES: usize = 4;
const QI_BYTES: usize = QI_PAGES * PAGE_SIZE as usize;
const QI_SIZE_MASK: u32 = 0x7fff0;
const QI_CONTEXT_GLOBAL: u64 = 0x1 | (0x1 << 4);
const QI_IOTLB_GLOBAL: u64 = 0x2 | (0x1 << 4) | (1 << 6) | (1 << 7);
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
    qi_tail: u32,
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
    fn wait_status(&self, mask: u32, set: bool) -> Result<(), Error> {
        let deadline = axhal::time::monotonic_time_nanos() + 100_000_000;
        loop {
            let value = read32(self, GSTS) & mask != 0;
            if value == set {
                return Ok(());
            }
            if axhal::time::monotonic_time_nanos() >= deadline {
                return Err(Error::MapFailed);
            }
            core::hint::spin_loop();
        }
    }

    fn qi_submit(&mut self, first: u64, second: u64) -> Result<(), Error> {
        let next = (self.qi_tail + 16) % QI_BYTES as u32;
        // SAFETY: queue is a dedicated, page-aligned coherent allocation and tail is in-range.
        unsafe {
            let slot = self
                .qi
                .virtual_address
                .as_ptr()
                .add(self.qi_tail as usize)
                .cast::<u64>();
            slot.write_volatile(first);
            slot.add(1).write_volatile(second);
        }
        core::sync::atomic::fence(Ordering::Release);
        write32(self, IQT, next);
        let deadline = axhal::time::monotonic_time_nanos() + 100_000_000;
        loop {
            if read32(self, IQH) & QI_SIZE_MASK == next {
                self.qi_tail = next;
                return self.check_faults();
            }
            if axhal::time::monotonic_time_nanos() >= deadline {
                return Err(Error::MapFailed);
            }
            core::hint::spin_loop();
        }
    }

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
        self.qi_submit(QI_CONTEXT_GLOBAL, 0)?;
        self.qi_submit(QI_IOTLB_GLOBAL, 0)
    }

    fn invalidate_registers(&self, extended: u64) -> Result<(), Error> {
        // Register-based global invalidation clears firmware-retained context
        // and IOTLB state before queued invalidation is enabled.
        write64(self, CCMD, (1 << 63) | (1 << 61)); // ICC | global context
        let deadline = axhal::time::monotonic_time_nanos() + 100_000_000;
        loop {
            if read32(self, CCMD + 4) & (1 << 31) == 0 {
                break;
            }
            if axhal::time::monotonic_time_nanos() >= deadline {
                return Err(Error::MapFailed);
            }
            core::hint::spin_loop();
        }
        let iotlb = (((extended >> 8) & 0x3ff) as usize)
            .checked_mul(16)
            .and_then(|offset| offset.checked_add(8))
            .ok_or(Error::InvalidRange)?;
        if iotlb + 8 > MMIO_BYTES {
            return Err(Error::InvalidRange);
        }
        write64(self, iotlb, (1 << 63) | (1 << 60) | (1 << 49) | (1 << 48));
        let deadline = axhal::time::monotonic_time_nanos() + 100_000_000;
        loop {
            if read64(self, iotlb) & (1 << 63) == 0 {
                return Ok(());
            }
            if axhal::time::monotonic_time_nanos() >= deadline {
                return Err(Error::MapFailed);
            }
            core::hint::spin_loop();
        }
    }

    fn enable(&mut self, root_physical: u64, queue_size: usize) -> Result<(), Error> {
        let capability = read64(self, CAP);
        let extended = read64(self, ECAP);
        if extended & ECAP_QI == 0
            || capability & CAP_SAGAW_4LVL == 0
            || capability & CAP_SPS_2M == 0
            || (capability & CAP_MGAW_MASK) >> 16 < 39
        {
            return Err(Error::MapFailed);
        }
        let status = read32(self, GSTS);
        if status & (GSTS_TES | GSTS_QIES | GSTS_IRES) != 0 {
            write32(self, GCMD, 0);
            self.wait_status(GSTS_TES | GSTS_QIES | GSTS_IRES, false)?;
        }
        write64(self, RTADDR, root_physical);
        write32(self, GCMD, GCMD_SRTP);
        self.wait_status(GSTS_RTPS, true)?;
        self.invalidate_registers(extended)?;
        write32(self, IQT, 0);
        write64(self, IQA, self.qi.physical | (queue_size as u64));
        write32(self, GCMD, GCMD_QIE);
        self.wait_status(GSTS_QIES, true)?;
        self.invalidate_all()?;
        write32(self, GCMD, GCMD_QIE | GCMD_TE);
        self.wait_status(GSTS_TES, true)?;
        self.check_faults()
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
        if let Err(error) = self.page_table.map(aligned_physical, iova, mapped_length) {
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
        self.page_table.unmap(mapping.iova, mapping.mapped_length)?;
        for unit in &mut self.units {
            if let Err(error) = unit.invalidate_all() {
                // Restore the PTE before reporting failure. Callers must not
                // release the backing page while a stale IOTLB entry may exist.
                let _ = self
                    .page_table
                    .map(mapping.physical, mapping.iova, mapping.mapped_length);
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

/// Discover DMAR before PCI probing; translation requires explicit opt-in.
pub(super) fn init(engine: &Engine) -> Result<(), Error> {
    // Keep the firmware-compatible identity path as the default until QEMU
    // and native DMA acceptance cover translated mappings end to end.
    if axhal::boot::command_line_value("intel_iommu") != Some("on") {
        MODE.store(MODE_IDENTITY, Ordering::Release);
        info!("vtd: translation disabled by default; use intel_iommu=on to opt in");
        return Ok(());
    }
    MODE.store(MODE_FAILED, Ordering::Release);
    let Some(dmar) = table(engine)? else {
        MODE.store(MODE_IDENTITY, Ordering::Release);
        info!("vtd: no DMAR table; admitting identity DMA");
        return Ok(());
    };
    if dmar.units.is_empty() {
        return Err(Error::InvalidStructure);
    }
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
    let context = allocate_table_page()?;
    let second_level_root = page_table.root_physical();
    let root_entries = root.virtual_address.as_ptr().cast::<u64>();
    for bus in 0..256usize {
        // SAFETY: root table has 256 16-byte entries.
        unsafe {
            root_entries
                .add(bus * 2)
                .write_volatile(context.physical | 1)
        };
    }
    let context_entries = context.virtual_address.as_ptr().cast::<u64>();
    let context_low = second_level_root | 1; // present, TT=00 (second-level translation)
    let context_high = 2 | (1 << 8); // 48-bit AW, domain ID 1
    for requester in 0..256usize {
        // SAFETY: context table has 256 16-byte entries and is zeroed.
        unsafe {
            context_entries
                .add(requester * 2)
                .write_volatile(context_low);
            context_entries
                .add(requester * 2 + 1)
                .write_volatile(context_high);
        }
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
            qi_tail: 0,
        };
        unit.enable(root.physical, 2)?;
        units.push(unit);
    }
    let manager = Manager {
        units,
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
