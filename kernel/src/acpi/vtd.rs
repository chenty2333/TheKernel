//! VT-d setup with translation enabled when a supported DMAR is present.
//! Requester-specific DMA domains are enabled by default; `iommu_domains=off`
//! selects the shared DMA context, and `intel_iommu=off` selects identity DMA.
use alloc::vec::Vec;
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};

use axalloc::{UsageKind, global_allocator};
use axhal::{
    cache::CacheFlushCaps,
    mem::{PhysAddr, phys_ram_ranges, phys_to_virt, virt_to_phys},
};
use kspin::SpinNoIrq;
use tk_acpica::Engine;
use tk_vtd::{
    DmarTable, Error,
    context::{ctx_id_entry_init, dmar_ensure_ctx_page},
    idpgtbl::{dmar_map_buf_locked, dmar_unmap_buf_locked},
    intrmap::{InterruptRemapIo, InterruptRemapper, InterruptSource},
    iova::IovaAllocator,
    pgtbl::{PAGE_SIZE, PageMemory, SecondLevel},
    qi::{self, QiIo, QiQueue},
    reg::{
        ContextEntry, DMAR_CAP_FRO, DMAR_CAP_MGAW, DMAR_CAP_ND, DMAR_CAP_NFR, DMAR_CAP_RWBF,
        DMAR_CAP_SAGAW, DMAR_CAP_SAGAW_4LVL, DMAR_CAP_SPS, DMAR_CAP_SPS_2M, DMAR_CTX2_AW_4LVL,
        DMAR_ECAP_C, DMAR_ECAP_EIM, DMAR_ECAP_IR, DMAR_ECAP_QI, DMAR_FECTL_IM, DMAR_FECTL_REG,
        DMAR_FRCD2_F, DMAR_FRCD2_F32, DMAR_FSTS_FRI, DMAR_IECTL_IM, DMAR_IECTL_REG, DMAR_PTE_R,
        DMAR_PTE_W, RootEntry,
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
const FSTS_PRO: u32 = 1 << 7;
const QI_ORDER: u32 = 2;
const QI_BYTES: usize = (1 << QI_ORDER) * PAGE_SIZE as usize;
const QI_PAGES: usize = (1 << QI_ORDER) + 1;
const IDENTITY_PAGE_SIZE: u64 = 1 << 21;
const IOVA_END: u64 = 1 << 36;

static MODE: AtomicU8 = AtomicU8::new(MODE_UNKNOWN);
/// Once a DMA invalidation fails, callers can no longer prove whether hardware
/// holds an old or new translation. Do not admit further devices or release
/// mappings/backing from this boot.
static DMA_POISONED: AtomicBool = AtomicBool::new(false);
static NEXT_DIRECT_LEASE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_DIRECT_MAPPING_ID: AtomicU64 = AtomicU64::new(1);
static DIRECT_IDENTITY_LEASES: SpinNoIrq<Vec<DirectIdentityLease>> = SpinNoIrq::new(Vec::new());
/// Why initialization failed closed, repeated when PCI admission is refused so
/// the reason sits next to the refusal on a screen-only machine.
static FAILURE: SpinNoIrq<Option<InitFailure>> = SpinNoIrq::new(None);

#[derive(Clone, Copy, Debug)]
struct InitFailure {
    error: Error,
    stage: &'static str,
    unit_index: Option<usize>,
    register_base: Option<u64>,
    cap: Option<u64>,
    ecap: Option<u64>,
    gsts: Option<u32>,
    fsts: Option<u32>,
}

impl InitFailure {
    const fn global(error: Error, stage: &'static str) -> Self {
        Self {
            error,
            stage,
            unit_index: None,
            register_base: None,
            cap: None,
            ecap: None,
            gsts: None,
            fsts: None,
        }
    }
}

#[derive(Clone, Copy)]
struct UnitProbe {
    mmio: usize,
    register_base: u64,
    cap: u64,
    ecap: u64,
    gsts: u32,
    fsts: u32,
}

fn invalidate_all_units(units: &mut [Unit]) -> Result<(), Error> {
    for unit in units {
        unit.invalidate_all()?;
    }
    Ok(())
}

const GSTS_ACTIVE_MASK: u32 = GSTS_TES | GSTS_QIES | GSTS_IRES;

const fn pci_dma_allowed_in_mode(mode: u8) -> bool {
    matches!(mode, MODE_IDENTITY | MODE_ENABLED)
}

const fn identity_dma_in_mode(mode: u8) -> bool {
    mode == MODE_IDENTITY
}

const fn direct_identity_lease_allowed(mode: u8) -> bool {
    mode == MODE_IDENTITY
}

fn poison_dma_state() -> Error {
    DMA_POISONED.store(true, Ordering::Release);
    MODE.store(MODE_FAILED, Ordering::Release);
    Error::Quarantined
}

fn failed_dma_error() -> Error {
    if DMA_POISONED.load(Ordering::Acquire) {
        Error::Quarantined
    } else {
        Error::NoDomain
    }
}

fn address_limit(width: u8) -> Option<u64> {
    match width {
        1..=63 => Some(1u64 << width),
        64 => None,
        _ => Some(0),
    }
}

/// Validate every remapping unit before writing its first register. QI is an
/// optimization for DMA translation; without it, the architectural register
/// invalidation path remains available. Interrupt remapping, however, needs QI
/// and EIM on the x2APIC platform this kernel runs.
fn validate_unit_capabilities(
    cap: u64,
    ecap: u64,
    maximum: u64,
    host_address_width: u8,
    intremap: bool,
) -> Result<(), Error> {
    // CAP.MGAW encodes the supported physical width minus one. The BSD
    // register extractor is raw; convert it before comparing to bit widths.
    let mgaw = (DMAR_CAP_MGAW(cap) + 1) as u8;
    if DMAR_CAP_SAGAW(cap) & DMAR_CAP_SAGAW_4LVL == 0
        || DMAR_CAP_SPS(cap) & DMAR_CAP_SPS_2M == 0
        || mgaw < 39
    {
        return Err(Error::Unsupported);
    }
    let supported_width = mgaw.min(48);
    if address_limit(supported_width).is_some_and(|limit| maximum > limit)
        || address_limit(host_address_width).is_some_and(|limit| maximum > limit)
    {
        return Err(Error::InvalidRange);
    }
    if intremap
        && ecap & (DMAR_ECAP_QI | DMAR_ECAP_IR | DMAR_ECAP_EIM)
            != (DMAR_ECAP_QI | DMAR_ECAP_IR | DMAR_ECAP_EIM)
    {
        return Err(Error::Unsupported);
    }
    Ok(())
}

fn cache_flush_required(probes: &[UnitProbe]) -> bool {
    probes.iter().any(|probe| probe.ecap & DMAR_ECAP_C == 0)
}

fn cache_flush_for_probes(probes: &[UnitProbe]) -> Result<Option<CacheFlushCaps>, Error> {
    if cache_flush_required(probes) {
        CacheFlushCaps::discover()
            .ok_or(Error::Unsupported)
            .map(Some)
    } else {
        Ok(None)
    }
}

fn flush_cache_range(
    cache_flush: Option<CacheFlushCaps>,
    address: usize,
    length: usize,
) -> Result<(), Error> {
    if let Some(cache_flush) = cache_flush {
        // SAFETY: callers pass a valid mapped range within a retained DMA
        // allocation or one of its table entries.
        unsafe { cache_flush.flush_range(address, length) }.map_err(|_| Error::InvalidRange)?;
    }
    Ok(())
}

fn store_irte_words(mut store: impl FnMut(usize, u64), low: u64, high: u64) {
    // The high word contains the requester/source qualification. Publish it
    // before the low word's Present bit, as FreeBSD's dmar_ir_program_irte()
    // does for a new entry.
    store(1, high);
    store(0, low);
}

/// Return the write needed to mask the DMAR fault-event MSI when this kernel
/// has no registered VT-d fault handler. Write only IM: FECTL.IP is W1C, so
/// echoing a pending bit from the readback could acknowledge firmware state.
const fn fault_event_mask_write(control: u32) -> Option<u32> {
    let mask = DMAR_FECTL_IM as u32;
    if control & mask == 0 {
        Some(mask)
    } else {
        None
    }
}

fn validate_all_unit_capabilities(
    probes: &[UnitProbe],
    maximum: u64,
    host_address_width: u8,
    intremap: bool,
) -> Result<(), (usize, Error)> {
    for (index, probe) in probes.iter().enumerate() {
        validate_unit_capabilities(probe.cap, probe.ecap, maximum, host_address_width, intremap)
            .map_err(|error| (index, error))?;
    }
    Ok(())
}

fn identity_fallback_safe_before_enable(statuses: impl IntoIterator<Item = u32>) -> bool {
    statuses
        .into_iter()
        .all(|status| status & GSTS_ACTIVE_MASK == 0)
}

static REQUESTER_DOMAINS: AtomicBool = AtomicBool::new(false);
static MANAGER: SpinNoIrq<Option<Manager>> = SpinNoIrq::new(None);

struct DirectIdentityBatch {
    id: u64,
    pages: Vec<u64>,
}

struct DirectIdentityLease {
    requester: tk_vtd::PciRequester,
    id: u64,
    initial_pages: Vec<u64>,
    batches: Vec<DirectIdentityBatch>,
}

struct DmaBlock {
    physical: u64,
    virtual_address: NonNull<u8>,
    pages: usize,
}
// SAFETY: ownership is unique and page memory is accessed through the manager lock.
unsafe impl Send for DmaBlock {}

impl Drop for DmaBlock {
    fn drop(&mut self) {
        global_allocator().dealloc_pages(
            self.virtual_address.as_ptr() as usize,
            self.pages,
            UsageKind::Dma,
        );
    }
}

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
    cache_flush: Option<CacheFlushCaps>,
}
// SAFETY: pages are uniquely owned and manager access is serialized; the
// physical-to-virtual mapping is only returned for pages owned by this object.
unsafe impl PageMemory for KernelPageMemory {
    fn alloc_page(&mut self) -> Result<u64, Error> {
        let page = allocate_dma(1)?;
        let physical = page.physical;
        self.pages.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
        self.pages.push(page);
        if let Err(error) = self.flush_range(physical, 0, PAGE_SIZE as usize) {
            drop(self.pages.pop());
            return Err(error);
        }
        Ok(physical)
    }
    fn page_mut(&mut self, physical: u64) -> Option<NonNull<[u64; 512]>> {
        if !physical.is_multiple_of(PAGE_SIZE)
            || !self.pages.iter().any(|page| page.physical == physical)
        {
            return None;
        }
        let address = phys_to_virt(PhysAddr::from_usize(usize::try_from(physical).ok()?));
        let pointer = address.as_mut_ptr().cast::<[u64; 512]>();
        NonNull::new(pointer)
    }
    fn flush_range(&mut self, physical: u64, offset: usize, length: usize) -> Result<(), Error> {
        let Some(page) = self.pages.iter().find(|page| page.physical == physical) else {
            return Err(Error::InvalidRange);
        };
        let end = offset.checked_add(length).ok_or(Error::InvalidRange)?;
        if end > page.pages * PAGE_SIZE as usize {
            return Err(Error::InvalidRange);
        }
        flush_cache_range(
            self.cache_flush,
            page.virtual_address.as_ptr() as usize + offset,
            length,
        )
    }
    unsafe fn free_page(&mut self, physical: u64) {
        if let Some(index) = self.pages.iter().position(|page| page.physical == physical) {
            drop(self.pages.swap_remove(index));
        }
    }
}

struct Unit {
    mmio: usize,
    register_base: u64,
    cache_flush: Option<CacheFlushCaps>,
    domain_count: u32,
    next_domain_id: u32,
    qi: DmaBlock,
    queue: QiQueue,
    gcmd: u32,
    root_physical: u64,
    ir_table: Option<DmaBlock>,
    ir: Option<InterruptRemapper>,
}

fn cap_domain_count(cap: u64) -> u32 {
    1u32 << (4 + 2 * DMAR_CAP_ND(cap) as u32)
}

fn take_domain_id(next_id: &mut u32, domain_count: u32) -> Result<u16, Error> {
    let id = *next_id;
    if id == 0 || id > u32::from(u16::MAX) || id >= domain_count {
        return Err(Error::OutOfMemory);
    }
    *next_id = id.checked_add(1).ok_or(Error::OutOfMemory)?;
    Ok(id as u16)
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

fn probe_unit(register_base: u64) -> Result<UnitProbe, Error> {
    let physical = usize::try_from(register_base).map_err(|_| Error::InvalidRange)?;
    let mmio = axmm::iomap(PhysAddr::from_usize(physical), MMIO_BYTES)
        .map_err(|_| Error::MapFailed)?
        .as_usize();
    // SAFETY: `iomap` exposes the full 4 KiB VT-d register page and all offsets
    // are naturally aligned, architecturally defined capability/status regs.
    let (cap, ecap, gsts, fsts) = unsafe {
        (
            ((mmio + CAP) as *const u64).read_volatile(),
            ((mmio + ECAP) as *const u64).read_volatile(),
            ((mmio + GSTS) as *const u32).read_volatile(),
            ((mmio + FSTS) as *const u32).read_volatile(),
        )
    };
    Ok(UnitProbe {
        mmio,
        register_base,
        cap,
        ecap,
        gsts,
        fsts,
    })
}

impl Unit {
    fn check_faults(&self) -> Result<(), Error> {
        let status = read32(self, FSTS);
        if status & (FSTS_PPF | FSTS_PFO | FSTS_PRO) == 0 {
            return Ok(());
        }
        if status & FSTS_PPF != 0 {
            let cap = read64(self, CAP);
            drain_primary_fault_records(
                cap,
                status,
                |offset| read64(self, offset),
                |offset, value| write32(self, offset, value),
                |sid, reason, address, index| {
                    error!(
                        "vtd: DMA fault sid={sid:#06x} reason={reason:#x} address={address:#x} \
                         record={index}"
                    );
                },
            )?;
        }
        // Return primary fault records to hardware before acknowledging their
        // FSTS summary bits. Linux's dmar_fault() also clears FRCD.F for each
        // record and acknowledges PPF/PFO/PRO afterward.
        write32(self, FSTS, status & (FSTS_PPF | FSTS_PFO | FSTS_PRO));
        Err(Error::MapFailed)
    }

    fn invalidate_all(&mut self) -> Result<(), Error> {
        if !self.queue.enabled {
            // Linux's Intel IOMMU driver also falls back to register-based
            // invalidations when queued invalidation is unavailable. This is
            // required on units where QI is absent; CAP.CM alone does not
            // disable QI (ECAP.QI controls that capability).
            utils::dmar_inv_ctx_glob(self)?;
            utils::dmar_inv_iotlb_glob(self)?;
            return self.check_faults();
        }
        let ir_table_physical = self.ir_table.as_ref().map_or(0, |table| table.physical);
        let irte_count = self
            .ir
            .as_ref()
            .map_or(0, |table| table.entry_count() as u32);
        let mut io = UnitQiIo {
            mmio: self.mmio,
            qi_physical: self.qi.physical,
            qi_virtual: self.qi.virtual_address,
            gcmd: &mut self.gcmd,
            ir_table_physical,
            irte_count,
        };
        qi::dmar_qi_invalidate_ctx_glob_locked(&mut io, &mut self.queue)?;
        qi::dmar_qi_invalidate_iotlb_glob_locked(&mut io, &mut self.queue)?;
        self.check_faults()
    }

    fn enable(
        &mut self,
        root_physical: u64,
        hardware_touched: &mut bool,
    ) -> Result<(), (&'static str, Error)> {
        macro_rules! step {
            ($stage:literal, $result:expr) => {
                $result.map_err(|error| ($stage, error))?
            };
        }
        let capability = read64(self, CAP);
        let extended = read64(self, ECAP);
        if DMAR_CAP_SAGAW(capability) & DMAR_CAP_SAGAW_4LVL == 0
            || DMAR_CAP_SPS(capability) & DMAR_CAP_SPS_2M == 0
            || DMAR_CAP_MGAW(capability) + 1 < 39
        {
            return Err(("capability recheck", Error::Unsupported));
        }
        self.gcmd = read32(self, GCMD);
        let status = read32(self, GSTS);
        // Linux installs dmar_fault and its MSI before enabling translation.
        // This kernel currently polls FSTS during VT-d operations instead of
        // owning a fault-event IRQ, so prevent a fault from being delivered to
        // firmware's stale/unregistered FEADDR/FEDATA vector.
        if let Some(mask) = fault_event_mask_write(read32(self, DMAR_FECTL_REG as usize)) {
            *hardware_touched = true;
            write32(self, DMAR_FECTL_REG as usize, mask);
            if read32(self, DMAR_FECTL_REG as usize) & DMAR_FECTL_IM as u32 == 0 {
                return Err(("fault-event interrupt mask readback", Error::MapFailed));
            }
        }
        if status & GSTS_IRES != 0 {
            *hardware_touched = true;
            step!(
                "disable existing interrupt remapping",
                utils::dmar_disable_ir(self)
            );
        }
        if status & GSTS_QIES != 0 {
            *hardware_touched = true;
            let mut io = UnitQiIo {
                mmio: self.mmio,
                qi_physical: self.qi.physical,
                qi_virtual: self.qi.virtual_address,
                gcmd: &mut self.gcmd,
                ir_table_physical: self.ir_table.as_ref().map_or(0, |table| table.physical),
                irte_count: self
                    .ir
                    .as_ref()
                    .map_or(0, |table| table.entry_count() as u32),
            };
            step!(
                "disable existing queued invalidation",
                qi::dmar_disable_qi(&mut io)
            );
        }
        if status & GSTS_TES != 0 {
            *hardware_touched = true;
            step!(
                "disable existing translation",
                utils::dmar_disable_translation(self)
            );
        }
        self.root_physical = root_physical;
        *hardware_touched = true;
        step!("load root table", utils::dmar_load_root_entry_ptr(self));
        // Clear state retained by firmware while queued invalidation is off.
        step!(
            "initial context invalidation",
            utils::dmar_inv_ctx_glob(self)
        );
        step!(
            "initial IOTLB invalidation",
            utils::dmar_inv_iotlb_glob(self)
        );
        if capability & DMAR_CAP_RWBF != 0 {
            step!(
                "initial write-buffer flush",
                utils::dmar_flush_write_bufs(self)
            );
        }
        if extended & DMAR_ECAP_QI != 0 {
            let mut io = UnitQiIo {
                mmio: self.mmio,
                qi_physical: self.qi.physical,
                qi_virtual: self.qi.virtual_address,
                gcmd: &mut self.gcmd,
                ir_table_physical: self.ir_table.as_ref().map_or(0, |table| table.physical),
                irte_count: self
                    .ir
                    .as_ref()
                    .map_or(0, |table| table.entry_count() as u32),
            };
            if let Some(queue) = step!(
                "queued-invalidation initialization",
                qi::dmar_init_qi(&mut io, QI_ORDER, QI_ORDER)
            ) {
                self.queue = queue;
            }
        } else {
            warn!(
                "vtd: DRHD {:#x} has no queued invalidation; using register invalidation",
                self.register_base
            );
        }
        step!("post-QI global invalidation", self.invalidate_all());
        if axhal::boot::command_line_value("intremap") == Some("on") {
            if extended & (DMAR_ECAP_IR | DMAR_ECAP_EIM | DMAR_ECAP_QI)
                != (DMAR_ECAP_IR | DMAR_ECAP_EIM | DMAR_ECAP_QI)
                || !self.queue.enabled
            {
                return Err(("interrupt-remapping capabilities", Error::Unsupported));
            }
            let table = allocate_dma(1).map_err(|error| ("interrupt-table allocation", error))?;
            let mut remapper = InterruptRemapper::new(256, extended & DMAR_ECAP_EIM != 0)
                .map_err(|error| ("interrupt-remapper allocation", error))?;
            let physical = table.physical;
            self.ir_table = Some(table);
            let virtual_address = self
                .ir_table
                .as_ref()
                .ok_or(("interrupt-table allocation", Error::NoDomain))?
                .virtual_address
                .as_ptr() as usize;
            flush_cache_range(self.cache_flush, virtual_address, PAGE_SIZE as usize)
                .map_err(|error| ("interrupt-table cache flush", error))?;
            let qi_enabled = self.queue.enabled;
            {
                let mut io = UnitInterruptIo::from_unit(self)
                    .map_err(|error| ("interrupt-remapper register adapter", error))?;
                if !step!(
                    "interrupt-remapper initialization",
                    remapper.initialize(&mut io, true, qi_enabled, physical)
                ) {
                    return Err(("interrupt-remapper unsupported", Error::Unsupported));
                }
            }
            self.ir = Some(remapper);
            info!(
                "vtd: interrupt remapping enabled for DRHD {:#x}",
                self.register_base
            );
        }
        step!("enable translation", utils::dmar_enable_translation(self));
        step!("post-enable fault check", self.check_faults());
        Ok(())
    }
}

/// Drain the bounded primary-fault ring, reading each FRCD2 before FRCD1 and
/// returning every consumed slot to hardware by W1C-clearing FRCD2.F.
fn drain_primary_fault_records(
    cap: u64,
    status: u32,
    mut read64: impl FnMut(usize) -> u64,
    mut clear32: impl FnMut(usize, u32),
    mut report: impl FnMut(u16, u8, u64, usize),
) -> Result<usize, Error> {
    if status & FSTS_PPF == 0 {
        return Ok(0);
    }
    let record_count = usize::try_from(DMAR_CAP_NFR(cap)).map_err(|_| Error::InvalidStructure)?;
    let mut index =
        usize::try_from(DMAR_FSTS_FRI(status as u64)).map_err(|_| Error::InvalidStructure)?;
    let records_start = usize::try_from(DMAR_CAP_FRO(cap))
        .ok()
        .and_then(|offset| offset.checked_mul(16))
        .ok_or(Error::InvalidStructure)?;
    if record_count == 0 || index >= record_count || records_start >= MMIO_BYTES {
        return Err(Error::InvalidStructure);
    }

    let mut drained = 0;
    for _ in 0..record_count {
        let offset = records_start
            .checked_add(index.checked_mul(16).ok_or(Error::InvalidStructure)?)
            .ok_or(Error::InvalidStructure)?;
        if offset.checked_add(16).map_or(true, |end| end > MMIO_BYTES) {
            return Err(Error::InvalidStructure);
        }
        let frcd2 = read64(offset + 8);
        if frcd2 & DMAR_FRCD2_F == 0 {
            break;
        }
        let frcd1 = read64(offset);
        let sid = frcd2 as u16;
        let reason = ((frcd2 >> 32) & 0xff) as u8;
        let address = frcd1 & !0xfff;
        clear32(offset + 12, DMAR_FRCD2_F32 as u32);
        report(sid, reason, address, index);
        drained += 1;
        index += 1;
        if index == record_count {
            index = 0;
        }
    }
    Ok(drained)
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
        self.ir_table.as_ref().map_or(0, |table| table.physical)
    }
    fn interrupt_entry_count(&self) -> u32 {
        self.ir
            .as_ref()
            .map_or(0, |table| table.entry_count() as u32)
    }
    fn x2apic_mode(&self) -> bool {
        true
    }
    fn now_ns(&mut self) -> u64 {
        axhal::time::monotonic_time_nanos()
    }
    fn flush_translation(&mut self, address: usize, length: usize) -> Result<(), Error> {
        flush_cache_range(self.cache_flush, address, length)
    }
}

struct UnitQiIo<'a> {
    mmio: usize,
    qi_physical: u64,
    qi_virtual: NonNull<u8>,
    gcmd: &'a mut u32,
    ir_table_physical: u64,
    irte_count: u32,
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
        self.ir_table_physical
    }
    fn interrupt_entry_count(&self) -> u32 {
        self.irte_count
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
        self.irte_count
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

struct UnitInterruptIo<'a> {
    mmio: usize,
    qi_physical: u64,
    qi_virtual: NonNull<u8>,
    queue: &'a mut QiQueue,
    gcmd: &'a mut u32,
    ir_table_physical: u64,
    ir_table_virtual: NonNull<u8>,
    irte_count: u32,
    cache_flush: Option<CacheFlushCaps>,
}

impl UnitInterruptIo<'_> {
    fn from_unit(unit: &mut Unit) -> Result<UnitInterruptIo<'_>, Error> {
        let table = unit.ir_table.as_ref().ok_or(Error::NoDomain)?;
        Ok(UnitInterruptIo {
            mmio: unit.mmio,
            qi_physical: unit.qi.physical,
            qi_virtual: unit.qi.virtual_address,
            queue: &mut unit.queue,
            gcmd: &mut unit.gcmd,
            ir_table_physical: table.physical,
            ir_table_virtual: table.virtual_address,
            irte_count: (PAGE_SIZE as usize / core::mem::size_of::<tk_vtd::reg::Irte>()) as u32,
            cache_flush: unit.cache_flush,
        })
    }
}

impl InterruptRemapIo for UnitInterruptIo<'_> {
    fn store_irte(&mut self, index: u16, entry: tk_vtd::reg::Irte) -> Result<(), Error> {
        if index as u32 >= self.irte_count {
            return Err(Error::InvalidRange);
        }
        // SAFETY: IRTA owns a page-aligned table with 256 16-byte entries.
        let slot = unsafe {
            self.ir_table_virtual
                .as_ptr()
                .add(index as usize * 16)
                .cast::<u64>()
        };
        // SAFETY: the validated slot is within the retained IR table; the
        // callback writes its two aligned hardware words in explicit order.
        store_irte_words(
            |word, value| unsafe { slot.add(word).write_volatile(value) },
            entry.irte1,
            entry.irte2,
        );
        if flush_cache_range(
            self.cache_flush,
            slot as usize,
            core::mem::size_of::<tk_vtd::reg::Irte>(),
        )
        .is_err()
        {
            return Err(Error::Quarantined);
        }
        core::sync::atomic::fence(Ordering::Release);
        Ok(())
    }
    fn invalidate_iec(&mut self, index: u16, count: u16) -> Result<(), Error> {
        let Self {
            mmio,
            qi_physical,
            qi_virtual,
            queue,
            gcmd,
            ir_table_physical,
            irte_count,
            ..
        } = self;
        let mut io = UnitQiIo {
            mmio: *mmio,
            qi_physical: *qi_physical,
            qi_virtual: *qi_virtual,
            gcmd,
            ir_table_physical: *ir_table_physical,
            irte_count: *irte_count,
        };
        qi::dmar_qi_invalidate_iec(&mut io, queue, index as u32, count as u32)
    }
    fn invalidate_iec_global(&mut self) -> Result<(), Error> {
        let Self {
            mmio,
            qi_physical,
            qi_virtual,
            queue,
            gcmd,
            ir_table_physical,
            irte_count,
            ..
        } = self;
        let mut io = UnitQiIo {
            mmio: *mmio,
            qi_physical: *qi_physical,
            qi_virtual: *qi_virtual,
            gcmd,
            ir_table_physical: *ir_table_physical,
            irte_count: *irte_count,
        };
        qi::dmar_qi_invalidate_iec_glob(&mut io, queue)
    }
    fn load_table_pointer(&mut self, physical: u64, _size_order: u8) -> Result<(), Error> {
        if physical != self.ir_table_physical {
            return Err(Error::InvalidRange);
        }
        let Self {
            mmio,
            qi_physical,
            qi_virtual,
            gcmd,
            ir_table_physical,
            irte_count,
            ..
        } = self;
        let mut io = UnitQiIo {
            mmio: *mmio,
            qi_physical: *qi_physical,
            qi_virtual: *qi_virtual,
            gcmd,
            ir_table_physical: *ir_table_physical,
            irte_count: *irte_count,
        };
        utils::dmar_load_irt_ptr(&mut io)
    }
    fn enable_interrupt_remapping(&mut self) -> Result<(), Error> {
        let Self {
            mmio,
            qi_physical,
            qi_virtual,
            gcmd,
            ir_table_physical,
            irte_count,
            ..
        } = self;
        let mut io = UnitQiIo {
            mmio: *mmio,
            qi_physical: *qi_physical,
            qi_virtual: *qi_virtual,
            gcmd,
            ir_table_physical: *ir_table_physical,
            irte_count: *irte_count,
        };
        utils::dmar_enable_ir(&mut io)
    }
    fn disable_interrupt_remapping(&mut self) -> Result<(), Error> {
        let Self {
            mmio,
            qi_physical,
            qi_virtual,
            gcmd,
            ir_table_physical,
            irte_count,
            ..
        } = self;
        let mut io = UnitQiIo {
            mmio: *mmio,
            qi_physical: *qi_physical,
            qi_virtual: *qi_virtual,
            gcmd,
            ir_table_physical: *ir_table_physical,
            irte_count: *irte_count,
        };
        utils::dmar_disable_ir(&mut io)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MappingState {
    Active,
    Quarantined,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Mapping {
    device_address: u64,
    length: usize,
    physical: u64,
    iova: u64,
    mapped_length: usize,
    state: MappingState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DomainState {
    Installing,
    Ready,
    Quarantined,
}

struct DeviceDomain {
    requester: tk_vtd::PciRequester,
    unit_index: usize,
    id: u16,
    page_table: SecondLevel<KernelPageMemory>,
    iovas: IovaAllocator,
    mappings: Vec<Mapping>,
    state: DomainState,
}

struct IdentityPage {
    physical: u64,
    references: u32,
    permanent: bool,
}

struct IdentityBatch {
    id: u64,
    pages: Vec<u64>,
    state: MappingState,
}

struct IdentityDmaOwner {
    lease_id: u64,
    pages: Vec<IdentityPage>,
    batches: Vec<IdentityBatch>,
}

struct RequesterIdentityLease {
    requester: tk_vtd::PciRequester,
    owner: IdentityDmaOwner,
}

fn prepare_identity_batch_record(
    owner: &mut IdentityDmaOwner,
    pages: &[u64],
) -> Result<Vec<u64>, Error> {
    let mut new_pages = 0usize;
    for &page in pages {
        if let Some(owned) = owner.pages.iter().find(|owned| owned.physical == page) {
            if owned.references == u32::MAX {
                return Err(Error::OutOfMemory);
            }
        } else {
            new_pages += 1;
        }
    }
    owner
        .pages
        .try_reserve(new_pages)
        .map_err(|_| Error::OutOfMemory)?;
    owner
        .batches
        .try_reserve(1)
        .map_err(|_| Error::OutOfMemory)?;
    let mut recorded_pages = Vec::new();
    recorded_pages
        .try_reserve_exact(pages.len())
        .map_err(|_| Error::OutOfMemory)?;
    recorded_pages.extend_from_slice(pages);
    Ok(recorded_pages)
}

fn publish_identity_batch(
    owner: &mut IdentityDmaOwner,
    id: u64,
    pages: Vec<u64>,
    state: MappingState,
) {
    for &page in &pages {
        if let Some(owned) = owner.pages.iter_mut().find(|owned| owned.physical == page) {
            owned.references += 1;
        } else {
            owner.pages.push(IdentityPage {
                physical: page,
                references: 1,
                permanent: false,
            });
        }
    }
    owner.batches.push(IdentityBatch { id, pages, state });
}

fn retire_identity_batch_record(owner: &mut IdentityDmaOwner, id: u64) -> Result<(), Error> {
    let batch_index = owner
        .batches
        .iter()
        .position(|batch| batch.id == id)
        .ok_or(Error::InvalidRange)?;
    if owner.batches[batch_index].state != MappingState::Active {
        return Err(Error::Quarantined);
    }
    for &page in &owner.batches[batch_index].pages {
        let page_index = owner
            .pages
            .iter()
            .position(|owned| owned.physical == page)
            .ok_or(Error::InvalidStructure)?;
        if owner.pages[page_index].references == 0 {
            return Err(Error::InvalidStructure);
        }
        owner.pages[page_index].references -= 1;
        if owner.pages[page_index].references == 0 && !owner.pages[page_index].permanent {
            owner.pages.swap_remove(page_index);
        }
    }
    owner.batches.swap_remove(batch_index);
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IrRouteState {
    Active,
    Quarantined,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IrRoute {
    requester: tk_vtd::PciRequester,
    vector: u8,
    unit_index: usize,
    cookie: u16,
    message_address: u64,
    message_data: u32,
    state: IrRouteState,
}

fn find_owned_msi_vector(
    routes: &[IrRoute],
    requester: tk_vtd::PciRequester,
    message_address: u64,
    message_data: u32,
) -> Result<Option<u8>, Error> {
    let mut found = None;
    for route in routes {
        if route.requester == requester
            && route.message_address == message_address
            && route.message_data == message_data
        {
            if route.state != IrRouteState::Active {
                return Err(Error::Quarantined);
            }
            if found.replace(route.vector).is_some() {
                return Err(Error::InvalidStructure);
            }
        }
    }
    Ok(found)
}

fn retire_ir_route_after_invalidation(
    routes: &mut Vec<IrRoute>,
    index: usize,
    invalidate: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    let Some(route) = routes.get(index).copied() else {
        return Err(Error::InvalidRange);
    };
    if route.state != IrRouteState::Active {
        return Err(Error::Quarantined);
    }
    if invalidate().is_err() {
        routes[index].state = IrRouteState::Quarantined;
        return Err(Error::Quarantined);
    }
    routes.swap_remove(index);
    Ok(())
}

fn require_ready_domain(state: DomainState) -> Result<(), Error> {
    match state {
        DomainState::Ready => Ok(()),
        DomainState::Installing | DomainState::Quarantined => Err(Error::Quarantined),
    }
}

fn complete_domain_install(
    state: &mut DomainState,
    invalidate: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    if invalidate().is_err() {
        *state = DomainState::Quarantined;
        return Err(Error::Quarantined);
    }
    *state = DomainState::Ready;
    Ok(())
}

/// Record a PTE update before invalidating caches so ambiguous completion can
/// never lose the software owner of the translation/backing relationship.
fn publish_mapping_after_invalidation(
    mappings: &mut Vec<Mapping>,
    mapping: Mapping,
    invalidate: impl FnOnce() -> Result<(), Error>,
) -> Result<u64, Error> {
    mappings.push(mapping);
    let index = mappings.len() - 1;
    if invalidate().is_err() {
        mappings[index].state = MappingState::Quarantined;
        return Err(Error::Quarantined);
    }
    Ok(mapping.device_address)
}

/// The caller owns the physical allocation until both the leaf removal and
/// all-unit invalidation are confirmed. Any uncertainty keeps the record and
/// IOVA reserved for quarantine instead of best-effort PTE restoration.
fn retire_mapping_after_invalidation(
    mappings: &mut Vec<Mapping>,
    index: usize,
    iovas: &mut IovaAllocator,
    clear_pte: impl FnOnce() -> Result<(), Error>,
    invalidate: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    let Some(mapping) = mappings.get(index).copied() else {
        return Err(Error::InvalidRange);
    };
    if mapping.state != MappingState::Active {
        return Err(Error::Quarantined);
    }
    if clear_pte().is_err() || invalidate().is_err() {
        mappings[index].state = MappingState::Quarantined;
        return Err(Error::Quarantined);
    }
    mappings.swap_remove(index);
    iovas.release(mapping.iova, mapping.mapped_length)
}

/// Build the ACPI DMAR PCI path for a requester by walking the configured
/// ECAM bridge topology.  DMAR scopes identify bridge prefixes, not merely
/// the endpoint's downstream bus number.
fn dmar_path(requester: tk_vtd::PciRequester) -> Vec<(u8, u8, u8)> {
    axdriver::requester_path(requester.bus, requester.device, requester.function)
        .unwrap_or_default()
}

fn dmar_scope_matches(
    scope: &tk_vtd::OwnedScope,
    requester: tk_vtd::PciRequester,
    path: &[(u8, u8, u8)],
) -> bool {
    if scope.scope_type != 1 && scope.scope_type != 2 {
        return false;
    }
    let Some(start) = path.iter().position(|entry| entry.0 == scope.start_bus) else {
        return false;
    };
    let path = &path[start..];
    if scope.path.is_empty() || scope.path.len() % 2 != 0 {
        return false;
    }
    let hops = scope.path.len() / 2;
    if scope.scope_type == 1 && hops != path.len() {
        return false;
    }
    if scope.scope_type == 2 && hops > path.len() {
        return false;
    }
    scope
        .path
        .chunks_exact(2)
        .zip(path.iter())
        .all(|(pair, (_, device, function))| pair[0] == *device && pair[1] & 7 == *function)
        && (path.last().is_some_and(|(_, device, function)| {
            *device == requester.device && *function == requester.function
        }) || scope.scope_type == 2)
}

fn dmar_unit_index_for_requester(
    table: &DmarTable,
    requester: tk_vtd::PciRequester,
    path: &[(u8, u8, u8)],
) -> Result<usize, Error> {
    if path.is_empty()
        && table.units.iter().any(|unit| {
            unit.segment == requester.segment && !unit.include_all && !unit.scopes.is_empty()
        })
    {
        return Err(Error::NoDevice);
    }
    let mut scoped = None;
    for (index, unit) in table.units.iter().enumerate() {
        if unit.segment == requester.segment
            && unit
                .scopes
                .iter()
                .any(|scope| dmar_scope_matches(scope, requester, path))
        {
            if scoped.replace(index).is_some() {
                return Err(Error::InvalidStructure);
            }
        }
    }
    if let Some(index) = scoped {
        return Ok(index);
    }
    let mut include_all = None;
    for (index, unit) in table.units.iter().enumerate() {
        if unit.segment == requester.segment && unit.include_all {
            if include_all.replace(index).is_some() {
                return Err(Error::InvalidStructure);
            }
        }
    }
    include_all.ok_or(Error::NoDevice)
}

fn identity_pages_for_requester(
    dmar: &DmarTable,
    requester: tk_vtd::PciRequester,
    initial_pages: &[u64],
) -> Result<Vec<u64>, Error> {
    let path = dmar_path(requester);
    if path.is_empty()
        && dmar
            .reserved_regions
            .iter()
            .any(|region| region.segment == requester.segment)
    {
        // Without PCI bridge topology, the service cannot prove whether a
        // scoped RMRR belongs to this requester; do not silently omit it.
        return Err(Error::NoDevice);
    }
    identity_pages_for_path(dmar, requester, &path, initial_pages)
}

fn identity_pages_for_path(
    dmar: &DmarTable,
    requester: tk_vtd::PciRequester,
    path: &[(u8, u8, u8)],
    initial_pages: &[u64],
) -> Result<Vec<u64>, Error> {
    let mut pages = Vec::new();
    pages
        .try_reserve(initial_pages.len())
        .map_err(|_| Error::OutOfMemory)?;
    for &page in initial_pages {
        if !page.is_multiple_of(PAGE_SIZE) || page >= 1 << 48 {
            return Err(Error::InvalidRange);
        }
        pages.push(page);
    }
    for region in &dmar.reserved_regions {
        if region.segment != requester.segment
            || !region
                .scopes
                .iter()
                .any(|scope| dmar_scope_matches(scope, requester, path))
        {
            continue;
        }
        let base = region.base & !(PAGE_SIZE - 1);
        let end = region
            .end_inclusive
            .checked_add(1)
            .ok_or(Error::InvalidRange)?;
        let end = end.checked_add(PAGE_SIZE - 1).ok_or(Error::InvalidRange)? & !(PAGE_SIZE - 1);
        if base >= end || end > 1 << 48 {
            return Err(Error::InvalidRange);
        }
        let additional =
            usize::try_from((end - base) / PAGE_SIZE).map_err(|_| Error::OutOfMemory)?;
        pages
            .try_reserve(additional)
            .map_err(|_| Error::OutOfMemory)?;
        pages.extend((base..end).step_by(PAGE_SIZE as usize));
    }
    pages.sort_unstable();
    pages.dedup();
    if pages.is_empty() {
        return Err(Error::InvalidRange);
    }
    Ok(pages)
}

struct Manager {
    dmar: DmarTable,
    units: Vec<Unit>,
    cache_flush: Option<CacheFlushCaps>,
    root_table: DmaBlock,
    context_tables: Vec<DmaBlock>,
    page_table: SecondLevel<KernelPageMemory>,
    iovas: IovaAllocator,
    mappings: Vec<Mapping>,
    ir_routes: Vec<IrRoute>,
    identity_end: u64,
    iova_start: u64,
    device_domains: Vec<DeviceDomain>,
    identity_leases: Vec<RequesterIdentityLease>,
    next_identity_lease_id: u64,
    next_identity_mapping_id: u64,
}
// SAFETY: table pages, IOVA state, and MMIO queues are mutated only while the
// global manager lock is held.
unsafe impl Send for Manager {}

impl Manager {
    fn create_device_domain(&mut self, requester: tk_vtd::PciRequester) -> Result<usize, Error> {
        let unit_index =
            dmar_unit_index_for_requester(&self.dmar, requester, &dmar_path(requester))?;
        self.units.get(unit_index).ok_or(Error::NoDevice)?;
        if self
            .device_domains
            .iter()
            .any(|domain| domain.requester == requester)
        {
            return Err(Error::InvalidStructure);
        }
        let domain_count = self.units[unit_index].domain_count;
        let id = take_domain_id(&mut self.units[unit_index].next_domain_id, domain_count)?;
        let mut page_table = SecondLevel::new(KernelPageMemory {
            pages: Vec::new(),
            cache_flush: self.cache_flush,
        })?;
        let root = page_table.root_physical();
        let domain = DeviceDomain {
            requester,
            unit_index,
            id,
            page_table,
            iovas: IovaAllocator::new(self.iova_start, IOVA_END)?,
            mappings: Vec::new(),
            state: DomainState::Installing,
        };
        self.device_domains
            .try_reserve(1)
            .map_err(|_| Error::OutOfMemory)?;
        self.device_domains.push(domain);
        let domain_index = self.device_domains.len() - 1;

        let Some(context_page) = self.context_tables.get(requester.bus as usize) else {
            self.device_domains.swap_remove(domain_index);
            return Err(Error::InvalidRange);
        };
        // SAFETY: each retained page contains 256 zeroed/initialized hardware
        // context entries and is exclusively mutated under MANAGER's lock.
        let entries = unsafe {
            core::slice::from_raw_parts_mut(
                context_page.virtual_address.as_ptr().cast::<ContextEntry>(),
                256,
            )
        };
        let rid = (u16::from(requester.device) << 3) | u16::from(requester.function);
        if let Err(error) = ctx_id_entry_init(
            entries,
            rid,
            id,
            DMAR_CTX2_AW_4LVL as u8,
            Some(root),
            false,
            true,
            false,
        ) {
            // ctx_id_entry_init validates every fallible input before writing
            // the entry; no hardware can have observed this page table yet.
            self.device_domains.swap_remove(domain_index);
            return Err(error);
        }
        let entry_virtual = context_page.virtual_address.as_ptr() as usize
            + rid as usize * core::mem::size_of::<ContextEntry>();
        if flush_cache_range(
            self.cache_flush,
            entry_virtual,
            core::mem::size_of::<ContextEntry>(),
        )
        .is_err()
        {
            // The context word has already been published and may be observed
            // by hardware; retain its domain and stop all further DMA work.
            self.device_domains[domain_index].state = DomainState::Quarantined;
            return Err(poison_dma_state());
        }
        core::sync::atomic::fence(Ordering::Release);
        let installed =
            complete_domain_install(&mut self.device_domains[domain_index].state, || {
                invalidate_all_units(&mut self.units)
            });
        if installed.is_err() {
            return Err(poison_dma_state());
        }
        info!(
            "vtd: installed requester domain id={} for {:04x}:{:02x}:{:02x}.{}",
            id, requester.segment, requester.bus, requester.device, requester.function
        );
        Ok(domain_index)
    }

    fn ensure_device_domain(&mut self, requester: tk_vtd::PciRequester) -> Result<usize, Error> {
        // A live identity lease means the requester still uses the shared boot
        // context. Never replace that context behind an active owner.
        require_no_identity_lease(&self.identity_leases, requester)?;
        if let Some(index) = self
            .device_domains
            .iter()
            .position(|domain| domain.requester == requester)
        {
            require_ready_domain(self.device_domains[index].state)?;
            return Ok(index);
        }
        self.create_device_domain(requester)
    }

    fn acquire_identity_dma(
        &mut self,
        requester: tk_vtd::PciRequester,
        initial_pages: &[u64],
    ) -> Result<u64, Error> {
        if self
            .device_domains
            .iter()
            .any(|domain| domain.requester == requester)
            || requester_has_identity_lease(&self.identity_leases, requester)
        {
            return Err(Error::InvalidStructure);
        }
        let path = dmar_path(requester);
        // Retain the exact requester-to-DRHD and scoped-RMRR proof even though
        // the live device stays on the already-installed shared DID=1 root.
        dmar_unit_index_for_requester(&self.dmar, requester, &path)?;
        let pages = identity_pages_for_requester(&self.dmar, requester, initial_pages)?;
        self.identity_leases
            .try_reserve(1)
            .map_err(|_| Error::OutOfMemory)?;
        let lease_id = take_nonreusing_id(&mut self.next_identity_lease_id)?;
        register_identity_lease(
            &mut self.identity_leases,
            requester,
            lease_id,
            &pages,
            self.identity_end,
        )?;
        Ok(lease_id)
    }

    fn map_identity_pages(
        &mut self,
        requester: tk_vtd::PciRequester,
        lease_id: u64,
        physical_pages: &[u64],
    ) -> Result<u64, Error> {
        let pages = normalize_lease_pages(physical_pages)?;
        validate_identity_pages_in_aperture(&pages, self.identity_end)?;
        let lease = self
            .identity_leases
            .iter_mut()
            .find(|lease| lease.requester == requester)
            .ok_or(Error::NoDomain)?;
        if lease.owner.lease_id != lease_id {
            return Err(Error::InvalidStructure);
        }
        let mapping_id = take_nonreusing_id(&mut self.next_identity_mapping_id)?;
        let batch_pages = prepare_identity_batch_record(&mut lease.owner, &pages)?;
        // These addresses are already identity-mapped by the retained boot
        // root. This publishes only the backing/token ownership record: it
        // deliberately does not modify PTEs, install a context, or invalidate.
        publish_identity_batch(
            &mut lease.owner,
            mapping_id,
            batch_pages,
            MappingState::Active,
        );
        Ok(mapping_id)
    }

    fn unmap_identity_pages(
        &mut self,
        requester: tk_vtd::PciRequester,
        lease_id: u64,
        mapping_id: u64,
    ) -> Result<(), Error> {
        let lease = self
            .identity_leases
            .iter_mut()
            .find(|lease| lease.requester == requester)
            .ok_or(Error::NoDomain)?;
        if lease.owner.lease_id != lease_id {
            return Err(Error::InvalidStructure);
        }
        retire_identity_batch_record(&mut lease.owner, mapping_id)
    }

    fn release_identity_dma(
        &mut self,
        requester: tk_vtd::PciRequester,
        lease_id: u64,
    ) -> Result<(), Error> {
        release_identity_lease(&mut self.identity_leases, requester, lease_id)
    }

    fn map_for(
        &mut self,
        requester: tk_vtd::PciRequester,
        physical: u64,
        length: usize,
    ) -> Result<u64, Error> {
        if length == 0 || physical.checked_add(length as u64).is_none() {
            return Err(Error::InvalidRange);
        }
        let domain_index = self.ensure_device_domain(requester)?;
        let offset = (physical & (PAGE_SIZE - 1)) as usize;
        let aligned_physical = physical & !(PAGE_SIZE - 1);
        let mapped_length = offset
            .checked_add(length)
            .and_then(|value| value.checked_add(PAGE_SIZE as usize - 1))
            .ok_or(Error::InvalidRange)?
            & !(PAGE_SIZE as usize - 1);
        let domain = self
            .device_domains
            .get_mut(domain_index)
            .ok_or(Error::NoDomain)?;
        require_ready_domain(domain.state)?;
        domain
            .mappings
            .try_reserve(1)
            .map_err(|_| Error::OutOfMemory)?;
        let iova = domain.iovas.allocate(mapped_length)?;
        if let Err(error) = dmar_map_buf_locked(
            &mut domain.page_table,
            aligned_physical,
            iova,
            mapped_length,
            DMAR_PTE_R | DMAR_PTE_W,
            4,
        ) {
            if error == Error::Quarantined {
                domain.mappings.push(Mapping {
                    device_address: iova + offset as u64,
                    length,
                    physical: aligned_physical,
                    iova,
                    mapped_length,
                    state: MappingState::Quarantined,
                });
                domain.state = DomainState::Quarantined;
                return Err(poison_dma_state());
            }
            domain.iovas.release(iova, mapped_length)?;
            return Err(error);
        }
        let device_address = iova + offset as u64;
        let mapping = Mapping {
            device_address,
            length,
            physical: aligned_physical,
            iova,
            mapped_length,
            state: MappingState::Active,
        };
        let Manager {
            units,
            device_domains,
            ..
        } = self;
        let domain = device_domains
            .get_mut(domain_index)
            .ok_or(Error::NoDomain)?;
        let result = publish_mapping_after_invalidation(&mut domain.mappings, mapping, || {
            invalidate_all_units(units)
        });
        match result {
            Ok(address) => Ok(address),
            Err(Error::Quarantined) => {
                domain.state = DomainState::Quarantined;
                Err(poison_dma_state())
            }
            Err(error) => Err(error),
        }
    }

    fn unmap_for(
        &mut self,
        requester: tk_vtd::PciRequester,
        device_address: u64,
        length: usize,
    ) -> Result<(), Error> {
        let domain_index = self.ensure_device_domain(requester)?;
        let domain = self
            .device_domains
            .get_mut(domain_index)
            .ok_or(Error::NoDomain)?;
        require_ready_domain(domain.state)?;
        let index = domain
            .mappings
            .iter()
            .position(|mapping| {
                mapping.device_address == device_address && mapping.length == length
            })
            .ok_or(Error::InvalidRange)?;
        let Manager {
            units,
            device_domains,
            ..
        } = self;
        let domain = device_domains
            .get_mut(domain_index)
            .ok_or(Error::NoDomain)?;
        let DeviceDomain {
            mappings,
            page_table,
            iovas,
            state,
            ..
        } = domain;
        let mapping = mappings.get(index).copied().ok_or(Error::InvalidRange)?;
        match retire_mapping_after_invalidation(
            mappings,
            index,
            iovas,
            || dmar_unmap_buf_locked(page_table, mapping.iova, mapping.mapped_length),
            || invalidate_all_units(units),
        ) {
            Ok(()) => Ok(()),
            Err(Error::Quarantined) => {
                *state = DomainState::Quarantined;
                Err(poison_dma_state())
            }
            Err(error) => Err(error),
        }
    }

    fn map_msi(
        &mut self,
        requester: tk_vtd::PciRequester,
        vector: u8,
        destination: u32,
    ) -> Result<Option<(u64, u32)>, Error> {
        let unit_index =
            dmar_unit_index_for_requester(&self.dmar, requester, &dmar_path(requester))?;
        let Manager {
            units, ir_routes, ..
        } = self;
        let unit = units.get_mut(unit_index).ok_or(Error::NoDevice)?;
        let Unit {
            mmio,
            qi,
            queue,
            gcmd,
            ir_table,
            ir,
            cache_flush,
            ..
        } = unit;
        let (Some(table), Some(remapper)) = (ir_table.as_ref(), ir.as_mut()) else {
            return Ok(None);
        };
        if ir_routes.iter().any(|route| route.vector == vector) {
            return Err(Error::InvalidStructure);
        }
        ir_routes.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
        let mut io = UnitInterruptIo {
            mmio: *mmio,
            qi_physical: qi.physical,
            qi_virtual: qi.virtual_address,
            queue,
            gcmd,
            ir_table_physical: table.physical,
            ir_table_virtual: table.virtual_address,
            irte_count: remapper.entry_count() as u32,
            cache_flush: *cache_flush,
        };
        let cookie = remapper.allocate_msi(&mut io, 1)?[0];
        let address =
            0xfee0_0018 | (u64::from(cookie & 0x7fff) << 5) | (u64::from(cookie & 0x8000) << 2);
        let route_index = ir_routes.len();
        ir_routes.push(IrRoute {
            requester,
            vector,
            unit_index,
            cookie,
            message_address: address,
            message_data: 0,
            state: IrRouteState::Quarantined,
        });
        match remapper.map_msi(
            &mut io,
            InterruptSource::Pci {
                unit: Some(unit_index),
                requester_id: ((u16::from(requester.bus) << 8)
                    | (u16::from(requester.device) << 3)
                    | u16::from(requester.function)),
            },
            destination,
            vector,
            cookie,
        ) {
            Ok((message_address, message_data)) => {
                let record = &mut ir_routes[route_index];
                record.message_address = message_address;
                record.message_data = message_data;
                record.state = IrRouteState::Active;
                info!(
                    "vtd: MSI vector={vector:#x} requester={:04x}:{:02x}:{:02x}.{} IRTE={cookie}",
                    requester.segment, requester.bus, requester.device, requester.function
                );
                Ok(Some((message_address, message_data)))
            }
            Err(_error) => {
                // Programming may have reached the IRTE before IEC
                // invalidation failed. Retain the allocated cookie and route
                // owner, then refuse further DMA/interrupt ownership.
                Err(poison_dma_state())
            }
        }
    }

    fn msi_vector(
        &self,
        requester: tk_vtd::PciRequester,
        message_address: u64,
        message_data: u32,
    ) -> Result<Option<u8>, Error> {
        find_owned_msi_vector(&self.ir_routes, requester, message_address, message_data)
    }

    fn unmap_msi(&mut self, vector: u8) -> Result<(), Error> {
        let Some(index) = self
            .ir_routes
            .iter()
            .position(|route| route.vector == vector)
        else {
            return Ok(());
        };
        let route = self.ir_routes[index];
        let unit = self
            .units
            .get_mut(route.unit_index)
            .ok_or(Error::NoDevice)?;
        let cache_flush = unit.cache_flush;
        let (Some(table), Some(remapper)) = (unit.ir_table.as_ref(), unit.ir.as_mut()) else {
            return Err(Error::NoDomain);
        };
        let mut io = UnitInterruptIo {
            mmio: unit.mmio,
            qi_physical: unit.qi.physical,
            qi_virtual: unit.qi.virtual_address,
            queue: &mut unit.queue,
            gcmd: &mut unit.gcmd,
            ir_table_physical: table.physical,
            ir_table_virtual: table.virtual_address,
            irte_count: remapper.entry_count() as u32,
            cache_flush,
        };
        match retire_ir_route_after_invalidation(&mut self.ir_routes, index, || {
            remapper.unmap_msi(&mut io, Some(route.cookie))
        }) {
            Ok(()) => Ok(()),
            Err(Error::Quarantined) => Err(poison_dma_state()),
            Err(error) => Err(error),
        }
    }

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
        let Manager {
            page_table,
            iovas,
            mappings,
            units,
            ..
        } = self;
        mappings.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
        let iova = iovas.allocate(mapped_length)?;
        if let Err(error) = dmar_map_buf_locked(
            page_table,
            aligned_physical,
            iova,
            mapped_length,
            DMAR_PTE_R | DMAR_PTE_W,
            4,
        ) {
            if error == Error::Quarantined {
                mappings.push(Mapping {
                    device_address: iova + offset as u64,
                    length,
                    physical: aligned_physical,
                    iova,
                    mapped_length,
                    state: MappingState::Quarantined,
                });
                return Err(poison_dma_state());
            }
            iovas.release(iova, mapped_length)?;
            return Err(error);
        }
        let device_address = iova + offset as u64;
        let mapping = Mapping {
            device_address,
            length,
            physical: aligned_physical,
            iova,
            mapped_length,
            state: MappingState::Active,
        };
        match publish_mapping_after_invalidation(mappings, mapping, || invalidate_all_units(units))
        {
            Ok(address) => Ok(address),
            Err(Error::Quarantined) => Err(poison_dma_state()),
            Err(error) => Err(error),
        }
    }

    fn unmap(&mut self, device_address: u64, length: usize) -> Result<(), Error> {
        let index = self
            .mappings
            .iter()
            .position(|mapping| {
                mapping.device_address == device_address && mapping.length == length
            })
            .ok_or(Error::InvalidRange)?;
        let Manager {
            mappings,
            page_table,
            iovas,
            units,
            ..
        } = self;
        let mapping = mappings.get(index).copied().ok_or(Error::InvalidRange)?;
        match retire_mapping_after_invalidation(
            mappings,
            index,
            iovas,
            || dmar_unmap_buf_locked(page_table, mapping.iova, mapping.mapped_length),
            || invalidate_all_units(units),
        ) {
            Ok(()) => Ok(()),
            Err(Error::Quarantined) => Err(poison_dma_state()),
            Err(error) => Err(error),
        }
    }
}

fn next_monotonic_id(counter: &AtomicU64) -> Result<u64, Error> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
        .map_err(|_| Error::OutOfMemory)
}

fn take_nonreusing_id(counter: &mut u64) -> Result<u64, Error> {
    let id = *counter;
    *counter = id.checked_add(1).ok_or(Error::OutOfMemory)?;
    Ok(id)
}

fn normalize_lease_pages(pages: &[u64]) -> Result<Vec<u64>, Error> {
    if pages.is_empty() {
        return Err(Error::InvalidRange);
    }
    let mut normalized = Vec::new();
    normalized
        .try_reserve_exact(pages.len())
        .map_err(|_| Error::OutOfMemory)?;
    for &page in pages {
        if !page.is_multiple_of(PAGE_SIZE) || page >= 1 << 48 {
            return Err(Error::InvalidRange);
        }
        normalized.push(page);
    }
    normalized.sort_unstable();
    normalized.dedup();
    Ok(normalized)
}

fn validate_identity_pages_in_aperture(pages: &[u64], identity_end: u64) -> Result<(), Error> {
    if pages.is_empty()
        || pages.iter().any(|page| {
            !page.is_multiple_of(PAGE_SIZE)
                || page
                    .checked_add(PAGE_SIZE)
                    .is_none_or(|end| end > identity_end)
        })
    {
        return Err(Error::InvalidRange);
    }
    Ok(())
}

fn requester_has_identity_lease(
    leases: &[RequesterIdentityLease],
    requester: tk_vtd::PciRequester,
) -> bool {
    leases.iter().any(|lease| lease.requester == requester)
}

fn require_no_identity_lease(
    leases: &[RequesterIdentityLease],
    requester: tk_vtd::PciRequester,
) -> Result<(), Error> {
    if requester_has_identity_lease(leases, requester) {
        Err(Error::InvalidStructure)
    } else {
        Ok(())
    }
}

fn register_identity_lease(
    leases: &mut Vec<RequesterIdentityLease>,
    requester: tk_vtd::PciRequester,
    lease_id: u64,
    pages: &[u64],
    identity_end: u64,
) -> Result<(), Error> {
    if requester_has_identity_lease(leases, requester) {
        return Err(Error::InvalidStructure);
    }
    let pages = normalize_lease_pages(pages)?;
    validate_identity_pages_in_aperture(&pages, identity_end)?;
    let mut owned_pages = Vec::new();
    owned_pages
        .try_reserve_exact(pages.len())
        .map_err(|_| Error::OutOfMemory)?;
    owned_pages.extend(pages.into_iter().map(|physical| IdentityPage {
        physical,
        references: 0,
        permanent: true,
    }));
    leases.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
    leases.push(RequesterIdentityLease {
        requester,
        owner: IdentityDmaOwner {
            lease_id,
            pages: owned_pages,
            batches: Vec::new(),
        },
    });
    Ok(())
}

fn release_identity_lease(
    leases: &mut Vec<RequesterIdentityLease>,
    requester: tk_vtd::PciRequester,
    lease_id: u64,
) -> Result<(), Error> {
    let index = leases
        .iter()
        .position(|lease| lease.requester == requester)
        .ok_or(Error::NoDomain)?;
    let lease = &leases[index];
    if lease.owner.lease_id != lease_id {
        return Err(Error::InvalidStructure);
    }
    if !lease.owner.batches.is_empty() || lease.owner.pages.iter().any(|page| page.references != 0)
    {
        return Err(Error::InvalidStructure);
    }
    leases.swap_remove(index);
    Ok(())
}

fn direct_identity_acquire(
    requester: tk_vtd::PciRequester,
    initial_pages: &[u64],
) -> Result<u64, Error> {
    let initial_pages = normalize_lease_pages(initial_pages)?;
    let mut leases = DIRECT_IDENTITY_LEASES.lock();
    if leases.iter().any(|lease| lease.requester == requester) {
        return Err(Error::InvalidStructure);
    }
    leases.try_reserve(1).map_err(|_| Error::OutOfMemory)?;
    let id = next_monotonic_id(&NEXT_DIRECT_LEASE_ID)?;
    leases.push(DirectIdentityLease {
        requester,
        id,
        initial_pages,
        batches: Vec::new(),
    });
    Ok(id)
}

fn direct_identity_map(
    requester: tk_vtd::PciRequester,
    lease_id: u64,
    pages: &[u64],
) -> Result<u64, Error> {
    let pages = normalize_lease_pages(pages)?;
    let mut leases = DIRECT_IDENTITY_LEASES.lock();
    let lease = leases
        .iter_mut()
        .find(|lease| lease.requester == requester && lease.id == lease_id)
        .ok_or(Error::NoDomain)?;
    lease
        .batches
        .try_reserve(1)
        .map_err(|_| Error::OutOfMemory)?;
    let id = next_monotonic_id(&NEXT_DIRECT_MAPPING_ID)?;
    lease.batches.push(DirectIdentityBatch { id, pages });
    Ok(id)
}

fn direct_identity_unmap(
    requester: tk_vtd::PciRequester,
    lease_id: u64,
    mapping_id: u64,
) -> Result<(), Error> {
    let mut leases = DIRECT_IDENTITY_LEASES.lock();
    let lease = leases
        .iter_mut()
        .find(|lease| lease.requester == requester && lease.id == lease_id)
        .ok_or(Error::NoDomain)?;
    let index = lease
        .batches
        .iter()
        .position(|batch| batch.id == mapping_id)
        .ok_or(Error::InvalidRange)?;
    if lease.batches[index].pages.is_empty() {
        return Err(Error::InvalidStructure);
    }
    lease.batches.swap_remove(index);
    Ok(())
}

fn direct_identity_release(requester: tk_vtd::PciRequester, lease_id: u64) -> Result<(), Error> {
    let mut leases = DIRECT_IDENTITY_LEASES.lock();
    let index = leases
        .iter()
        .position(|lease| lease.requester == requester && lease.id == lease_id)
        .ok_or(Error::NoDomain)?;
    if leases[index].initial_pages.is_empty() || !leases[index].batches.is_empty() {
        return Err(Error::InvalidStructure);
    }
    leases.swap_remove(index);
    Ok(())
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

/// Keep every table and queue allocation alive when a partially enabled unit
/// may still fetch them. MODE_FAILED prevents further PCI admission; leaking
/// is safer than allowing Rust drop glue to release a hardware-visible page.
fn quarantine_boot_resources<P, R, C, U>(page_table: P, root: R, contexts: C, units: U) {
    core::mem::forget((page_table, root, contexts, units));
}

fn release_boot_resources(
    page_table: SecondLevel<KernelPageMemory>,
    root: DmaBlock,
    contexts: Vec<DmaBlock>,
    units: Vec<Unit>,
) {
    // Dropping the page-table owner frees its page-table pages; DmaBlock owns
    // allocator release for root, context, and per-unit queue pages.
    drop(page_table);
    drop(root);
    drop(contexts);
    drop(units);
}

/// Discover DMAR before PCI probing; translate by default when a supported DMAR exists.
///
/// A failure before the first unit is enabled has changed no hardware state,
/// so it falls back to identity DMA, as `intel_iommu=off` would, instead of
/// refusing every PCI device. A failure after a unit may have been enabled
/// still fails closed.
pub(super) fn init(engine: &Engine) -> Result<(), Error> {
    let mut hardware_touched = false;
    let mut identity_fallback_safe = false;
    DMA_POISONED.store(false, Ordering::Release);
    *FAILURE.lock() = None;
    let result = init_translation(engine, &mut hardware_touched, &mut identity_fallback_safe);
    if let Err(error) = result
        && FAILURE.lock().is_none()
    {
        *FAILURE.lock() = Some(InitFailure::global(error, "DMAR discovery/setup"));
    }
    if let Err(error) = result
        && identity_fallback_safe
        && !hardware_touched
    {
        MODE.store(MODE_IDENTITY, Ordering::Release);
        warn!(
            "vtd: initialization failed before any unit was enabled ({error:?}); admitting \
             identity DMA"
        );
    }
    result
}

fn init_translation(
    engine: &Engine,
    hardware_touched: &mut bool,
    identity_fallback_safe: &mut bool,
) -> Result<(), Error> {
    // Explicitly disabled translation is only safe if every described unit
    // is confirmed inactive. Unknown firmware state must not authorize a GPU
    // identity-DMA lease.
    if axhal::boot::command_line_value("intel_iommu") == Some("off") {
        MODE.store(MODE_FAILED, Ordering::Release);
        let Some(dmar) = table(engine)? else {
            MODE.store(MODE_IDENTITY, Ordering::Release);
            *identity_fallback_safe = true;
            info!("vtd: intel_iommu=off with no DMAR table; using direct DMA");
            return Ok(());
        };
        if dmar.units.is_empty() {
            return Err(Error::InvalidStructure);
        }
        let mut probes = Vec::new();
        probes
            .try_reserve_exact(dmar.units.len())
            .map_err(|_| Error::OutOfMemory)?;
        for record in &dmar.units {
            probes.push(probe_unit(record.register_base)?);
        }
        *identity_fallback_safe =
            identity_fallback_safe_before_enable(probes.iter().map(|probe| probe.gsts));
        if !*identity_fallback_safe {
            let (index, probe) = probes
                .iter()
                .enumerate()
                .find(|(_, probe)| probe.gsts & GSTS_ACTIVE_MASK != 0)
                .ok_or(Error::InvalidStructure)?;
            *FAILURE.lock() = Some(InitFailure {
                error: Error::Unsupported,
                stage: "intel_iommu=off but firmware left DRHD active",
                unit_index: Some(index),
                register_base: Some(probe.register_base),
                cap: Some(probe.cap),
                ecap: Some(probe.ecap),
                gsts: Some(probe.gsts),
                fsts: Some(probe.fsts),
            });
            error!(
                "vtd: intel_iommu=off refused; DRHD {index} at {:#x} already active, GSTS={:#x} \
                 FSTS={:#x}",
                probe.register_base, probe.gsts, probe.fsts
            );
            return Err(Error::Unsupported);
        }
        MODE.store(MODE_IDENTITY, Ordering::Release);
        info!("vtd: intel_iommu=off after read-only inactive-unit preflight");
        return Ok(());
    }
    MODE.store(MODE_FAILED, Ordering::Release);
    info!("vtd: parsing ACPI DMAR for default translation");
    let Some(dmar) = table(engine)? else {
        MODE.store(MODE_IDENTITY, Ordering::Release);
        *identity_fallback_safe = true;
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
    let intremap = axhal::boot::command_line_value("intremap") == Some("on");
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

    // Read every unit's capabilities and handoff state before allocating
    // translation structures or issuing any command. A capability mismatch
    // on a later DRHD must not be discovered only after an earlier unit has
    // already been switched to our root table.
    let mut probes = Vec::new();
    probes
        .try_reserve_exact(dmar.units.len())
        .map_err(|_| Error::OutOfMemory)?;
    for record in &dmar.units {
        probes.push(probe_unit(record.register_base)?);
    }
    *identity_fallback_safe =
        identity_fallback_safe_before_enable(probes.iter().map(|probe| probe.gsts));

    if maximum == 0 || maximum > (1 << 48) {
        return Err(Error::InvalidRange);
    }
    if intremap && !dmar.interrupt_remapping {
        *FAILURE.lock() = Some(InitFailure::global(
            Error::Unsupported,
            "DMAR interrupt-remapping flag",
        ));
        return Err(Error::Unsupported);
    }
    if let Err((index, error)) =
        validate_all_unit_capabilities(&probes, maximum, dmar.host_address_width, intremap)
    {
        let probe = &probes[index];
        *FAILURE.lock() = Some(InitFailure {
            error,
            stage: "DRHD capability/address-width preflight",
            unit_index: Some(index),
            register_base: Some(probe.register_base),
            cap: Some(probe.cap),
            ecap: Some(probe.ecap),
            gsts: Some(probe.gsts),
            fsts: Some(probe.fsts),
        });
        error!(
            "vtd: preflight rejected DRHD {index} at {:#x}: {error:?} CAP={:#x} ECAP={:#x} \
             GSTS={:#x} FSTS={:#x}",
            probe.register_base, probe.cap, probe.ecap, probe.gsts, probe.fsts
        );
        return Err(error);
    }

    let cache_flush = match cache_flush_for_probes(&probes) {
        Ok(cache_flush) => cache_flush,
        Err(error) => {
            *FAILURE.lock() = Some(InitFailure::global(
                error,
                "non-coherent DRHD but CPU lacks CLFLUSH",
            ));
            error!("vtd: refusing non-coherent DRHD; CPU does not advertise a usable CLFLUSH path");
            return Err(error);
        }
    };

    let mut page_table = SecondLevel::new(KernelPageMemory {
        pages: Vec::new(),
        cache_flush,
    })?;
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

    let iova_start = maximum
        .max(1 << 30)
        .checked_add((1 << 30) - 1)
        .ok_or(Error::InvalidRange)?
        & !((1 << 30) - 1);
    let iovas = IovaAllocator::new(iova_start, IOVA_END)?;

    let mut units = Vec::new();
    units
        .try_reserve_exact(dmar.units.len())
        .map_err(|_| Error::OutOfMemory)?;
    for probe in &probes {
        let qi = allocate_dma(QI_PAGES)?;
        let unit = Unit {
            mmio: probe.mmio,
            register_base: probe.register_base,
            cache_flush,
            domain_count: cap_domain_count(probe.cap),
            next_domain_id: 2,
            qi,
            queue: QiQueue::new(QI_BYTES as u32)?,
            gcmd: 0,
            root_physical: 0,
            ir_table: None,
            ir: None,
        };
        units.push(unit);
    }
    // The root and context pages were prepared in cached RAM before any unit
    // was enabled. Flush each non-coherent unit's view before loading RTADDR.
    for unit in &mut units {
        utils::dmar_flush_transl_to_ram(
            unit,
            root.virtual_address.as_ptr() as usize,
            PAGE_SIZE as usize,
        )?;
        for context in &context_tables {
            utils::dmar_flush_transl_to_ram(
                unit,
                context.virtual_address.as_ptr() as usize,
                PAGE_SIZE as usize,
            )?;
        }
    }
    // Complete every fallible allocation before enabling the first unit. If a
    // later unit fails after an earlier unit enabled TE, quarantine all of the
    // pages/queues rather than dropping backing memory still visible to DMA.
    let enable_error = units.iter_mut().enumerate().find_map(|(index, unit)| {
        unit.enable(root.physical, hardware_touched)
            .err()
            .map(|failure| (index, failure))
    });
    if let Some((index, (stage, error))) = enable_error {
        let register_base = dmar.units[index].register_base;
        let unit = &units[index];
        let (cap, ecap, gsts, fsts) = (
            read64(unit, CAP),
            read64(unit, ECAP),
            read32(unit, GSTS),
            read32(unit, FSTS),
        );
        *FAILURE.lock() = Some(InitFailure {
            error,
            stage,
            unit_index: Some(index),
            register_base: Some(register_base),
            cap: Some(cap),
            ecap: Some(ecap),
            gsts: Some(gsts),
            fsts: Some(fsts),
        });
        error!(
            "vtd: DRHD {index} at {register_base:#x} failed during {stage}: {error:?} \
             CAP={cap:#x} ECAP={ecap:#x} GSTS={gsts:#x} FSTS={fsts:#x}; quarantining DMA tables"
        );
        if *hardware_touched {
            quarantine_boot_resources(page_table, root, context_tables, units);
        } else {
            release_boot_resources(page_table, root, context_tables, units);
        }
        return Err(error);
    }
    let manager = Manager {
        dmar: dmar.clone(),
        units,
        cache_flush,
        root_table: root,
        context_tables,
        page_table,
        iovas,
        mappings: Vec::new(),
        ir_routes: Vec::new(),
        identity_end: maximum,
        iova_start,
        device_domains: Vec::new(),
        identity_leases: Vec::new(),
        next_identity_lease_id: 1,
        next_identity_mapping_id: 1,
    };
    *MANAGER.lock() = Some(manager);
    let requester_domains = match axhal::boot::command_line_value("iommu_domains") {
        None | Some("on") => true,
        Some("off") => false,
        Some(value) => {
            warn!("vtd: ignoring invalid iommu_domains={value:?}; using shared DMA context");
            false
        }
    };
    REQUESTER_DOMAINS.store(requester_domains, Ordering::Release);
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
        let mode = MODE.load(Ordering::Acquire);
        let allowed = pci_dma_allowed_in_mode(mode);
        if !allowed {
            if mode == MODE_UNKNOWN {
                error!("vtd: PCI DMA refused; no safe firmware/VT-d handoff state was established");
            } else {
                error!("vtd: PCI DMA refused; VT-d failure: {:?}", *FAILURE.lock());
            }
        }
        allowed
    }
    fn map(physical: u64, length: usize) -> Result<u64, Error> {
        let mode = MODE.load(Ordering::Acquire);
        if identity_dma_in_mode(mode) {
            if length != 0 && physical.checked_add(length as u64).is_some() {
                Ok(physical)
            } else {
                Err(Error::InvalidRange)
            }
        } else if mode == MODE_ENABLED {
            MANAGER
                .lock()
                .as_mut()
                .ok_or(Error::NoDomain)?
                .map(physical, length)
        } else {
            Err(failed_dma_error())
        }
    }
    fn unmap(device_address: u64, length: usize) -> Result<(), Error> {
        let mode = MODE.load(Ordering::Acquire);
        if identity_dma_in_mode(mode) {
            Ok(())
        } else if mode == MODE_ENABLED {
            MANAGER
                .lock()
                .as_mut()
                .ok_or(Error::NoDomain)?
                .unmap(device_address, length)
        } else {
            Err(failed_dma_error())
        }
    }

    fn map_for(
        requester: tk_vtd::PciRequester,
        physical: u64,
        length: usize,
    ) -> Result<u64, Error> {
        let mode = MODE.load(Ordering::Acquire);
        let result = if identity_dma_in_mode(mode) {
            if length != 0 && physical.checked_add(length as u64).is_some() {
                Ok(physical)
            } else {
                Err(Error::InvalidRange)
            }
        } else if mode == MODE_ENABLED {
            let mut manager = MANAGER.lock();
            let manager = manager.as_mut().ok_or(Error::NoDomain)?;
            if REQUESTER_DOMAINS.load(Ordering::Acquire) {
                manager.map_for(requester, physical, length)
            } else {
                manager.map(physical, length)
            }
        } else {
            Err(failed_dma_error())
        };
        if let Err(error) = result {
            let message = alloc::format!(
                "THEKERNEL_VTD_REQUESTER_MAP_FAILED requester={:04x}:{:02x}:{:02x}.{} \
                 physical={:#x} len={:#x} error={:?}\n",
                requester.segment,
                requester.bus,
                requester.device,
                requester.function,
                physical,
                length,
                error,
            );
            let _ = axhal::console::try_write_diagnostic_bytes(message.as_bytes());
        }
        result
    }

    fn unmap_for(
        requester: tk_vtd::PciRequester,
        device_address: u64,
        length: usize,
    ) -> Result<(), Error> {
        let mode = MODE.load(Ordering::Acquire);
        if identity_dma_in_mode(mode) {
            Ok(())
        } else if mode == MODE_ENABLED {
            let mut manager = MANAGER.lock();
            let manager = manager.as_mut().ok_or(Error::NoDomain)?;
            if REQUESTER_DOMAINS.load(Ordering::Acquire) {
                manager.unmap_for(requester, device_address, length)
            } else {
                manager.unmap(device_address, length)
            }
        } else {
            Err(failed_dma_error())
        }
    }
}

struct PlatformInterruptRemap;
#[crate_interface::impl_interface]
impl tk_vtd::PlatformInterruptRemap for PlatformInterruptRemap {
    fn map_msi(
        requester: tk_vtd::PciRequester,
        vector: u8,
        destination: u32,
    ) -> Result<Option<(u64, u32)>, Error> {
        if axhal::boot::command_line_value("intremap") != Some("on") {
            return Ok(None);
        }
        match MODE.load(Ordering::Acquire) {
            MODE_UNKNOWN | MODE_IDENTITY => return Ok(None),
            MODE_FAILED => return Err(failed_dma_error()),
            MODE_ENABLED => {}
            _ => return Err(Error::NoDomain),
        }
        MANAGER
            .lock()
            .as_mut()
            .ok_or(Error::NoDomain)?
            .map_msi(requester, vector, destination)
    }

    fn unmap_msi(vector: u8) -> Result<(), Error> {
        match MODE.load(Ordering::Acquire) {
            MODE_UNKNOWN | MODE_IDENTITY => return Ok(()),
            MODE_FAILED => return Err(failed_dma_error()),
            MODE_ENABLED => {}
            _ => return Err(Error::NoDomain),
        }
        MANAGER
            .lock()
            .as_mut()
            .ok_or(Error::NoDomain)?
            .unmap_msi(vector)
    }

    fn msi_vector(
        requester: tk_vtd::PciRequester,
        message_address: u64,
        message_data: u32,
    ) -> Result<Option<u8>, Error> {
        match MODE.load(Ordering::Acquire) {
            MODE_UNKNOWN | MODE_IDENTITY => return Ok(None),
            MODE_FAILED => return Err(failed_dma_error()),
            MODE_ENABLED => {}
            _ => return Err(Error::NoDomain),
        }
        MANAGER.lock().as_ref().ok_or(Error::NoDomain)?.msi_vector(
            requester,
            message_address,
            message_data,
        )
    }
}

struct PlatformIdentityDma;
#[crate_interface::impl_interface]
impl tk_vtd::PlatformIdentityDma for PlatformIdentityDma {
    fn acquire_identity_dma(
        requester: tk_vtd::PciRequester,
        initial_pages: &[u64],
    ) -> Result<u64, Error> {
        let mode = MODE.load(Ordering::Acquire);
        if direct_identity_lease_allowed(mode) {
            return direct_identity_acquire(requester, initial_pages);
        }
        match mode {
            MODE_UNKNOWN => Err(Error::NoDomain),
            MODE_FAILED => Err(failed_dma_error()),
            MODE_ENABLED => MANAGER
                .lock()
                .as_mut()
                .ok_or(Error::NoDomain)?
                .acquire_identity_dma(requester, initial_pages),
            _ => Err(Error::NoDomain),
        }
    }

    fn map_identity_pages(
        requester: tk_vtd::PciRequester,
        lease_id: u64,
        pages: &[u64],
    ) -> Result<u64, Error> {
        match MODE.load(Ordering::Acquire) {
            MODE_UNKNOWN => Err(Error::NoDomain),
            MODE_IDENTITY => direct_identity_map(requester, lease_id, pages),
            MODE_FAILED => Err(failed_dma_error()),
            MODE_ENABLED => MANAGER
                .lock()
                .as_mut()
                .ok_or(Error::NoDomain)?
                .map_identity_pages(requester, lease_id, pages),
            _ => Err(Error::NoDomain),
        }
    }

    fn unmap_identity_pages(
        requester: tk_vtd::PciRequester,
        lease_id: u64,
        mapping_id: u64,
    ) -> Result<(), Error> {
        match MODE.load(Ordering::Acquire) {
            MODE_UNKNOWN => Err(Error::NoDomain),
            MODE_IDENTITY => direct_identity_unmap(requester, lease_id, mapping_id),
            MODE_FAILED => Err(failed_dma_error()),
            MODE_ENABLED => MANAGER
                .lock()
                .as_mut()
                .ok_or(Error::NoDomain)?
                .unmap_identity_pages(requester, lease_id, mapping_id),
            _ => Err(Error::NoDomain),
        }
    }

    fn release_identity_dma(requester: tk_vtd::PciRequester, lease_id: u64) -> Result<(), Error> {
        match MODE.load(Ordering::Acquire) {
            MODE_UNKNOWN => Err(Error::NoDomain),
            MODE_IDENTITY => direct_identity_release(requester, lease_id),
            MODE_FAILED => Err(failed_dma_error()),
            MODE_ENABLED => MANAGER
                .lock()
                .as_mut()
                .ok_or(Error::NoDomain)?
                .release_identity_dma(requester, lease_id),
            _ => Err(Error::NoDomain),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::{
        cell::RefCell,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use tk_vtd::{
        iova::IovaAllocator,
        reg::{DMAR_FECTL_IP, DMAR_FRCD2_F},
    };

    use super::{
        DMAR_CAP_SAGAW_4LVL, DMAR_CAP_SPS_2M, DMAR_ECAP_C, DMAR_ECAP_EIM, DMAR_ECAP_IR,
        DMAR_ECAP_QI, DMAR_FECTL_IM, DomainState, Error, IdentityDmaOwner, IdentityPage, IrRoute,
        IrRouteState, KernelPageMemory, MODE_ENABLED, MODE_FAILED, MODE_IDENTITY, MODE_UNKNOWN,
        Mapping, MappingState, PageMemory, RequesterIdentityLease, UnitProbe, cap_domain_count,
        complete_domain_install, direct_identity_acquire, direct_identity_lease_allowed,
        direct_identity_map, direct_identity_release, direct_identity_unmap,
        drain_primary_fault_records, fault_event_mask_write, find_owned_msi_vector,
        identity_dma_in_mode, identity_fallback_safe_before_enable, identity_pages_for_path,
        pci_dma_allowed_in_mode, prepare_identity_batch_record, publish_identity_batch,
        publish_mapping_after_invalidation, quarantine_boot_resources, register_identity_lease,
        release_identity_lease, require_no_identity_lease, require_ready_domain,
        retire_identity_batch_record, retire_ir_route_after_invalidation,
        retire_mapping_after_invalidation, store_irte_words, take_domain_id, take_nonreusing_id,
        validate_all_unit_capabilities, validate_identity_pages_in_aperture,
        validate_unit_capabilities,
    };

    struct DropProbe(Arc<AtomicUsize>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn failed_partial_enable_quarantines_every_dma_owner() {
        let drops = Arc::new(AtomicUsize::new(0));
        quarantine_boot_resources(
            DropProbe(drops.clone()),
            DropProbe(drops.clone()),
            DropProbe(drops.clone()),
            DropProbe(drops.clone()),
        );
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn unknown_firmware_state_denies_pci_dma_until_safe_mode_is_established() {
        assert!(!pci_dma_allowed_in_mode(MODE_UNKNOWN));
        assert!(pci_dma_allowed_in_mode(MODE_IDENTITY));
        assert!(pci_dma_allowed_in_mode(MODE_ENABLED));
        assert!(!pci_dma_allowed_in_mode(MODE_FAILED));
        assert!(!identity_dma_in_mode(MODE_UNKNOWN));
        assert!(identity_dma_in_mode(MODE_IDENTITY));
        assert!(!identity_dma_in_mode(MODE_ENABLED));
        assert!(!identity_dma_in_mode(MODE_FAILED));
        assert!(!direct_identity_lease_allowed(MODE_UNKNOWN));
        assert!(direct_identity_lease_allowed(MODE_IDENTITY));
        assert!(!direct_identity_lease_allowed(MODE_ENABLED));
        assert!(!direct_identity_lease_allowed(MODE_FAILED));
    }

    #[test]
    fn unowned_fault_event_msi_is_masked_without_acknowledging_pending_state() {
        let mask = DMAR_FECTL_IM as u32;
        let pending = DMAR_FECTL_IP as u32;

        assert_eq!(fault_event_mask_write(0), Some(mask));
        assert_eq!(fault_event_mask_write(pending), Some(mask));
        assert_eq!(fault_event_mask_write(mask), None);
        assert_eq!(fault_event_mask_write(mask | pending), None);
        assert_eq!(fault_event_mask_write(pending).unwrap() & pending, 0);
    }

    #[test]
    fn primary_fault_drain_decodes_frcd_words_and_releases_wrapped_slots() {
        const FRO: usize = 0x80;
        let records = RefCell::new(vec![0u64; super::MMIO_BYTES / 8]);
        let cap = ((3u64) << 40) | ((FRO as u64) << 24); // four records
        let start = FRO * 16;
        let record = |index: usize, address: u64, sid: u16, reason: u8| {
            let mut words = records.borrow_mut();
            words[(start + index * 16) / 8] = address;
            words[(start + index * 16 + 8) / 8] =
                DMAR_FRCD2_F | (u64::from(reason) << 32) | u64::from(sid);
        };
        record(3, 0x1234_5678_9abc_deff, 0x4321, 0x2a);
        record(0, 0x0fed_cba9_8765_4321, 0x1234, 0x17);
        let status = super::FSTS_PPF | (3 << 8);
        let mut cleared = Vec::new();
        let mut reported = Vec::new();

        let drained = drain_primary_fault_records(
            cap,
            status,
            |offset| records.borrow()[offset / 8],
            |offset, value| {
                cleared.push(offset);
                let mut words = records.borrow_mut();
                words[offset / 8] &= !(u64::from(value) << 32);
            },
            |sid, reason, address, index| reported.push((sid, reason, address, index)),
        )
        .unwrap();

        assert_eq!(drained, 2);
        assert_eq!(cleared, [start + 3 * 16 + 12, start + 12]);
        assert_eq!(
            reported,
            [
                (0x4321, 0x2a, 0x1234_5678_9abc_d000, 3),
                (0x1234, 0x17, 0x0fed_cba9_8765_4000, 0),
            ]
        );
        let words = records.borrow();
        assert_eq!(words[(start + 3 * 16 + 8) / 8] & DMAR_FRCD2_F, 0);
        assert_eq!(words[(start + 8) / 8] & DMAR_FRCD2_F, 0);
    }

    #[test]
    fn primary_fault_drain_rejects_out_of_range_fri_without_clearing_records() {
        let records = vec![0u64; super::MMIO_BYTES / 8];
        let cap = (3u64 << 40) | (0x80u64 << 24); // four records
        let status = super::FSTS_PPF | (4 << 8);
        let mut cleared = false;
        assert_eq!(
            drain_primary_fault_records(
                cap,
                status,
                |offset| records[offset / 8],
                |_, _| cleared = true,
                |_, _, _, _| {},
            ),
            Err(Error::InvalidStructure)
        );
        assert!(!cleared);
    }

    #[test]
    fn all_drdhs_are_checked_for_host_and_unit_address_widths() {
        let cap = (38 << 16) | (DMAR_CAP_SAGAW_4LVL << 8) | (DMAR_CAP_SPS_2M << 34);
        let ecap = DMAR_ECAP_C | DMAR_ECAP_QI | DMAR_ECAP_IR | DMAR_ECAP_EIM;
        assert_eq!(
            validate_unit_capabilities(cap, ecap, 1 << 38, 39, false),
            Ok(())
        );
        assert_eq!(
            validate_unit_capabilities(cap, ecap, 1 << 39, 39, false),
            Ok(())
        );
        assert_eq!(
            validate_unit_capabilities(cap, ecap, (1 << 39) + 4096, 39, false),
            Err(Error::InvalidRange)
        );
        assert_eq!(
            validate_unit_capabilities(cap, ecap, 1 << 38, 38, false),
            Ok(())
        );
        assert_eq!(
            validate_unit_capabilities(cap, ecap, (1 << 38) + 4096, 38, false),
            Err(Error::InvalidRange)
        );
    }

    #[test]
    fn kernel_page_memory_refuses_unowned_table_pages() {
        let mut memory = KernelPageMemory {
            pages: Vec::new(),
            cache_flush: None,
        };
        assert!(memory.page_mut(0x1000).is_none());
        assert!(memory.page_mut(0x1001).is_none());
    }

    #[test]
    fn requester_domain_ids_respect_selected_drhd_cap_nd_capacity() {
        assert_eq!(cap_domain_count(0), 16);
        assert_eq!(cap_domain_count(1), 64);
        assert_eq!(cap_domain_count(7), 1 << 18);
        let mut first_unit_next_did = 2;
        let mut second_unit_next_did = 2;
        assert_eq!(take_domain_id(&mut first_unit_next_did, 16), Ok(2));
        assert_eq!(take_domain_id(&mut second_unit_next_did, 16), Ok(2));
        for expected in 3..16 {
            assert_eq!(take_domain_id(&mut first_unit_next_did, 16), Ok(expected));
        }
        assert_eq!(
            take_domain_id(&mut first_unit_next_did, 16),
            Err(Error::OutOfMemory)
        );
        assert_eq!(first_unit_next_did, 16);
        assert_eq!(second_unit_next_did, 3);
        let mut high_capacity_next_did = u32::from(u16::MAX);
        assert_eq!(
            take_domain_id(&mut high_capacity_next_did, 1 << 18),
            Ok(u16::MAX)
        );
        assert_eq!(high_capacity_next_did, 1 << 16);
        assert_eq!(
            take_domain_id(&mut high_capacity_next_did, 1 << 18),
            Err(Error::OutOfMemory)
        );
    }

    #[test]
    fn register_invalidation_supports_dma_without_qi_but_not_interrupt_remapping() {
        let cap = (38 << 16) | (DMAR_CAP_SAGAW_4LVL << 8) | (DMAR_CAP_SPS_2M << 34);
        assert_eq!(
            validate_unit_capabilities(cap, 0, 1 << 38, 39, false),
            Ok(())
        );
        assert_eq!(
            validate_unit_capabilities(cap, 0, 1 << 38, 39, true),
            Err(Error::Unsupported)
        );
    }

    #[test]
    fn cache_maintenance_is_required_if_any_unit_is_noncoherent() {
        let mut probes = [UnitProbe {
            mmio: 0,
            register_base: 0x1000,
            cap: 0,
            ecap: DMAR_ECAP_C,
            gsts: 0,
            fsts: 0,
        }];
        assert!(!super::cache_flush_required(&probes));
        probes[0].ecap = 0;
        assert!(super::cache_flush_required(&probes));
    }

    #[test]
    fn all_drhd_capabilities_are_validated_before_any_unit_is_enabled() {
        let cap = (38 << 16) | (DMAR_CAP_SAGAW_4LVL << 8) | (DMAR_CAP_SPS_2M << 34);
        let ecap = DMAR_ECAP_C | DMAR_ECAP_QI | DMAR_ECAP_IR | DMAR_ECAP_EIM;
        let probes = [
            UnitProbe {
                mmio: 0,
                register_base: 0x1000,
                cap,
                ecap,
                gsts: 0,
                fsts: 0,
            },
            UnitProbe {
                mmio: 0,
                register_base: 0x2000,
                cap,
                ecap: 0,
                gsts: 0,
                fsts: 0,
            },
        ];

        assert_eq!(
            validate_all_unit_capabilities(&probes, 1 << 38, 39, true),
            Err((1, Error::Unsupported))
        );
    }

    #[test]
    fn firmware_enabled_units_are_never_classified_as_identity_safe() {
        assert!(identity_fallback_safe_before_enable([0, 0]));
        assert!(!identity_fallback_safe_before_enable([0, super::GSTS_TES]));
        assert!(!identity_fallback_safe_before_enable([0, super::GSTS_QIES]));
        assert!(!identity_fallback_safe_before_enable([0, super::GSTS_IRES]));
    }

    #[test]
    fn failed_context_invalidation_poisons_domain_and_retry_cannot_skip_it() {
        let mut state = DomainState::Installing;
        assert_eq!(
            complete_domain_install(&mut state, || Err(Error::Timeout)),
            Err(Error::Quarantined)
        );
        assert_eq!(state, DomainState::Quarantined);
        assert_eq!(require_ready_domain(state), Err(Error::Quarantined));
        let mut ready = DomainState::Installing;
        assert_eq!(complete_domain_install(&mut ready, || Ok(())), Ok(()));
        assert_eq!(require_ready_domain(ready), Ok(()));
    }

    fn mapping(iova: u64) -> Mapping {
        Mapping {
            device_address: iova,
            length: 4096,
            physical: 0x8000,
            iova,
            mapped_length: 4096,
            state: MappingState::Active,
        }
    }

    #[test]
    fn failed_map_invalidation_retains_quarantined_mapping_record() {
        let mut mappings = Vec::new();
        let expected = mapping(0x4000);
        assert_eq!(
            publish_mapping_after_invalidation(&mut mappings, expected, || Err(Error::Timeout)),
            Err(Error::Quarantined)
        );
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].physical, expected.physical);
        assert_eq!(mappings[0].state, MappingState::Quarantined);
    }

    #[test]
    fn failed_unmap_invalidation_keeps_backing_record_and_iova_reserved() {
        let mut iovas = IovaAllocator::new(0x1000, 0x10_000).unwrap();
        let iova = iovas.allocate(4096).unwrap();
        let mut mappings = vec![mapping(iova)];
        let mut pte_cleared = false;
        assert_eq!(
            retire_mapping_after_invalidation(
                &mut mappings,
                0,
                &mut iovas,
                || {
                    pte_cleared = true;
                    Ok(())
                },
                || Err(Error::Timeout),
            ),
            Err(Error::Quarantined)
        );
        assert!(pte_cleared);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].physical, 0x8000);
        assert_eq!(mappings[0].state, MappingState::Quarantined);
        assert_ne!(iovas.allocate(4096).unwrap(), iova);
    }

    fn test_requester() -> tk_vtd::PciRequester {
        tk_vtd::PciRequester {
            segment: 0,
            bus: 0,
            device: 2,
            function: 0,
        }
    }

    fn test_ir_route() -> IrRoute {
        IrRoute {
            requester: test_requester(),
            vector: 0xee,
            unit_index: 0,
            cookie: 7,
            message_address: 0xfee0_00f8,
            message_data: 0,
            state: IrRouteState::Active,
        }
    }

    #[test]
    fn remapped_msi_query_requires_exact_requester_address_and_data() {
        let route = test_ir_route();
        assert_eq!(
            find_owned_msi_vector(
                &[route],
                route.requester,
                route.message_address,
                route.message_data,
            ),
            Ok(Some(route.vector))
        );
        assert_eq!(
            find_owned_msi_vector(&[route], route.requester, route.message_address, 0xee,),
            Ok(None)
        );
        assert_eq!(
            find_owned_msi_vector(
                &[route],
                tk_vtd::PciRequester {
                    device: 3,
                    ..route.requester
                },
                route.message_address,
                route.message_data,
            ),
            Ok(None)
        );
        assert_eq!(
            find_owned_msi_vector(
                &[route],
                route.requester,
                route.message_address ^ 0x20,
                route.message_data,
            ),
            Ok(None)
        );
    }

    #[test]
    fn irte_store_publishes_high_qualifier_before_present_low_word() {
        let mut writes = Vec::new();
        store_irte_words(|word, value| writes.push((word, value)), 0x1111, 0x2222);
        assert_eq!(writes, [(1, 0x2222), (0, 0x1111)]);
    }

    #[test]
    fn failed_ir_invalidation_keeps_route_quarantined_until_successful_retirement() {
        let mut routes = alloc::vec![test_ir_route()];
        assert_eq!(
            retire_ir_route_after_invalidation(&mut routes, 0, || Err(Error::Timeout)),
            Err(Error::Quarantined)
        );
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].state, IrRouteState::Quarantined);
        assert_eq!(
            find_owned_msi_vector(
                &routes,
                routes[0].requester,
                routes[0].message_address,
                routes[0].message_data,
            ),
            Err(Error::Quarantined)
        );
        assert_eq!(
            retire_ir_route_after_invalidation(&mut routes, 0, || Ok(())),
            Err(Error::Quarantined)
        );
        assert_eq!(routes.len(), 1);
    }

    #[test]
    fn successfully_retired_ir_route_is_no_longer_resolvable() {
        let mut routes = alloc::vec![test_ir_route()];
        let route = routes[0];
        assert_eq!(
            retire_ir_route_after_invalidation(&mut routes, 0, || Ok(())),
            Ok(())
        );
        assert!(routes.is_empty());
        assert_eq!(
            find_owned_msi_vector(
                &routes,
                route.requester,
                route.message_address,
                route.message_data,
            ),
            Ok(None)
        );
    }

    #[test]
    fn shared_boot_identity_lease_rejects_out_of_aperture_before_recording() {
        let requester = test_requester();
        let other = tk_vtd::PciRequester {
            device: 3,
            ..requester
        };
        let mut leases = Vec::new();
        register_identity_lease(&mut leases, requester, 17, &[0x1000, 0x1000], 0x3000).unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(
            register_identity_lease(&mut leases, other, 18, &[0x2000], 0x2000),
            Err(Error::InvalidRange)
        );
        assert_eq!(leases.len(), 1, "out-of-aperture page created no owner");
        assert_eq!(
            register_identity_lease(&mut leases, requester, 19, &[0x1000], 0x3000),
            Err(Error::InvalidStructure)
        );
        assert_eq!(
            require_no_identity_lease(&leases, requester),
            Err(Error::InvalidStructure)
        );
        assert_eq!(require_no_identity_lease(&leases, other), Ok(()));
    }

    #[test]
    fn shared_boot_identity_tokens_refcount_overlap_and_require_explicit_release() {
        let requester = test_requester();
        let mut leases = Vec::new();
        register_identity_lease(&mut leases, requester, 9, &[0x1000], 0x5000).unwrap();
        {
            let owner = &mut leases[0].owner;
            let first = prepare_identity_batch_record(owner, &[0x1000, 0x2000]).unwrap();
            publish_identity_batch(owner, 1, first, MappingState::Active);
            let second = prepare_identity_batch_record(owner, &[0x2000, 0x3000]).unwrap();
            publish_identity_batch(owner, 2, second, MappingState::Active);
            assert_eq!(
                owner
                    .pages
                    .iter()
                    .find(|p| p.physical == 0x1000)
                    .unwrap()
                    .references,
                1
            );
            assert_eq!(
                owner
                    .pages
                    .iter()
                    .find(|p| p.physical == 0x2000)
                    .unwrap()
                    .references,
                2
            );
        }
        assert_eq!(
            release_identity_lease(&mut leases, requester, 9),
            Err(Error::InvalidStructure)
        );

        {
            let owner = &mut leases[0].owner;
            retire_identity_batch_record(owner, 1).unwrap();
            assert_eq!(
                owner
                    .pages
                    .iter()
                    .find(|p| p.physical == 0x1000)
                    .unwrap()
                    .references,
                0
            );
            assert!(
                owner
                    .pages
                    .iter()
                    .find(|p| p.physical == 0x1000)
                    .unwrap()
                    .permanent
            );
            assert_eq!(
                owner
                    .pages
                    .iter()
                    .find(|p| p.physical == 0x2000)
                    .unwrap()
                    .references,
                1
            );
            retire_identity_batch_record(owner, 2).unwrap();
            assert_eq!(owner.pages.len(), 1);
            assert_eq!(owner.pages[0].physical, 0x1000);
        }

        release_identity_lease(&mut leases, requester, 9).unwrap();
        assert!(leases.is_empty());
        assert_eq!(require_no_identity_lease(&leases, requester), Ok(()));
    }

    #[test]
    fn requester_lease_and_mapping_ids_never_wrap_and_reuse() {
        let mut next_lease_id = 1;
        let mut next_mapping_id = 1;
        assert_eq!(take_nonreusing_id(&mut next_lease_id), Ok(1));
        assert_eq!(take_nonreusing_id(&mut next_mapping_id), Ok(1));
        assert_eq!(next_lease_id, 2);
        assert_eq!(next_mapping_id, 2);
        next_mapping_id = u64::MAX;
        assert_eq!(
            take_nonreusing_id(&mut next_mapping_id),
            Err(Error::OutOfMemory)
        );
        assert_eq!(next_mapping_id, u64::MAX);
    }

    #[test]
    fn direct_identity_lease_tracks_complete_no_translation_lifecycle() {
        let requester = test_requester();
        let lease = direct_identity_acquire(requester, &[0x1000, 0x1000]).unwrap();
        let mapping = direct_identity_map(requester, lease, &[0x1000, 0x2000]).unwrap();
        assert_eq!(
            direct_identity_release(requester, lease),
            Err(Error::InvalidStructure)
        );
        assert_eq!(direct_identity_unmap(requester, lease, mapping), Ok(()));
        assert_eq!(
            direct_identity_unmap(requester, lease, mapping),
            Err(Error::InvalidRange)
        );
        assert_eq!(direct_identity_release(requester, lease), Ok(()));
        assert_eq!(
            direct_identity_release(requester, lease),
            Err(Error::NoDomain)
        );
    }

    #[test]
    fn identity_lease_adds_only_matching_scoped_rmrr_pages() {
        let requester = test_requester();
        let scope = tk_vtd::OwnedScope {
            scope_type: 1,
            enumeration_id: 0,
            start_bus: 0,
            path: vec![2, 0],
        };
        let other_scope = tk_vtd::OwnedScope {
            path: vec![3, 0],
            ..scope.clone()
        };
        let dmar = tk_vtd::DmarTable {
            host_address_width: 39,
            interrupt_remapping: true,
            units: Vec::new(),
            reserved_regions: vec![
                tk_vtd::ReservedRegion {
                    segment: 0,
                    base: 0x3001,
                    end_inclusive: 0x4fff,
                    scopes: vec![scope],
                },
                tk_vtd::ReservedRegion {
                    segment: 0,
                    base: 0x8000,
                    end_inclusive: 0x8fff,
                    scopes: vec![other_scope],
                },
            ],
            hardware_affinities: Vec::new(),
        };
        assert_eq!(
            identity_pages_for_path(&dmar, requester, &[(0, 2, 0)], &[0x1000, 0x3000]).unwrap(),
            [0x1000, 0x3000, 0x4000]
        );
    }
}
