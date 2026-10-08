// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
// Native sequence adapts Linux7.2.3 gt/intel_execlists_submission.c
// enable_execlists/reset_csb_pointers (Copyright © 2014 Intel Corporation).
// Full MIT grant and inventory: crates/ax/tk-intel-gt/LICENSE-MIT and NOTICE.
//! BCS/RCS selftests and standard nonprivileged soft-pinned jobs. Software
//! preparation uses existing SharedPages/GGTT; DMA ownership precedes ELSQ load.
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering, fence};

use intel_gt::{Error, GtIo, bcs, lrc, ppgtt};

use super::super::{
    gtt::{Binding, Gtt},
    pci,
};
use crate::mm::{SharedFixedView, SharedPages};
#[path = "copy_ppgtt.rs"]
mod sparse;
const PAGE: usize = 4096;
const PAYLOAD: usize = 64 * 64 * 4;
struct Ram {
    pages: Arc<SharedPages>,
    _pin: SharedFixedView,
    physical: Vec<u64>,
}
impl Ram {
    fn allocate(count: usize) -> Result<Self, Error> {
        let bytes = count.checked_mul(PAGE).ok_or(Error::Refused)?;
        let pages = Arc::try_new(
            SharedPages::new_fixed(bytes, axhal::paging::PageSize::Size4K)
                .map_err(|_| Error::Refused)?,
        )
        .map_err(|_| Error::Refused)?;
        let pin = pages.fixed_view().map_err(|_| Error::Refused)?;
        let mut physical = Vec::new();
        physical
            .try_reserve_exact(count)
            .map_err(|_| Error::Refused)?;
        for i in 0..count {
            let p = pages.paddr_at(i).map_err(|_| Error::Refused)?.as_usize() as u64;
            ppgtt::physical(p)?;
            physical.push(p);
        }
        Ok(Self {
            pages,
            _pin: pin,
            physical,
        })
    }
    fn from_pages(pages: Arc<SharedPages>) -> Result<Self, Error> {
        if pages.is_external() || pages.page_size() != axhal::paging::PageSize::Size4K {
            return Err(Error::Refused);
        }
        let pin = pages.fixed_view().map_err(|_| Error::Refused)?;
        let count = pin.len() / PAGE;
        if count == 0 || count > 4096 {
            return Err(Error::Refused);
        }
        let mut physical = Vec::new();
        physical
            .try_reserve_exact(count)
            .map_err(|_| Error::Refused)?;
        for i in 0..count {
            let p = pages.paddr_at(i).map_err(|_| Error::Refused)?.as_usize() as u64;
            ppgtt::physical(p)?;
            physical.push(p);
        }
        Ok(Self {
            pages,
            _pin: pin,
            physical,
        })
    }
    fn write(&self, offset: usize, data: &[u8]) -> Result<(), Error> {
        self.pages
            .write_bytes(offset, data)
            .map_err(|_| Error::Refused)
    }
    fn read(&self, offset: usize, data: &mut [u8]) -> Result<(), Error> {
        self.pages
            .read_bytes(offset, data)
            .map_err(|_| Error::Refused)
    }
    fn dwords(&self, page: usize, words: &[u32]) -> Result<(), Error> {
        let mut data = Vec::new();
        data.try_reserve_exact(words.len() * 4)
            .map_err(|_| Error::Refused)?;
        for word in words {
            data.extend_from_slice(&word.to_le_bytes());
        }
        self.write(page * PAGE, &data)
    }
    fn table(&self, page: usize, words: &[u64; 512]) -> Result<(), Error> {
        let mut data = Vec::new();
        data.try_reserve_exact(PAGE).map_err(|_| Error::Refused)?;
        for word in words {
            data.extend_from_slice(&word.to_le_bytes());
        }
        self.write(page * PAGE, &data)
    }
    fn flush(&self) {
        // x86_64 only. Flush both before GPU reads and before CPU reads of GPU
        // output. Fixed-view pins exclude physical folio replacement/reclaim.
        for &physical in &self.physical {
            let base =
                axhal::mem::phys_to_virt(axhal::mem::PhysAddr::from_usize(physical as usize))
                    .as_usize();
            for offset in (0..PAGE).step_by(64) {
                // SAFETY: retained ordinary system-RAM page, each address is a
                // cache line inside it. CLFLUSH neither frees nor changes PTEs.
                unsafe { core::arch::x86_64::_mm_clflush((base + offset) as *const u8) };
            }
        }
        fence(Ordering::SeqCst);
    }
}
pub(crate) fn sync_cpu_pages(pages: Arc<SharedPages>) -> Result<(), Error> {
    // The same fixed system-RAM cache synchronization serves rendering and
    // display pin-to-GTT preparation. Do not apply the <=16MiB execution-BO
    // limit to a 4K scanout (or its double-height dumb backing).
    if pages.is_external() || pages.page_size() != axhal::paging::PageSize::Size4K {
        return Err(Error::Refused);
    }
    let pin = pages.fixed_view().map_err(|_| Error::Refused)?;
    let count = pin.len() / PAGE;
    if count == 0 || count > 16_384 {
        return Err(Error::Refused);
    }
    // CLFLUSH is optional in the architecture; admit the actual executing
    // CPU's capability/line size before accessing any page. This target is
    // N305 only, not a cache-maintenance fallback for unknown CPU layouts.
    let cpu = core::arch::x86_64::__cpuid(1);
    if cpu.edx & (1 << 19) == 0 || ((cpu.ebx >> 8) & 0xff) * 8 != 64 {
        return Err(Error::Refused);
    }
    for index in 0..count {
        ppgtt::physical(
            pages
                .paddr_at(index)
                .map_err(|_| Error::Refused)?
                .as_usize() as u64,
        )?;
    }
    for index in 0..count {
        let physical = pages.paddr_at(index).map_err(|_| Error::Refused)?;
        let base = axhal::mem::phys_to_virt(physical).as_usize();
        for offset in (0..PAGE).step_by(64) {
            // SAFETY: an ordinary allocator-owned, physically validated 4K
            // page retained by `pages` and its fixed-view pin. CLFLUSH invalidates
            // the WB alias before the non-snooping display/GT reads; no MMIO.
            unsafe { core::arch::x86_64::_mm_clflush((base + offset) as *const u8) };
        }
    }
    fence(Ordering::SeqCst);
    Ok(())
}
/// Real, pinned private page-table ownership. Currently the admitted three
/// windows and sparse four-level48-bit residency share the same persistent root.
/// The gate serializes all contexts sharing this root through GPU retirement.
struct Residency {
    _tables: Arc<Vec<Ram>>,
    _pages: Vec<Arc<SharedPages>>,
}
pub(crate) struct Vm {
    tables: Arc<Ram>,
    charge: Option<Arc<crate::drm::gem::GemMemoryCharge>>,
    residency: axsync::Mutex<Option<Arc<Residency>>>,
    pub(crate) gate: axsync::Mutex<()>,
}
impl Vm {
    pub(crate) fn new() -> Result<Arc<Self>, Error> {
        let tables = Arc::try_new(Ram::allocate(8)?).map_err(|_| Error::Refused)?;
        let p = &tables.physical;
        let mut words = zero_words::<u64>(512)?;
        let page: &mut [u64; 512] = words.as_mut_slice().try_into().unwrap();
        // Empty low-address hierarchy plus dedicated per-level scratch. The
        // VM ID owns real initialized page tables, not an unbacked identifier.
        ppgtt::directory(page, p[4], p[1])?;
        tables.table(0, page)?;
        ppgtt::directory(page, p[5], p[2])?;
        tables.table(1, page)?;
        ppgtt::directory(page, p[6], p[3])?;
        tables.table(2, page)?;
        ppgtt::leaf(page, p[7], 3)?;
        tables.table(3, page)?;
        page.fill(ppgtt::pde(p[5])?);
        tables.table(4, page)?;
        page.fill(ppgtt::pde(p[6])?);
        tables.table(5, page)?;
        ppgtt::leaf(page, p[7], 3)?;
        tables.table(6, page)?;
        tables.flush();
        Arc::try_new(Self {
            tables,
            charge: None,
            residency: axsync::Mutex::new(None),
            gate: axsync::Mutex::new(()),
        })
        .map_err(|_| Error::Refused)
    }
    pub(crate) fn new_for_file(file: &crate::drm::DrmFile) -> axerrno::AxResult<Arc<Self>> {
        let charge = file
            .reserve_render_memory(8 * PAGE)
            .map_err(axerrno::AxError::from)?;
        let mut vm = Self::new().map_err(|_| axerrno::AxError::NoMemory)?;
        // Charge follows the actual page-table allocation, including retained
        // DMA/quarantine owners after a file or VM handle disappears.
        vm.tables.pages.retain_allocation_owner(charge.clone())?;
        Arc::get_mut(&mut vm).unwrap().charge = Some(charge);
        Ok(vm)
    }
    #[cfg(test)]
    pub(crate) fn root(&self) -> u64 {
        self.tables.physical[0]
    }
}

/// Standard soft-pinned nonprivileged batch residency, not a private shader ABI.
pub(crate) struct UserObject {
    pub(crate) address: u64,
    pub(crate) pages: Arc<SharedPages>,
    pub(crate) writable: bool,
    pub(crate) cache: Option<Arc<core::sync::atomic::AtomicU8>>,
}
pub(crate) struct UserJob {
    pub(crate) objects: Vec<UserObject>,
    pub(crate) batch: u64,
    pub(crate) render: bool,
}
impl UserJob {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.objects.is_empty() || self.objects.len() > 1024 || !self.batch.is_multiple_of(8) {
            return Err(Error::Refused);
        }
        for (i, object) in self.objects.iter().enumerate() {
            let ram = Ram::from_pages(object.pages.clone())?;
            let (start, end) = sparse::checked_range(object.address, ram.physical.len())?;
            if i == 0 {
                let batch = sparse::normalize(self.batch)?;
                if batch < start || batch >= end {
                    return Err(Error::Refused);
                }
            }
            for previous in &self.objects[..i] {
                let pin = previous.pages.fixed_view().map_err(|_| Error::Refused)?;
                let (a, b) = sparse::checked_range(previous.address, pin.len() / PAGE)?;
                if start < b && a < end {
                    return Err(Error::Refused);
                }
            }
        }
        Ok(())
    }
}
/// An opaque image is valid only after a confirmed hardware context switch
/// and scoped reset retirement. Userspace cannot import a CPU-produced image.
// Linux intel_gt.c __engines_record_defaults: reset-state inhibited request,
// switch to a distinct kernel context, retire, then retain its opaque image.
// Copyright ©2014 Intel Corporation; existing tk-intel-gt/LICENSE-MIT grant.
static DEFAULTS: axsync::Mutex<[Option<Arc<Ram>>; 2]> = axsync::Mutex::new([None, None]);
pub(super) fn isolation_classes() -> u32 {
    let defaults = DEFAULTS.lock();
    captured_classes(defaults[0].is_some(), defaults[1].is_some())
}
fn captured_classes(copy: bool, render: bool) -> u32 {
    (u32::from(copy) << 1) | u32::from(render)
}
pub(crate) struct SavedContext {
    ram: Arc<Ram>,
    valid: AtomicBool,
    render: bool,
}
impl SavedContext {
    pub(crate) fn new(file: &crate::drm::DrmFile, render: bool) -> axerrno::AxResult<Arc<Self>> {
        let count = if render {
            intel_gt::rcs::CONTEXT_PAGES
        } else {
            lrc::CONTEXT_PAGES
        };
        let charge = file
            .reserve_render_memory(count * PAGE)
            .map_err(axerrno::AxError::from)?;
        let ram = Arc::try_new(Ram::allocate(count).map_err(|_| axerrno::AxError::NoMemory)?)
            .map_err(|_| axerrno::AxError::NoMemory)?;
        ram.pages.retain_allocation_owner(charge)?;
        let default = DEFAULTS.lock()[usize::from(render)].clone();
        if let Some(default) = &default {
            let count = if render { 14 } else { 2 };
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(count * PAGE)
                .map_err(|_| axerrno::AxError::NoMemory)?;
            bytes.resize(count * PAGE, 0);
            default
                .read(0, &mut bytes)
                .map_err(|_| axerrno::AxError::Io)?;
            ram.write(0, &bytes).map_err(|_| axerrno::AxError::Io)?;
            ram.write(0, &[0; 4096]).map_err(|_| axerrno::AxError::Io)?;
            // lrc_init_state/init_common_regs for a newly cloned context,
            // not a subsequent warm request: fresh runtime and BB offset.
            ram.write(PAGE + 35 * 4, &0u32.to_le_bytes())
                .map_err(|_| axerrno::AxError::Io)?;
            ram.write(PAGE + 0x71 * 4, &0u32.to_le_bytes())
                .map_err(|_| axerrno::AxError::Io)?;
        }
        Arc::try_new(Self {
            ram,
            valid: AtomicBool::new(default.is_some()),
            render,
        })
        .map_err(|_| axerrno::AxError::NoMemory)
    }
}
struct SwitchAway {
    context: Ram,
    ring: Ram,
    bindings: Vec<Binding>,
    descriptor: u64,
}
impl SwitchAway {
    fn new(render: bool) -> Result<Self, Error> {
        let mut bindings = Vec::new();
        bindings.try_reserve_exact(2).map_err(|_| Error::Refused)?;
        Ok(Self {
            context: Ram::allocate(if render {
                intel_gt::rcs::CONTEXT_PAGES
            } else {
                lrc::CONTEXT_PAGES
            })?,
            ring: Ram::allocate(1)?,
            bindings,
            descriptor: 0,
        })
    }
    fn build(&mut self, gtt: &Gtt, render: bool, root: u64, io: &impl GtIo) -> Result<(), Error> {
        for ram in [&self.context, &self.ring] {
            self.bindings.push(
                gtt.bind_pages(&ram.physical)
                    .map_err(|_| Error::Quarantined)?,
            );
        }
        let context = self.bindings[0].address as u32;
        let ring = self.bindings[1].address as u32;
        let mut regs = [0; 1024];
        let mut indirect = [0; 1024];
        let mut per = [0; 1024];
        let descriptor = if render {
            intel_gt::rcs::build_context(
                &mut regs,
                &mut indirect,
                &mut per,
                context,
                ring,
                78 * 4,
                root,
            )?
        } else {
            lrc::build(&mut regs, &mut indirect, &mut per, context, ring, 120, root)?
        };
        // Source Gen11/12.0 descriptor SW context bits37..47. Port1 must be a
        // distinct context to force the port0 image save before its breadcrumb.
        self.descriptor = (descriptor & !(0x7ffu64 << 37)) | (3u64 << 37);
        self.context.dwords(1, &regs)?;
        self.context
            .dwords(if render { 14 } else { 2 }, &indirect)?;
        self.context.dwords(if render { 15 } else { 3 }, &per)?;
        if render {
            let mut words = [0; 78];
            let mut normal = [0; 42];
            intel_gt::rcs::ring(&mut normal, 0x30000, context, 1)?;
            normal[23..26].fill(0); // source idle request has no BB dispatch.
            words[..22].copy_from_slice(&normal[..22]);
            words[1] |= 1 << 27;
            intel_gt::rcs::context_wa(io, &mut words[22..36])?;
            words[36..].copy_from_slice(&normal);
            words[37] |= 1 << 27;
            self.ring.dwords(0, &words)?;
        } else {
            let count = bcs::ring(&mut regs, 0x30000, context, 1)?;
            regs[15..18].fill(0); // only MI_BATCH_BUFFER_START and its two address words.
            self.ring.dwords(0, &regs[..count])?;
        }
        self.context.flush();
        self.ring.flush();
        Ok(())
    }
    fn release(&mut self, gtt: &Gtt) -> Result<(), Error> {
        for binding in self.bindings.iter().rev() {
            // SAFETY: outer selected-engine reset established full retirement.
            unsafe { gtt.release_binding(binding) }.map_err(|_| Error::Quarantined)?;
        }
        self.bindings.clear();
        Ok(())
    }
}

pub(super) struct Memory {
    gtt: Arc<Gtt>,
    tables: Arc<Ram>,
    extra_tables: Arc<Vec<Ram>>,
    vm: Option<Arc<Vm>>,
    retained: Option<Arc<Residency>>,
    user: Option<Arc<UserJob>>,
    resident: Vec<Ram>,
    batch_address: u64,
    table_charge: Option<Arc<crate::drm::gem::GemMemoryCharge>>,
    source: Ram,
    destination: Ram,
    context: Arc<Ram>,
    ring: Ram,
    batch: Ram,
    status: Ram,
    bindings: Vec<Binding>,
    descriptor: u64,
    operation: bcs::Copy,
    selftest: bool,
    render: bool,
    idle: bool,
    saved: Option<Arc<SavedContext>>,
    switch: Option<Box<SwitchAway>>,
}

/// A firmware GGTT source retained until its DMA completion has been proven.
#[cfg(target_os = "none")]
pub(super) struct UcDmaMemory {
    _gtt: Arc<Gtt>,
    _ram: Ram,
    _binding: Binding,
}

#[cfg(target_os = "none")]
impl UcDmaMemory {
    fn release(self) -> Result<(), Self> {
        // SAFETY: callers use this only after the GuC has either completed
        // HuC authentication or explicitly rejected the action.
        if unsafe { self._gtt.release_binding(&self._binding) }.is_err() {
            Err(self)
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "none")]
pub(super) struct CtDmaMemory {
    _gtt: Arc<Gtt>,
    _ram: Ram,
    _binding: Binding,
    pair: intel_gt::guc_ct::CtbPair,
    submission: intel_gt::guc_submission::GucSubmission,
    blob: Vec<u8>,
    enabled: bool,
}

#[cfg(target_os = "none")]
pub(super) struct AdsDmaMemory {
    _gtt: Arc<Gtt>,
    _ram: Ram,
    binding: Binding,
    input: intel_gt::guc_ads::AdsBuildInput,
    layout: intel_gt::guc_ads::AdsLayout,
    blob: Vec<u8>,
    registered: bool,
}

#[cfg(target_os = "none")]
pub(super) struct LogDmaMemory {
    _gtt: Arc<Gtt>,
    _ram: Ram,
    binding: Binding,
    layout: intel_gt::guc_log::GucLogLayout,
    config: intel_gt::guc_config::LogConfig,
    registered: bool,
}

#[cfg(target_os = "none")]
impl LogDmaMemory {
    fn release(self) -> Result<(), Self> {
        if self.registered {
            return Err(self);
        }
        // SAFETY: GuC has not been configured with this address, or its reset
        // completed before registration was cleared.
        if unsafe { self._gtt.release_binding(&self.binding) }.is_err() {
            Err(self)
        } else {
            Ok(())
        }
    }

    /// Allocate a zeroed GuC logging state/data VMA and derive GUC_CTL layout
    /// fields from upstream section sizing.
    /// upstream: intel_guc_log.c intel_guc_log_create()/guc_log_init_sizes().
    pub(super) fn initialize(
        owner: &mut super::Owner,
        debug_guc: bool,
        debug_gem: bool,
    ) -> Result<(), Error> {
        if owner.lost || owner.log_memory.is_some() {
            return Err(Error::Quarantined);
        }
        let layout = intel_gt::guc_log::guc_log_init_sizes(debug_guc, debug_gem)
            .map_err(|_| Error::Refused)?;
        let size = usize::try_from(layout.buffer_bytes).map_err(|_| Error::Refused)?;
        let pages = size.checked_add(PAGE - 1).ok_or(Error::Refused)? / PAGE;
        let mut zeroes = Vec::new();
        zeroes.try_reserve_exact(size).map_err(|_| Error::Refused)?;
        zeroes.resize(size, 0);
        let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
        let ram = Ram::allocate(pages)?;
        let binding = gtt
            .bind_pages(&ram.physical)
            .map_err(|_| Error::Quarantined)?;
        let base = match u32::try_from(binding.address) {
            Ok(base) if base != 0 && base.is_multiple_of(PAGE as u32) => base,
            _ => {
                let memory = Self {
                    _gtt: gtt,
                    _ram: ram,
                    binding,
                    layout,
                    config: layout.control_config(0),
                    registered: false,
                };
                if let Err(memory) = memory.release() {
                    owner.log_memory = Some(memory);
                    return Err(Error::Quarantined);
                }
                return Err(Error::Refused);
            }
        };
        let config = layout.control_config(base);
        if let Err(error) = ram.write(0, &zeroes) {
            let memory = Self {
                _gtt: gtt,
                _ram: ram,
                binding,
                layout,
                config,
                registered: false,
            };
            if let Err(memory) = memory.release() {
                owner.log_memory = Some(memory);
                return Err(Error::Quarantined);
            }
            return Err(error);
        }
        ram.flush();
        owner.log_memory = Some(Self {
            _gtt: gtt,
            _ram: ram,
            binding,
            layout,
            config,
            registered: false,
        });
        Ok(())
    }

    pub(super) fn config(&self) -> intel_gt::guc_config::LogConfig {
        self.config
    }

    pub(super) fn layout(&self) -> intel_gt::guc_log::GucLogLayout {
        self.layout
    }

    fn read_shared_log(&self) -> Result<Vec<u8>, Error> {
        let size = usize::try_from(self.layout.buffer_bytes).map_err(|_| Error::Refused)?;
        let mut shared = Vec::new();
        shared.try_reserve_exact(size).map_err(|_| Error::Refused)?;
        shared.resize(size, 0);
        self._ram.read(0, &mut shared)?;
        Ok(shared)
    }

    /// Copy debug/crash rings, update their read state and leave capture data
    /// for the state-capture event path.
    /// upstream: intel_guc_log.c _guc_log_copy_debuglogs_for_relay().
    pub(super) fn drain_debug_logs(
        &mut self,
        stats: &mut [intel_gt::guc_log::LogStats; 3],
    ) -> Result<intel_gt::guc_log::LogSnapshot, Error> {
        let mut shared = self.read_shared_log()?;
        let snapshot =
            intel_gt::guc_log::copy_debug_logs_for_relay(&mut shared, self.layout, stats)?;
        self._ram.write(0, &shared)?;
        self._ram.flush();
        Ok(snapshot)
    }

    /// Parse and acknowledge the capture-region state after a G2H capture
    /// notification. The caller sends the corresponding flush-complete action.
    pub(super) fn drain_capture_log(
        &mut self,
        stats: &mut intel_gt::guc_log::LogStats,
        reset_in_progress: bool,
    ) -> Result<intel_gt::guc_capture::CaptureLogResult, Error> {
        let mut shared = self.read_shared_log()?;
        let result = intel_gt::guc_log::process_capture_log(
            &mut shared,
            self.layout,
            stats,
            reset_in_progress,
        )?;
        self._ram.write(0, &shared)?;
        self._ram.flush();
        Ok(result)
    }

    pub(super) fn mark_registered(&mut self) {
        self.registered = true;
    }
}

/// Publish GuC scratch parameters only after matching pinned ADS/log buffers
/// exist. The GuC must still be held in MIA reset; after any write succeeds,
/// retain both VMA owners before firmware transfer can consume the pointers.
/// upstream: intel_guc.c intel_guc_write_params().
pub(super) fn write_guc_init_params(
    owner: &mut super::Owner,
    options: intel_gt::guc_config::GucOptions,
) -> Result<[u32; intel_gt::guc_config::GUC_CTL_MAX_DWORDS], Error> {
    if owner.lost || !owner.bus.awake.load(Ordering::Acquire) {
        return Err(Error::Refused);
    }
    if owner.bus.read(0xc000)? & 1 == 0 {
        return Err(Error::Refused);
    }
    let ads_address = owner
        .ads_memory
        .as_ref()
        .ok_or(Error::Refused)?
        .ggtt_address()?;
    let log_config = owner.log_memory.as_ref().ok_or(Error::Refused)?.config();
    if options.ads_ggtt_address != ads_address
        || options.log.ggtt_address != log_config.ggtt_address
    {
        return Err(Error::Refused);
    }
    let params = intel_gt::guc_config::guc_init_params(options).map_err(|_| Error::Refused)?;
    intel_gt::guc_config::write_params(&owner.bus, &params)?;
    owner
        .ads_memory
        .as_mut()
        .ok_or(Error::Quarantined)?
        .mark_registered();
    owner
        .log_memory
        .as_mut()
        .ok_or(Error::Quarantined)?
        .mark_registered();
    Ok(params)
}

/// Handle a G2H state-capture notification: report NOSPACE, drain/ack the
/// capture region, then send LOG_BUFFER_FILE_FLUSH_COMPLETE over CTB.
/// upstream: intel_guc_submission.c intel_guc_error_capture_process_msg()
/// + intel_guc_capture.c __guc_capture_process_output().
pub(super) fn handle_guc_capture_notification(
    owner: &mut super::Owner,
    stats: &mut intel_gt::guc_log::LogStats,
    reset_in_progress: bool,
    status_word: u32,
) -> Result<intel_gt::guc_capture::CaptureLogResult, Error> {
    if owner.lost || !owner.bus.awake.load(Ordering::Acquire) {
        return Err(Error::Refused);
    }
    let status = status_word & intel_gt::guc_capture::CAPTURE_EVENT_STATUS_MASK;
    if status == intel_gt::guc_capture::CAPTURE_EVENT_STATUS_NOSPACE {
        axlog::warn!("intel-gt: GuC capture buffer reported no space");
    }
    let result = owner
        .log_memory
        .as_mut()
        .ok_or(Error::Refused)?
        .drain_capture_log(stats, reset_in_progress)?;
    let mut cache_error = false;
    if let Some(cache) = owner.capture_nodes.as_mut() {
        for group in &result.groups {
            if cache.process_group(group).is_err() {
                cache_error = true;
                break;
            }
        }
    } else if !result.groups.is_empty() {
        cache_error = true;
    }
    // As upstream, acknowledge the buffer even when a group could not be
    // parsed/retained so GuC does not remain blocked on the flush handshake.
    owner
        .ct_memory
        .as_mut()
        .ok_or(Error::Quarantined)?
        .send_capture_flush_complete(&owner.bus)
        .map_err(|_| Error::Quarantined)?;
    if cache_error {
        return Err(Error::Quarantined);
    }
    if result.parse_error.is_some() {
        axlog::warn!("intel-gt: GuC capture log contained a malformed record");
    }
    Ok(result)
}

/// Workqueue-side debug-log relay snapshot and GuC flush acknowledgment.
/// upstream: intel_guc_log.c copy_debug_logs_work()/guc_log_copy_debuglogs_for_relay().
pub(super) fn process_guc_debug_log_flush(
    owner: &mut super::Owner,
    stats: &mut [intel_gt::guc_log::LogStats; 3],
) -> Result<intel_gt::guc_log::LogSnapshot, Error> {
    if owner.lost || !owner.bus.awake.load(Ordering::Acquire) {
        return Err(Error::Refused);
    }
    let snapshot = owner
        .log_memory
        .as_mut()
        .ok_or(Error::Refused)?
        .drain_debug_logs(stats)?;
    owner
        .ct_memory
        .as_mut()
        .ok_or(Error::Quarantined)?
        .send_debug_flush_complete(&owner.bus)
        .map_err(|_| Error::Quarantined)?;
    Ok(snapshot)
}

#[cfg(target_os = "none")]
pub(super) fn take_guc_capture_node(
    owner: &mut super::Owner,
    engine_guc_id: u32,
    context_guc_id: u32,
    context_lrca: u32,
) -> Option<intel_gt::guc_capture::PooledCaptureNode> {
    owner
        .capture_nodes
        .as_mut()?
        .take_matching_node(engine_guc_id, context_guc_id, context_lrca)
}

#[cfg(target_os = "none")]
pub(super) fn recycle_guc_capture_node(
    owner: &mut super::Owner,
    node: intel_gt::guc_capture::PooledCaptureNode,
) {
    if let Some(cache) = owner.capture_nodes.as_mut() {
        cache.recycle_node(node);
    }
}

#[cfg(target_os = "none")]
impl AdsDmaMemory {
    fn release(self) -> Result<(), Self> {
        if self.registered {
            return Err(self);
        }
        // SAFETY: ADS has not been published to GuC, or GuC was reset before
        // the caller cleared `registered`.
        if unsafe { self._gtt.release_binding(&self.binding) }.is_err() {
            Err(self)
        } else {
            Ok(())
        }
    }

    /// Allocate and populate the ADS GGTT VMA. Caller-provided data must come
    /// from probed engine/MCR/system-info/context state, not guessed defaults.
    /// upstream: intel_guc_ads.c intel_guc_ads_create()/__guc_ads_init().
    pub(super) fn initialize(
        owner: &mut super::Owner,
        mut input: intel_gt::guc_ads::AdsBuildInput,
    ) -> Result<(), Error> {
        if owner.lost || owner.ads_memory.is_some() {
            return Err(Error::Quarantined);
        }
        // The required allocation size is independent of the eventual GGTT
        // address; use an aligned placeholder to run the checked builder once.
        input.base_ggtt = PAGE as u32;
        let (layout, _) = intel_gt::guc_ads::build_ads(&input).map_err(|_| Error::Refused)?;
        let pages = layout
            .total_size
            .checked_add(PAGE - 1)
            .ok_or(Error::Refused)?
            / PAGE;
        let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
        let ram = Ram::allocate(pages)?;
        let binding = gtt
            .bind_pages(&ram.physical)
            .map_err(|_| Error::Quarantined)?;
        let base = match u32::try_from(binding.address) {
            Ok(base) if base != 0 && base.is_multiple_of(PAGE as u32) => base,
            _ => {
                let memory = Self {
                    _gtt: gtt,
                    _ram: ram,
                    binding,
                    input,
                    layout,
                    blob: Vec::new(),
                    registered: false,
                };
                if let Err(memory) = memory.release() {
                    owner.ads_memory = Some(memory);
                    return Err(Error::Quarantined);
                }
                return Err(Error::Refused);
            }
        };
        input.base_ggtt = base;
        let (actual_layout, blob) = match intel_gt::guc_ads::build_ads(&input) {
            Ok(built) if built.0.total_size == layout.total_size => built,
            _ => {
                let memory = Self {
                    _gtt: gtt,
                    _ram: ram,
                    binding,
                    input,
                    layout,
                    blob: Vec::new(),
                    registered: false,
                };
                if let Err(memory) = memory.release() {
                    owner.ads_memory = Some(memory);
                    return Err(Error::Quarantined);
                }
                return Err(Error::Refused);
            }
        };
        let memory = Self {
            _gtt: gtt,
            _ram: ram,
            binding,
            input,
            layout: actual_layout,
            blob,
            registered: false,
        };
        if let Err(error) = memory._ram.write(0, &memory.blob) {
            if let Err(memory) = memory.release() {
                owner.ads_memory = Some(memory);
                return Err(Error::Quarantined);
            }
            return Err(error);
        }
        memory._ram.flush();
        owner.ads_memory = Some(memory);
        Ok(())
    }

    pub(super) fn ggtt_address(&self) -> Result<u32, Error> {
        u32::try_from(self.binding.address).map_err(|_| Error::Refused)
    }

    /// Rebuild all ADS regions after GuC reset and publish the bytes before
    /// re-registering the VMA.
    /// upstream: intel_guc_ads.c intel_guc_ads_reset().
    pub(super) fn reset_after_guc_reset(&mut self) -> Result<(), Error> {
        let layout = intel_gt::guc_ads::intel_guc_ads_reset(&self.input, &mut self.blob)
            .map_err(|_| Error::Quarantined)?;
        if layout.total_size != self.layout.total_size {
            return Err(Error::Quarantined);
        }
        self._ram.write(0, &self.blob)?;
        self._ram.flush();
        self.registered = false;
        Ok(())
    }

    pub(super) fn mark_registered(&mut self) {
        self.registered = true;
    }
}

#[cfg(target_os = "none")]
impl CtDmaMemory {
    fn release(self) -> Result<(), Self> {
        // SAFETY: the CTB KLVs have not been registered with GuC yet.
        if unsafe { self._gtt.release_binding(&self._binding) }.is_err() {
            Err(self)
        } else {
            Ok(())
        }
    }

    // upstream: intel_guc_ct.c intel_guc_ct_init()/intel_guc_ct_enable()
    pub(super) fn initialize(owner: &mut super::Owner) -> Result<(), Error> {
        if owner.ct_memory.is_some() {
            return Err(Error::Quarantined);
        }
        let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
        let submission = intel_gt::guc_submission::GucSubmission::new(
            intel_gt::guc_submission::GUC_MAX_CONTEXT_ID,
        )
        .map_err(|_| Error::Refused)?;
        let pages = intel_gt::guc_ct::CTB_BLOB_SIZE
            .checked_add(PAGE - 1)
            .ok_or(Error::Refused)?
            / PAGE;
        let ram = Ram::allocate(pages)?;
        let pair = intel_gt::guc_ct::CtbPair::new().map_err(|_| Error::Refused)?;
        let blob = pair.initial_blob();
        ram.write(0, &blob)?;
        ram.flush();
        let binding = gtt
            .bind_pages(&ram.physical)
            .map_err(|_| Error::Quarantined)?;
        let memory = Self {
            _gtt: gtt,
            _ram: ram,
            _binding: binding,
            pair,
            submission,
            blob,
            enabled: false,
        };
        let base = match u32::try_from(memory._binding.address) {
            Ok(address) => address,
            Err(_) => {
                if let Err(memory) = memory.release() {
                    owner.ct_memory = Some(memory);
                    return Err(Error::Quarantined);
                }
                return Err(Error::Refused);
            }
        };
        let addresses = match intel_gt::guc_ct::ctb_addresses(base) {
            Ok(addresses) => addresses,
            Err(_) => {
                if let Err(memory) = memory.release() {
                    owner.ct_memory = Some(memory);
                    return Err(Error::Quarantined);
                }
                return Err(Error::Refused);
            }
        };
        owner.ct_memory = Some(memory);
        let result = intel_gt::guc_ct::enable_buffer_transport(&owner.bus, addresses);
        if result.is_err() {
            // Self-config may have landed even when MMIO response handling
            // failed; keep the whole blob resident and quarantine the owner.
            return Err(Error::Quarantined);
        }
        if let Some(memory) = owner.ct_memory.as_mut() {
            memory.enabled = true;
        }
        Ok(())
    }

    pub(super) fn send_nonblocking(
        &mut self,
        bus: &impl GtIo,
        action: &[u32],
        flags: u32,
    ) -> Result<u16, Error> {
        if !self.enabled {
            return Err(Error::Refused);
        }
        self._ram.read(0, &mut self.blob)?;
        self.pair
            .sync_from_blob(&self.blob)
            .map_err(|_| Error::Quarantined)?;
        let fence = self
            .pair
            .send_nonblocking(action, flags)
            .map_err(|_| Error::Refused)?;
        self.pair
            .sync_to_blob(&mut self.blob)
            .map_err(|_| Error::Quarantined)?;
        self._ram.write(0, &self.blob)?;
        self._ram.flush();
        intel_gt::guc_fw::notify(bus)?;
        Ok(fence)
    }

    /// Exercise enabled CTB with an H2G control action and wait for its HXG
    /// response before reporting the startup transport ready. This does not
    /// replace the later IRQ/tasklet/workqueue event path.
    /// upstream: intel_guc_ct.c ct_send()/ct_handle_response().
    pub(super) fn verify_ctb_roundtrip(&mut self, bus: &impl GtIo) -> Result<(), Error> {
        if !self.enabled {
            return Err(Error::Refused);
        }
        self._ram.read(0, &mut self.blob)?;
        self.pair
            .sync_from_blob(&self.blob)
            .map_err(|_| Error::Quarantined)?;
        let action = [
            intel_gt::guc_ct::ACTION_HOST2GUC_CONTROL_CTB,
            intel_gt::guc_ct::CTB_CONTROL_ENABLE,
        ];
        let ram = &self._ram;
        let blob = &mut self.blob;
        let completion = self
            .pair
            .send_request_with_retry(bus, &action, 0, |pair, fence, short, long| {
                if fence.is_some() {
                    pair.sync_to_blob(blob)
                        .map_err(|_| intel_gt::guc_ct::CtError::InvalidMessage)?;
                    ram.write(0, blob)
                        .map_err(|_| intel_gt::guc_ct::CtError::InvalidMessage)?;
                    ram.flush();
                    intel_gt::guc_fw::notify(bus)
                        .map_err(|_| intel_gt::guc_ct::CtError::InvalidMessage)?;
                }
                let start = bus.now_us();
                let timeout = short.saturating_add(long);
                loop {
                    ram.read(0, blob)
                        .map_err(|_| intel_gt::guc_ct::CtError::InvalidMessage)?;
                    pair.sync_from_blob(blob)?;
                    let old_send_tail = pair.send.descriptor.tail;
                    if let Some(words) = pair.receive.read_message()? {
                        if pair.handle_incoming_message(&words)?.is_some() {
                            return Err(intel_gt::guc_ct::CtError::InvalidMessage);
                        }
                        pair.sync_to_blob(blob)?;
                        ram.write(0, blob)
                            .map_err(|_| intel_gt::guc_ct::CtError::InvalidMessage)?;
                        ram.flush();
                        if pair.send.descriptor.tail != old_send_tail {
                            intel_gt::guc_fw::notify(bus)
                                .map_err(|_| intel_gt::guc_ct::CtError::InvalidMessage)?;
                        }
                        return Ok(());
                    }
                    if bus.now_us().saturating_sub(start) > timeout {
                        return Err(intel_gt::guc_ct::CtError::Timeout);
                    }
                    bus.delay_us(50);
                }
            })
            .map_err(|_| Error::Quarantined)?;
        self.pair
            .sync_to_blob(&mut self.blob)
            .map_err(|_| Error::Quarantined)?;
        self._ram.write(0, &self.blob)?;
        self._ram.flush();
        intel_gt::guc_fw::notify(bus).map_err(|_| Error::Quarantined)?;
        match completion {
            intel_gt::guc_ct::CtCompletion::Success { .. } => Ok(()),
            intel_gt::guc_ct::CtCompletion::Failure { .. }
            | intel_gt::guc_ct::CtCompletion::Retry { .. } => Err(Error::Quarantined),
        }
    }

    /// Map one GuC scheduling H2G action to CTB. `expected_response_dwords`
    /// reserves the matching G2H credit before notifying GuC.
    pub(super) fn send_scheduling_action(
        &mut self,
        bus: &impl GtIo,
        action: &intel_gt::guc_submission::SchedAction,
    ) -> Result<u16, Error> {
        let len = usize::from(action.len);
        if len == 0 || len > action.words.len() {
            return Err(Error::Refused);
        }
        let flags = intel_gt::guc_ct::CT_SEND_NB | u32::from(action.expected_response_dwords);
        self.send_nonblocking(bus, &action.words[..len], flags)
    }

    /// Acknowledge completed capture-buffer file flush through CTB.
    pub(super) fn send_capture_flush_complete(&mut self, bus: &impl GtIo) -> Result<u16, Error> {
        let action = intel_gt::guc_capture::capture_flush_complete_action();
        self.send_nonblocking(bus, &action, intel_gt::guc_ct::CT_SEND_NB)
    }

    pub(super) fn send_debug_flush_complete(&mut self, bus: &impl GtIo) -> Result<u16, Error> {
        let action = intel_gt::guc_log::flush_log_complete_action();
        self.send_nonblocking(bus, &action, intel_gt::guc_ct::CT_SEND_NB)
    }

    fn publish_ctb(&mut self, bus: &impl GtIo) -> Result<(), Error> {
        self.pair
            .sync_to_blob(&mut self.blob)
            .map_err(|_| Error::Quarantined)?;
        self._ram.write(0, &self.blob)?;
        self._ram.flush();
        intel_gt::guc_fw::notify(bus)
    }

    /// GuC v70 context registration bridge. The context/LRC image and GGTT
    /// descriptor inputs remain owned by the caller; uncertain shared-memory
    /// publication retains the CT owner and must quarantine the GT.
    pub(super) fn register_guc_context_v70(
        &mut self,
        bus: &impl GtIo,
        info: intel_gt::guc_submission::GuCContextRegistrationInfo,
        policy: intel_gt::guc_submission::ContextPolicy,
        lease: intel_gt::guc_submission::ContextIdLease,
        child_lrcas: &[u64],
        child_ids: &[u32],
    ) -> Result<u16, Error> {
        if !self.enabled {
            return Err(Error::Refused);
        }
        self._ram.read(0, &mut self.blob)?;
        self.pair
            .sync_from_blob(&self.blob)
            .map_err(|_| Error::Quarantined)?;
        let old_tail = self.pair.send.descriptor.tail;
        let result = self.submission.register_v70(
            &mut self.pair,
            info,
            policy,
            lease,
            child_lrcas,
            child_ids,
        );
        if self.pair.send.descriptor.tail != old_tail {
            self.publish_ctb(bus).map_err(|_| Error::Quarantined)?;
        }
        result.map_err(|_| Error::Refused)
    }

    pub(super) fn submit_guc_context_request(
        &mut self,
        bus: &impl GtIo,
        context_id: u32,
        is_parent: bool,
    ) -> Result<([u16; 2], usize), Error> {
        if !self.enabled {
            return Err(Error::Refused);
        }
        self._ram.read(0, &mut self.blob)?;
        self.pair
            .sync_from_blob(&self.blob)
            .map_err(|_| Error::Quarantined)?;
        let old_tail = self.pair.send.descriptor.tail;
        let result = self
            .submission
            .submit_request(&mut self.pair, context_id, is_parent);
        if self.pair.send.descriptor.tail != old_tail {
            self.publish_ctb(bus).map_err(|_| Error::Quarantined)?;
        }
        result.map_err(|_| Error::Refused)
    }

    /// Receive one CTB message after a GuC G2H interrupt/poll notification and
    /// dispatch submission completion events before publishing the new head.
    /// IRQ/tasklet registration remains owned by the GT event integration.
    pub(super) fn receive_guc_submission_event(&mut self, bus: &impl GtIo) -> Result<bool, Error> {
        if !self.enabled {
            return Err(Error::Refused);
        }
        self._ram.read(0, &mut self.blob)?;
        self.pair
            .sync_from_blob(&self.blob)
            .map_err(|_| Error::Quarantined)?;
        if self.pair.take_unused_receive_status_seen() {
            axlog::warn!("intel-gt: unexpected GuC G2H after CT shutdown (UNUSED status)");
        }
        let words = match self
            .pair
            .receive
            .read_message()
            .map_err(|_| Error::Quarantined)?
        {
            Some(words) => words,
            None => return Ok(false),
        };
        let event = self
            .pair
            .handle_incoming_message(&words)
            .map_err(|_| Error::Quarantined)?;
        let old_send_tail = self.pair.send.descriptor.tail;
        let dispatch = match event {
            Some(event)
                if matches!(
                    event.action,
                    intel_gt::guc_ct::ACTION_SCHED_CONTEXT_MODE_DONE
                        | intel_gt::guc_ct::ACTION_DEREGISTER_CONTEXT_DONE
                ) =>
            {
                self.submission.handle_event(&mut self.pair, event)
            }
            Some(event) => Err(intel_gt::guc_ct::CtError::InvalidMessage),
            None => Err(intel_gt::guc_ct::CtError::InvalidMessage),
        };
        self.pair
            .sync_to_blob(&mut self.blob)
            .map_err(|_| Error::Quarantined)?;
        self._ram.write(0, &self.blob)?;
        self._ram.flush();
        if self.pair.send.descriptor.tail != old_send_tail {
            intel_gt::guc_fw::notify(bus)?;
        }
        dispatch.map_err(|_| Error::Quarantined)?;
        Ok(true)
    }
}

#[cfg(target_os = "none")]
fn upload_huc_for_auth(
    owner: &mut super::Owner,
    image: &intel_gt::uc::FirmwareImage,
) -> Result<(UcDmaMemory, u32), Error> {
    if image.kind != intel_gt::uc::Kind::HuC
        || image.bytes.is_empty()
        || image.bytes.len() > 2 * 1024 * 1024
    {
        return Err(Error::Refused);
    }
    let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
    let pages = image
        .bytes
        .len()
        .checked_add(PAGE - 1)
        .ok_or(Error::Refused)?
        / PAGE;
    let ram = Ram::allocate(pages)?;
    ram.write(0, &image.bytes)?;
    ram.flush();
    let binding = gtt
        .bind_pages(&ram.physical)
        .map_err(|_| Error::Quarantined)?;
    let rsa_offset = image
        .css
        .header_bytes
        .checked_add(image.css.microcode_bytes)
        .and_then(|offset| u32::try_from(offset).ok())
        .and_then(|offset| u32::try_from(binding.address).ok()?.checked_add(offset));
    let Some(rsa_offset) = rsa_offset else {
        // SAFETY: no firmware transfer has started, so the binding is idle.
        if unsafe { gtt.release_binding(&binding) }.is_err() {
            owner.uc_memory = Some(UcDmaMemory {
                _gtt: gtt,
                _ram: ram,
                _binding: binding,
            });
            return Err(Error::Quarantined);
        }
        return Err(Error::Refused);
    };
    if image
        .change_status(intel_gt::uc::FirmwareStatus::Loadable)
        .is_err()
    {
        // SAFETY: HuC DMA has not started; the binding has no device consumer.
        if unsafe { gtt.release_binding(&binding) }.is_err() {
            owner.uc_memory = Some(UcDmaMemory {
                _gtt: gtt,
                _ram: ram,
                _binding: binding,
            });
            return Err(Error::Quarantined);
        }
        return Err(Error::Refused);
    }
    if let Err(error) = intel_gt::guc_fw::huc_upload(&owner.bus, binding.address, image, false) {
        let _ = image.change_status(intel_gt::uc::FirmwareStatus::LoadFail);
        let release_failed = if error == Error::Quarantined {
            true
        } else {
            // SAFETY: non-quarantine errors mean the DMA transfer was never
            // started or was observed complete before returning.
            unsafe { gtt.release_binding(&binding) }.is_err()
        };
        if release_failed {
            owner.uc_memory = Some(UcDmaMemory {
                _gtt: gtt,
                _ram: ram,
                _binding: binding,
            });
            return Err(Error::Quarantined);
        }
        return Err(error);
    }
    if image
        .change_status(intel_gt::uc::FirmwareStatus::Transferred)
        .is_err()
    {
        owner.uc_memory = Some(UcDmaMemory {
            _gtt: gtt,
            _ram: ram,
            _binding: binding,
        });
        return Err(Error::Quarantined);
    }
    Ok((
        UcDmaMemory {
            _gtt: gtt,
            _ram: ram,
            _binding: binding,
        },
        rsa_offset,
    ))
}

#[cfg(target_os = "none")]
// upstream: intel_uc_fw.c intel_uc_fw_upload()
fn upload_uc_one(
    owner: &mut super::Owner,
    image: &intel_gt::uc::FirmwareImage,
) -> Result<(), Error> {
    if image.bytes.is_empty() || image.bytes.len() > 2 * 1024 * 1024 {
        return Err(Error::Refused);
    }
    let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
    let pages = image
        .bytes
        .len()
        .checked_add(PAGE - 1)
        .ok_or(Error::Refused)?
        / PAGE;
    let ram = Ram::allocate(pages)?;
    ram.write(0, &image.bytes)?;
    ram.flush();
    let binding = gtt
        .bind_pages(&ram.physical)
        .map_err(|_| Error::Quarantined)?;
    if image
        .change_status(intel_gt::uc::FirmwareStatus::Loadable)
        .is_err()
    {
        // SAFETY: firmware DMA has not started; the binding has no device consumer.
        if unsafe { gtt.release_binding(&binding) }.is_err() {
            owner.uc_memory = Some(UcDmaMemory {
                _gtt: gtt,
                _ram: ram,
                _binding: binding,
            });
            return Err(Error::Quarantined);
        }
        return Err(Error::Refused);
    }
    let transfer = match image.kind {
        intel_gt::uc::Kind::HuC => {
            intel_gt::guc_fw::huc_upload(&owner.bus, binding.address, image, false)
        }
        intel_gt::uc::Kind::GuC => intel_gt::guc_fw::guc_upload(&owner.bus, binding.address, image)
            .map(|_| ())
            .map_err(|error| match error {
                intel_gt::guc_fw::LoadError::Io(error) => error,
                intel_gt::guc_fw::LoadError::Firmware { status, failure } => {
                    failure.into_error(status)
                }
                intel_gt::guc_fw::LoadError::Timeout { .. } => Error::Timeout(0xc000),
            }),
    };
    if let Err(error) = transfer {
        let _ = image.change_status(intel_gt::uc::FirmwareStatus::LoadFail);
        let release_failed = if error == Error::Quarantined {
            true
        } else {
            // SAFETY: non-quarantine errors are returned only after DMA is
            // either not started or its completion was observed.
            unsafe { gtt.release_binding(&binding) }.is_err()
        };
        if release_failed {
            owner.uc_memory = Some(UcDmaMemory {
                _gtt: gtt,
                _ram: ram,
                _binding: binding,
            });
            return Err(Error::Quarantined);
        }
        return Err(error);
    }
    if image
        .change_status(intel_gt::uc::FirmwareStatus::Transferred)
        .is_err()
    {
        owner.uc_memory = Some(UcDmaMemory {
            _gtt: gtt,
            _ram: ram,
            _binding: binding,
        });
        return Err(Error::Quarantined);
    }
    if image.kind == intel_gt::uc::Kind::GuC
        && image
            .change_status(intel_gt::uc::FirmwareStatus::Running)
            .is_err()
    {
        owner.uc_memory = Some(UcDmaMemory {
            _gtt: gtt,
            _ram: ram,
            _binding: binding,
        });
        return Err(Error::Quarantined);
    }
    // SAFETY: GuC/HuC DMA completion has been observed before success returns.
    if unsafe { gtt.release_binding(&binding) }.is_err() {
        owner.uc_memory = Some(UcDmaMemory {
            _gtt: gtt,
            _ram: ram,
            _binding: binding,
        });
        return Err(Error::Quarantined);
    }
    Ok(())
}

/// Upload HuC then GuC and authenticate it as in intel_uc_init_hw(). ADL-N
/// defaults to HuC authentication plus GuC submission; submission setup is a
/// separate stage and must not be treated as complete by this loader.
#[cfg(target_os = "none")]
// upstream: intel_uc.c __uc_init_hw()
pub(super) fn upload_uc_firmware(
    owner: &mut super::Owner,
    guc: &intel_gt::uc::FirmwareImage,
    huc: &intel_gt::uc::FirmwareImage,
) -> Result<(), Error> {
    if owner.lost
        || owner.memory.is_some()
        || owner.uc_memory.is_some()
        || owner.ct_memory.is_some()
    {
        return Err(Error::Quarantined);
    }
    super::super::dma::require_direct(owner.bdf).map_err(|_| Error::Refused)?;
    if !owner.bus.awake.load(Ordering::Acquire) {
        return Err(Error::Refused);
    }
    let guc_upload_size = guc
        .css
        .header_bytes
        .checked_add(guc.css.microcode_bytes)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or(Error::Refused)?;
    let huc_upload_size = huc
        .css
        .header_bytes
        .checked_add(huc.css.microcode_bytes)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or(Error::Refused)?;
    intel_gt::wopcm::initialize_gen12(&owner.bus, guc_upload_size, huc_upload_size, true, false)
        .map_err(|_| Error::Quarantined)?;
    intel_gt::reset::reset_guc(&owner.bus, (12, 0)).map_err(|_| Error::Quarantined)?;
    // The loader must publish pinned ADS/log addresses before the GuC image is
    // DMA'd, because the firmware consumes these GUC_CTL scratch values at
    // boot. This first runtime profile is exact N305/ADL-N only; it is derived
    // from the validated PCI identity, GT topology, media fuses and GuC CSS.
    if owner.platform == intel_gt::uc::Platform::AlderLakeN {
        let input = n305_guc_ads_input(owner, guc)?;
        AdsDmaMemory::initialize(owner, input)?;
        LogDmaMemory::initialize(owner, false, false)?;
        let ads_address = owner
            .ads_memory
            .as_ref()
            .ok_or(Error::Quarantined)?
            .ggtt_address()?;
        let log = owner
            .log_memory
            .as_ref()
            .ok_or(Error::Quarantined)?
            .config();
        let options = intel_gt::guc_config::gen12_options(
            owner.platform,
            0x46d0,
            0,
            guc.css.version,
            ads_address,
            log,
        );
        write_guc_init_params(owner, options)?;
    }
    let (huc_memory, rsa_offset) = upload_huc_for_auth(owner, huc)?;
    if let Err(error) = upload_uc_one(owner, guc) {
        let _ = huc.change_status(intel_gt::uc::FirmwareStatus::LoadFail);
        if let Err(memory) = huc_memory.release() {
            owner.uc_memory = Some(memory);
            return Err(Error::Quarantined);
        }
        return Err(error);
    }
    let authentication = intel_gt::huc::authenticate_by_guc(&owner.bus, rsa_offset)
        .map(|_| ())
        .map_err(|_| Error::Quarantined);
    if let Err(error) = authentication {
        let _ = huc.change_status(intel_gt::uc::FirmwareStatus::LoadFail);
        owner.uc_memory = Some(huc_memory);
        return Err(error);
    }
    if huc
        .change_status(intel_gt::uc::FirmwareStatus::Running)
        .is_err()
    {
        owner.uc_memory = Some(huc_memory);
        return Err(Error::Quarantined);
    }
    if let Err(memory) = huc_memory.release() {
        owner.uc_memory = Some(memory);
        return Err(Error::Quarantined);
    }
    if intel_gt::uc::default_enable_mask(owner.platform) & intel_gt::uc::ENABLE_GUC_SUBMISSION != 0
    {
        CtDmaMemory::initialize(owner)?;
        owner
            .ct_memory
            .as_mut()
            .ok_or(Error::Quarantined)?
            .verify_ctb_roundtrip(&owner.bus)
            .map_err(|_| Error::Quarantined)?;
    }
    Ok(())
}

/// Construct only the GuC boot-time ADS inventory for the exact supported
/// Alder Lake-N device.  The engine list follows `adl_p_info`'s platform mask
/// and is pruned with the Gen11+ media-enable fuse; GT topology and doorbell
/// count are read from hardware.  Dynamic MMIO regsets/default LRC images are
/// intentionally left empty here; those are task-3 runtime initialization,
/// not fabricated values for the firmware loader.
/// upstream: intel_guc_ads.c __guc_ads_init()/fill_engine_enable_masks().
#[cfg(target_os = "none")]
fn n305_guc_ads_input(
    owner: &super::Owner,
    guc: &intel_gt::uc::FirmwareImage,
) -> Result<intel_gt::guc_ads::AdsBuildInput, Error> {
    use intel_gt::guc_ads::{AdsBuildInput, AdsRuntimeInfo, EngineMapEntry};

    if owner.platform != intel_gt::uc::Platform::AlderLakeN {
        return Err(Error::Refused);
    }
    let topology = intel_gt::info::Topology::read(&owner.bus)?;
    let media_fuse = !owner.bus.read(0x9140)?;
    let vdbox_mask = media_fuse & 0xff;
    let vebox_mask = (media_fuse >> 16) & 0xf;
    let doorbell_count = ((owner.bus.read(0xd08)? >> 16) & 0xff) + 1;

    let mut engines = vec![
        EngineMapEntry {
            guc_class: 0,
            instance: 0,
            logical_index: 0,
        },
        EngineMapEntry {
            guc_class: 3,
            instance: 0,
            logical_index: 0,
        },
    ]; // RCS0 + BCS0
    // `setup_logical_ids()` compacts enabled VDBOX instances in physical map
    // order {0, 2, 4, 6, 1, 3, 5, 7}; ADL-N's platform mask contains VCS0 and
    // VCS2, so VCS2's GuC logical index is 1 while its physical instance is 2.
    let mut logical_video = 0u8;
    let mut sfc_mask = 0u32;
    for instance in [0u8, 2] {
        if vdbox_mask & (1 << instance) != 0 {
            engines.push(EngineMapEntry {
                guc_class: 1,
                instance,
                logical_index: logical_video,
            });
            logical_video += 1;
            // On Gen12, each enabled even physical VDBOX is attached to SFC.
            sfc_mask |= 1 << instance;
        }
    }
    if vebox_mask & 1 != 0 {
        engines.push(EngineMapEntry {
            guc_class: 2,
            instance: 0,
            logical_index: 0,
        });
    }
    let enabled_masks = intel_gt::guc_ads::fill_engine_enable_masks(&engines)?;
    let render_context = 14 * PAGE;
    let other_context = 2 * PAGE;
    let skip = intel_gt::guc_ads::lrc_skip_size(12, 0);
    let mut engine_context_sizes = Vec::new();
    for class in 0u8..=3 {
        if enabled_masks[usize::from(class)] != 0 {
            let total = if class == 0 {
                render_context
            } else {
                other_context
            };
            engine_context_sizes.push((class, total.checked_sub(skip).ok_or(Error::Refused)?));
        }
    }
    let mut generic_gt_sysinfo = [0; intel_gt::guc_ads::GUC_GENERIC_GT_SYSINFO_MAX];
    // `Topology::read()` has verified the sole Gen12.0 slice-enable register
    // value (bit 0 only), so use the actual slice count rather than DSS count.
    generic_gt_sysinfo[0] = 1;
    generic_gt_sysinfo[1] = sfc_mask;
    generic_gt_sysinfo[2] = doorbell_count;
    let css = intel_gt::uc::guc_css_info(guc.css.version, guc.css);
    Ok(AdsBuildInput {
        base_ggtt: PAGE as u32,
        reset_parameter: 2,
        runtime: AdsRuntimeInfo {
            generic_gt_sysinfo,
            graphics_ip_major: 12,
            graphics_ip_minor: 0,
            media_ip_major: 12,
            media_ip_minor: 0,
            firmware_version: guc.css.version,
            dgfx: false,
        },
        engines,
        regsets: Vec::new(),
        engine_context_sizes,
        golden_contexts: Vec::new(),
        capture_lists: Vec::new(),
        private_data_size: css.private_data_bytes,
    })
}

impl Memory {
    fn allocate(gtt: Arc<Gtt>) -> Result<Self, Error> {
        let mut bindings = Vec::new();
        bindings.try_reserve_exact(3).map_err(|_| Error::Refused)?;
        Ok(Self {
            gtt,
            tables: Arc::try_new(Ram::allocate(8)?).map_err(|_| Error::Refused)?,
            extra_tables: Arc::new(Vec::new()),
            vm: None,
            retained: None,
            user: None,
            resident: Vec::new(),
            batch_address: 0x30000,
            table_charge: None,
            source: Ram::allocate(6)?,
            destination: Ram::allocate(6)?,
            context: Arc::try_new(Ram::allocate(4)?).map_err(|_| Error::Refused)?,
            ring: Ram::allocate(1)?,
            batch: Ram::allocate(1)?,
            status: Ram::allocate(1)?,
            bindings,
            descriptor: 0,
            operation: bcs::Copy {
                source: 0x11000,
                destination: 0x21000,
                source_bytes: PAYLOAD as u64,
                destination_bytes: PAYLOAD as u64,
                width: 64,
                height: 64,
                pitch: 256,
            },
            selftest: true,
            render: false,
            idle: false,
            saved: None,
            switch: None,
        })
    }
    fn from_objects(
        gtt: Arc<Gtt>,
        source: Arc<SharedPages>,
        destination: Arc<SharedPages>,
        operation: bcs::Copy,
    ) -> Result<Self, Error> {
        if Arc::ptr_eq(&source, &destination) {
            return Err(Error::Refused);
        }
        let source = Ram::from_pages(source)?;
        let destination = Ram::from_pages(destination)?;
        let batch = bcs::batch(operation)?;
        bcs::decode_copy(
            batch[3..].try_into().unwrap(),
            (source.physical.len() * PAGE) as u64,
            (destination.physical.len() * PAGE) as u64,
        )?;
        let mut memory = Self::allocate(gtt)?;
        memory.source = source;
        memory.destination = destination;
        memory.operation = operation;
        memory.selftest = false;
        Ok(memory)
    }
    fn bind_and_build(&mut self) -> Result<(), Error> {
        for r in [&*self.context, &self.ring, &self.status] {
            self.bindings.push(
                self.gtt
                    .bind_pages(&r.physical)
                    .map_err(|_| Error::Quarantined)?,
            );
        }
        let ctx = self.bindings[0].address as u32;
        let ring = self.bindings[1].address as u32;
        let p = &self.tables.physical;
        let mut table_data = zero_words::<u64>(512)?;
        let table: &mut [u64; 512] = table_data.as_mut_slice().try_into().unwrap();
        // Main root/PDPT/PD/PT and scratch PDPT/PD/PT/data. All unused VA
        // branches reach owned read-only scratch, not arbitrary RAM or zero.
        table.fill(ppgtt::pde(p[5])?);
        self.tables.table(4, table)?;
        table.fill(ppgtt::pde(p[6])?);
        self.tables.table(5, table)?;
        ppgtt::leaf(table, p[7], 3)?;
        self.tables.table(6, table)?;
        let stash = if let Some(user) = &self.user {
            self.resident
                .try_reserve_exact(user.objects.len())
                .map_err(|_| Error::Refused)?;
            for object in &user.objects {
                self.resident.push(Ram::from_pages(object.pages.clone())?);
            }
            let mut mappings = Vec::new();
            mappings
                .try_reserve_exact(self.resident.len())
                .map_err(|_| Error::Refused)?;
            for (ram, object) in self.resident.iter().zip(&user.objects) {
                mappings.push(sparse::Mapping {
                    address: object.address,
                    pages: &ram.physical,
                    writable: object.writable,
                    pat: if object
                        .cache
                        .as_ref()
                        .is_some_and(|c| c.load(Ordering::Acquire) == 1)
                    {
                        0
                    } else {
                        3
                    },
                });
            }
            sparse::populate(&self.tables, &mappings, self.table_charge.as_ref())?
        } else {
            sparse::populate(
                &self.tables,
                &[
                    sparse::Mapping {
                        address: 0x10000,
                        pages: &self.source.physical,
                        writable: false,
                        pat: 3,
                    },
                    sparse::Mapping {
                        address: 0x20000,
                        pages: &self.destination.physical,
                        writable: true,
                        pat: 3,
                    },
                    sparse::Mapping {
                        address: 0x30000,
                        pages: &self.batch.physical,
                        writable: false,
                        pat: 3,
                    },
                ],
                self.table_charge.as_ref(),
            )?
        };
        let sparse::Stash { tables, root } = stash;
        self.extra_tables = Arc::try_new(tables).map_err(|_| Error::Refused)?;
        if self.vm.is_some() {
            let mut pages = Vec::new();
            pages
                .try_reserve_exact(self.resident.len() + 3)
                .map_err(|_| Error::Refused)?;
            for ram in &self.resident {
                pages.push(ram.pages.clone());
            }
            if self.user.is_none() {
                pages.extend([
                    self.source.pages.clone(),
                    self.destination.pages.clone(),
                    self.batch.pages.clone(),
                ]);
            }
            let retained = Arc::try_new(Residency {
                _tables: self.extra_tables.clone(),
                _pages: pages,
            })
            .map_err(|_| Error::Refused)?;
            self.retained = Some(retained);
        }
        self.tables.table(0, root.as_slice().try_into().unwrap())?;
        self.tables.flush();
        if let Some(vm) = &self.vm {
            *vm.residency.lock() = self.retained.clone();
        }
        let mut regs_data = zero_words::<u32>(1024)?;
        let mut indirect_data = zero_words::<u32>(1024)?;
        let mut per_data = zero_words::<u32>(1024)?;
        let regs: &mut [u32; 1024] = regs_data.as_mut_slice().try_into().unwrap();
        let indirect: &mut [u32; 1024] = indirect_data.as_mut_slice().try_into().unwrap();
        let per_ctx: &mut [u32; 1024] = per_data.as_mut_slice().try_into().unwrap();
        let restore = self
            .saved
            .as_ref()
            .is_some_and(|s| s.valid.load(Ordering::Acquire));
        if restore {
            let mut bytes = [0; 4096];
            self.context.read(PAGE, &mut bytes)?;
            for (word, bytes) in regs.iter_mut().zip(bytes.as_chunks::<4>().0) {
                *word = u32::from_le_bytes(*bytes);
            }
        }
        self.descriptor = if self.render {
            if restore {
                intel_gt::rcs::restore_context(regs, indirect, per_ctx, ctx, ring, 78 * 4, p[0])?
            } else {
                intel_gt::rcs::build_context(regs, indirect, per_ctx, ctx, ring, 78 * 4, p[0])?
            }
        } else {
            if restore {
                lrc::restore_context(regs, indirect, per_ctx, ctx, ring, 120, p[0])?
            } else {
                lrc::build(regs, indirect, per_ctx, ctx, ring, 120, p[0])?
            }
        };
        self.context.write(0, &[0; 4096])?;
        self.context.dwords(1, regs)?;
        self.context
            .dwords(if self.render { 14 } else { 2 }, indirect)?;
        self.context
            .dwords(if self.render { 15 } else { 3 }, per_ctx)?;
        let batch = bcs::batch(self.operation)?;
        if self.user.is_none() {
            if self.render {
                self.batch.dwords(0, &intel_gt::rcs_page::PAGE)?;
            } else {
                self.batch.dwords(0, &batch)?;
            }
        }
        let count = bcs::ring(regs, self.batch_address, ctx, 1)?;
        if self.idle {
            regs[15..18].fill(0);
        }
        self.ring.dwords(0, &regs[..count])?;
        if self.selftest {
            let mut data = Vec::new();
            data.try_reserve_exact(6 * PAGE)
                .map_err(|_| Error::Refused)?;
            data.resize(6 * PAGE, 0xa5);
            for (i, b) in data[PAGE..PAGE + PAYLOAD].iter_mut().enumerate() {
                *b = self.pattern(i);
            }
            self.source.write(0, &data)?;
            data.fill(0x5a);
            data[PAGE..PAGE + PAYLOAD].fill(0);
            self.destination.write(0, &data)?;
        }
        self.status.write(0x10 * 4, &[0xff; 12 * 8])?;
        self.status.write(0x2f * 4, &11u32.to_le_bytes())?;
        for r in [
            &*self.tables,
            &self.source,
            &self.destination,
            &*self.context,
            &self.ring,
            &self.batch,
            &self.status,
        ] {
            r.flush();
        }
        for ram in &self.resident {
            ram.flush();
        }
        Ok(())
    }
    fn pattern(&self, i: usize) -> u8 {
        if self.render && i % 4 == 3 {
            255
        } else {
            pattern(i)
        }
    }
    fn render_ring(&self, io: &impl GtIo) -> Result<(), Error> {
        let mut words = [0u32; 78];
        let mut normal = [0u32; 42];
        intel_gt::rcs::ring(
            &mut normal,
            self.batch_address,
            self.bindings[0].address as u32,
            1,
        )?;
        words[..22].copy_from_slice(&normal[..22]);
        words[1] |= 1 << 27; // before-WA full barrier.
        intel_gt::rcs::context_wa(io, &mut words[22..36])?;
        words[36..].copy_from_slice(&normal);
        words[37] |= 1 << 27; // after-WA full barrier.
        if self.idle {
            words[36 + 23..36 + 26].fill(0);
        }
        self.ring.dwords(0, &words)?;
        self.ring.flush();
        Ok(())
    }
    fn verify(&self) -> Result<(), Error> {
        self.source.flush();
        self.destination.flush();
        let mut data = Vec::new();
        data.try_reserve_exact(6 * PAGE)
            .map_err(|_| Error::Refused)?;
        data.resize(6 * PAGE, 0);
        for (r, guard) in [(&self.source, 0xa5), (&self.destination, 0x5a)] {
            r.read(0, &mut data)?;
            if data[..PAGE]
                .iter()
                .chain(&data[PAGE + PAYLOAD..])
                .any(|&b| b != guard)
                || data[PAGE..PAGE + PAYLOAD]
                    .iter()
                    .enumerate()
                    .any(|(i, &b)| b != self.pattern(i))
            {
                return Err(Error::Refused);
            }
        }
        Ok(())
    }
    fn release(&mut self) -> Result<(), Error> {
        if let Some(switch) = self.switch.as_mut() {
            switch.release(&self.gtt)?;
        }
        for binding in self.bindings.iter().rev() {
            // SAFETY: only reached after successful source selected-engine stop/reset,
            // pending-MI-wake/ready/GDRST/cancel checks; no other engine sees
            // this private context, ring, status or PPGTT. RAM stays pinned.
            unsafe { self.gtt.release_binding(binding) }.map_err(|_| Error::Quarantined)?;
        }
        self.bindings.clear();
        Ok(())
    }
}
fn zero_words<T: Default + Clone>(count: usize) -> Result<Vec<T>, Error> {
    let mut v = Vec::new();
    v.try_reserve_exact(count).map_err(|_| Error::Refused)?;
    v.resize(count, T::default());
    Ok(v)
}
fn pattern(index: usize) -> u8 {
    (index as u8).wrapping_mul(29) ^ ((index >> 8) as u8) ^ 0x73
}

fn submit(io: &impl GtIo, memory: &Memory) -> Result<(), Error> {
    let base = if memory.render { 0x2000 } else { 0x22000 };
    let status = memory.bindings[2].address as u32;
    // Polling selftest masks engine IRQs; USER_INTERRUPT cannot become an
    // unowned CPU IRQ. Preserve the source HWSTAM and error-clear setup.
    io.write(base + 0x0a8, u32::MAX)?;
    io.write(base + 0x098, u32::MAX)?;
    io.write(base + 0x0b4, u32::MAX)?;
    io.write(base + 0x0b0, u32::MAX)?;
    if io.read(base + 0x0b8)? != 0 {
        return Err(Error::Refused);
    }
    io.write(
        base + 0x29c,
        intel_gt::masked_enable(1 << 3) | intel_gt::masked_disable(1 << 10),
    )?;
    io.write(base + 0x09c, intel_gt::masked_disable(1 << 8))?;
    io.write(base + 0x080, status)?;
    if io.read(base + 0x080)? != status {
        return Err(Error::Refused);
    }
    io.write(base + 0x0c4, (0x3fff << 16) | 6 | (6 << 7))?; // source UC index3 read/write.
    // Source CSB read/write pointer reset: Gen11 twelve slots, invalid index11.
    io.write(base + 0x3a0, 0xffff0000 | (11 << 8) | 11)?;
    io.read(base + 0x3a0)?;
    io.write(base + 0x3a0, 0xffff0000 | (11 << 8) | 11)?;
    io.read(base + 0x3a0)?;
    fence(Ordering::SeqCst);
    // Gen12 ELSQ writes port1 then port0, low then high, then explicit load.
    let second = memory.switch.as_ref().map_or(0, |s| s.descriptor);
    io.write(base + 0x518, second as u32)?;
    io.write(base + 0x51c, (second >> 32) as u32)?;
    io.write(base + 0x510, memory.descriptor as u32)?;
    io.write(base + 0x514, (memory.descriptor >> 32) as u32)?;
    io.write(base + 0x550, 1)?;
    let start = io.now_us();
    let mut value = [0u8; 4];
    for _ in 0..100_000 {
        let finished = memory
            .switch
            .as_ref()
            .map_or(&*memory.context, |s| &s.context);
        finished.flush();
        finished.read(lrc::SCRATCH as usize, &mut value)?;
        if u32::from_le_bytes(value) == 1 {
            memory.context.flush();
            if memory.switch.is_some() {
                let mut first = [0; 4];
                memory.context.read(lrc::SCRATCH as usize, &mut first)?;
                if u32::from_le_bytes(first) != 1 {
                    return Err(Error::Refused);
                }
            }
            fence(Ordering::SeqCst);
            return Ok(());
        }
        if io.read(base + 0x0b8)? != 0 {
            return Err(Error::Refused);
        }
        if io.now_us().saturating_sub(start) > 500_000 {
            return Err(Error::Timeout(base + 0x550));
        }
        io.delay_us(10);
    }
    Err(Error::Timeout(base + 0x550))
}

// Outer error means quiescence was not established: caller MUST retain all
// DMA owners. An inner error can be reported after safe scoped unbinding.
fn execute_and_quiesce(io: &impl GtIo, memory: &Memory) -> Result<Result<(), Error>, Error> {
    if let Some(saved) = &memory.saved {
        saved.valid.store(false, Ordering::Release);
    }
    let executed = submit(io, memory);
    if memory.render {
        intel_gt::reset::stop_and_reset_rcs(io)
    } else {
        intel_gt::reset::stop_and_reset_bcs(io)
    }
    .map_err(|_| Error::Quarantined)?;
    // WB userspace mappings may have refilled while DMA was in flight. UC GPU
    // stores must be visible before the GEM completion is signaled, not only
    // in the bootstrap's optional byte verifier. Retirement precedes invalidate.
    memory.destination.flush();
    for ram in &memory.resident {
        ram.flush();
    }
    if let Some(saved) = &memory.saved {
        saved.valid.store(executed.is_ok(), Ordering::Release);
    }
    Ok(executed.and_then(|()| {
        if memory.selftest {
            memory.verify()
        } else {
            Ok(())
        }
    }))
}

#[cfg(target_os = "none")]
pub(super) fn run(owner: &mut super::Owner, bdf: pci::Bdf) -> Result<(), Error> {
    super::super::dma::require_direct(bdf).map_err(|_| Error::Refused)?;
    intel_gt::uncore::acquire_render(&owner.bus)?;
    owner.bus.render_awake.store(true, Ordering::Release);
    owner.bus.acquire_idle_media()?;
    // Never change shared cache policy while an abandoned firmware RCS is busy.
    let start = owner.bus.now_us();
    while !intel_gt::uncore::ring_idle(&owner.bus, 0x2000)? {
        if owner.bus.now_us().saturating_sub(start) > 100_000 {
            return Err(Error::Refused);
        }
        owner.bus.delay_us(10);
    }
    owner.bus.prepare_shared()?;
    let gtt = super::super::shared_ggtt(bdf).map_err(|_| Error::Refused)?;
    owner.memory = Some(Memory::allocate(gtt)?);
    let memory = owner.memory.as_mut().unwrap();
    memory.bind_and_build()?;
    let ecam = pci::Ecam::platform().ok_or(Error::Refused)?;
    ecam.enable_n305_bus_master(bdf).ok_or(Error::Refused)?;
    // Breadcrumb does not establish context/page-table retirement. The
    // source stop/reset must succeed even after a possibly-landed ELSQ error.
    let verified = execute_and_quiesce(&owner.bus, memory)?;
    memory.release()?;
    owner.memory = None;
    verified?;
    record_defaults(owner, false)
}

#[cfg(target_os = "none")]
fn record_defaults(owner: &mut super::Owner, render: bool) -> Result<(), Error> {
    if owner.memory.is_some() || owner.lost {
        return Err(Error::Quarantined);
    }
    if render {
        intel_gt::reset::stop_and_reset_rcs(&owner.bus)?;
    } else {
        intel_gt::reset::stop_and_reset_bcs(&owner.bus)?;
    }
    owner.bus.prepare_shared()?;
    if render {
        intel_gt::rcs::prepare(&owner.bus)?;
    }
    let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
    let mut memory = Memory::allocate(gtt)?;
    memory.render = render;
    memory.idle = true;
    memory.selftest = false;
    memory.context = Arc::try_new(Ram::allocate(if render {
        intel_gt::rcs::CONTEXT_PAGES
    } else {
        lrc::CONTEXT_PAGES
    })?)
    .map_err(|_| Error::Refused)?;
    memory.switch = Some(Box::try_new(SwitchAway::new(render)?).map_err(|_| Error::Refused)?);
    owner.memory = Some(memory);
    let memory = owner.memory.as_mut().unwrap();
    memory.bind_and_build()?;
    if render {
        memory.render_ring(&owner.bus)?;
    }
    memory.switch.as_mut().unwrap().build(
        &memory.gtt,
        render,
        memory.tables.physical[0],
        &owner.bus,
    )?;
    execute_and_quiesce(&owner.bus, memory)??;
    let captured = memory.context.clone();
    memory.release()?;
    owner.memory = None;
    DEFAULTS.lock()[usize::from(render)] = Some(captured);
    if render {
        owner.bus.prepare_shared()?;
    }
    Ok(())
}

/// Shader-driven 3D rectangle selftest, not BCS or a CPU-copy fallback.
#[cfg(target_os = "none")]
pub(super) fn render_test(owner: &mut super::Owner) -> Result<(), Error> {
    if owner.memory.is_some() || owner.lost {
        return Err(Error::Quarantined);
    }
    super::super::dma::require_direct(owner.bdf).map_err(|_| Error::Refused)?;
    if owner.bus.read(0xc000)? & 1 == 0 || !intel_gt::uncore::ring_idle(&owner.bus, 0x2000)? {
        return Err(Error::Refused);
    }
    owner.bus.rcs_owned.store(true, Ordering::Release);
    record_defaults(owner, true)?;
    intel_gt::reset::stop_and_reset_rcs(&owner.bus)?;
    owner.bus.prepare_shared()?;
    intel_gt::rcs::prepare(&owner.bus)?;
    let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
    let mut memory = Memory::allocate(gtt)?;
    memory.context = Arc::new(Ram::allocate(intel_gt::rcs::CONTEXT_PAGES)?);
    memory.render = true;
    owner.memory = Some(memory);
    let memory = owner.memory.as_mut().unwrap();
    memory.bind_and_build()?;
    memory.render_ring(&owner.bus)?;
    let verified = execute_and_quiesce(&owner.bus, memory)?;
    memory.release()?;
    owner.memory = None;
    // Render reset may lose shared render-domain L3 policy. Restore the single
    // UC policy used by subsequent BCS jobs while all engines remain stopped.
    owner.bus.prepare_shared()?;
    verified
}

#[cfg(target_os = "none")]
pub(super) fn render_objects(
    owner: &mut super::Owner,
    source: Arc<SharedPages>,
    destination: Arc<SharedPages>,
    vm: Arc<Vm>,
    saved: Arc<SavedContext>,
) -> Result<(), Error> {
    if owner.lost || owner.memory.is_some() {
        return Err(Error::Quarantined);
    }
    super::super::dma::require_direct(owner.bdf).map_err(|_| Error::Refused)?;
    if owner.bus.read(intel_gt::uncore::GT_ACK)? & 1 == 0
        || owner.bus.read(intel_gt::uncore::RENDER_ACK)? & 1 == 0
        || owner.bus.read(0xc000)? & 1 == 0
    {
        return Err(Error::Refused);
    }
    intel_gt::reset::stop_and_reset_rcs(&owner.bus)?;
    owner.bus.prepare_shared()?;
    intel_gt::rcs::prepare(&owner.bus)?;
    let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
    let operation = bcs::Copy {
        source: 0x11000,
        destination: 0x21000,
        source_bytes: PAYLOAD as u64,
        destination_bytes: PAYLOAD as u64,
        width: 64,
        height: 64,
        pitch: 256,
    };
    let mut memory = Memory::from_objects(gtt, source, destination, operation)?;
    memory.tables = vm.tables.clone();
    memory.table_charge = vm.charge.clone();
    memory.vm = Some(vm.clone());
    memory.context = Arc::new(Ram::allocate(intel_gt::rcs::CONTEXT_PAGES)?);
    memory.render = true;
    if saved.render != memory.render {
        return Err(Error::Refused);
    }
    memory.context = saved.ram.clone();
    memory.saved = Some(saved);
    memory.switch =
        Some(Box::try_new(SwitchAway::new(memory.render)?).map_err(|_| Error::Refused)?);
    owner.memory = Some(memory);
    let memory = owner.memory.as_mut().unwrap();
    memory.bind_and_build()?;
    memory.switch.as_mut().unwrap().build(
        &memory.gtt,
        memory.render,
        memory.tables.physical[0],
        &owner.bus,
    )?;
    memory.render_ring(&owner.bus)?;
    let result = execute_and_quiesce(&owner.bus, memory)?;
    memory.release()?;
    owner.memory = None;
    owner.bus.prepare_shared()?;
    result
}

/// Linux gen8_emit_bb_start_noarb uses NON_SECURE bit8 for both engines;
/// Gen12 intel_engine_init_cmd_parser needs no software privileged parser.
/// Objects, page tables, opaque images and idle switch are retained together.
#[cfg(target_os = "none")]
pub(super) fn user_objects(
    owner: &mut super::Owner,
    job: Arc<UserJob>,
    vm: Arc<Vm>,
    saved: Arc<SavedContext>,
) -> Result<(), Error> {
    if owner.lost || owner.memory.is_some() || (job.render && !owner.render_ready) {
        return Err(Error::Refused);
    }
    job.validate()?;
    super::super::dma::require_direct(owner.bdf).map_err(|_| Error::Refused)?;
    owner.bus.assert_media_idle()?;
    if job.render {
        intel_gt::reset::stop_and_reset_rcs(&owner.bus)?;
    } else {
        intel_gt::reset::stop_and_reset_bcs(&owner.bus)?;
    }
    owner.bus.prepare_shared()?;
    if job.render {
        intel_gt::rcs::prepare(&owner.bus)?;
    } else {
        bcs::apply_nonpriv(&owner.bus, false)?;
    }
    intel_gt::cache::prepare(&owner.bus)?;
    if saved.render != job.render {
        return Err(Error::Refused);
    }
    let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
    let mut memory = Memory::allocate(gtt)?;
    memory.tables = vm.tables.clone();
    memory.table_charge = vm.charge.clone();
    memory.vm = Some(vm.clone());
    memory.context = saved.ram.clone();
    memory.saved = Some(saved);
    memory.render = job.render;
    memory.selftest = false;
    memory.batch_address = sparse::normalize(job.batch)?;
    memory.user = Some(job);
    memory.switch =
        Some(Box::try_new(SwitchAway::new(memory.render)?).map_err(|_| Error::Refused)?);
    owner.memory = Some(memory);
    let memory = owner.memory.as_mut().unwrap();
    memory.bind_and_build()?;
    memory.switch.as_mut().unwrap().build(
        &memory.gtt,
        memory.render,
        memory.tables.physical[0],
        &owner.bus,
    )?;
    if memory.render {
        memory.render_ring(&owner.bus)?;
    }
    let result = execute_and_quiesce(&owner.bus, memory)?;
    memory.release()?;
    owner.memory = None;
    owner.bus.prepare_shared()?;
    result
}

/// Scoped synchronous execution over existing GEM SharedPages. Caller owns all
/// GEM Arcs/reservation fences through this call; the GT owner additionally
/// retains the fixed views/page tables on any ambiguous retirement error.
#[cfg(target_os = "none")]
pub(super) fn objects(
    owner: &mut super::Owner,
    source: Arc<SharedPages>,
    destination: Arc<SharedPages>,
    operation: bcs::Copy,
    vm: Arc<Vm>,
    saved: Arc<SavedContext>,
) -> Result<(), Error> {
    if owner.lost || owner.memory.is_some() {
        return Err(Error::Quarantined);
    }
    super::super::dma::require_direct(owner.bdf).map_err(|_| Error::Refused)?;
    operation.validate()?;
    if Arc::ptr_eq(&source, &destination) {
        return Err(Error::Refused);
    }
    if owner.bus.read(intel_gt::uncore::GT_ACK)? & 1 == 0
        || owner.bus.read(intel_gt::uncore::RENDER_ACK)? & 1 == 0
        || owner.bus.read(0xc000)? & 1 == 0
        || owner.bus.read(0x480c)? != 0
        || owner.bus.read(0x400c)? != 5
        || owner.bus.read(0xb024)? >> 16 != 0x10
    {
        return Err(Error::Refused);
    }
    owner.bus.assert_media_idle()?;
    // Last job and bootstrap must already be quiescent; bounded reset also
    // establishes a fresh engine state before loading another private context.
    intel_gt::reset::stop_and_reset_bcs(&owner.bus)?;
    bcs::apply_nonpriv(&owner.bus, false)?;
    let gtt = super::super::shared_ggtt(owner.bdf).map_err(|_| Error::Refused)?;
    let mut memory = Memory::from_objects(gtt, source, destination, operation)?;
    memory.tables = vm.tables.clone();
    memory.table_charge = vm.charge.clone();
    memory.vm = Some(vm.clone());
    if saved.render != memory.render {
        return Err(Error::Refused);
    }
    memory.context = saved.ram.clone();
    memory.saved = Some(saved);
    memory.switch =
        Some(Box::try_new(SwitchAway::new(memory.render)?).map_err(|_| Error::Refused)?);
    owner.memory = Some(memory);
    let memory = owner.memory.as_mut().unwrap();
    memory.bind_and_build()?;
    memory.switch.as_mut().unwrap().build(
        &memory.gtt,
        memory.render,
        memory.tables.physical[0],
        &owner.bus,
    )?;
    let outcome = execute_and_quiesce(&owner.bus, memory)?;
    memory.release()?;
    owner.memory = None;
    outcome
}

#[cfg(test)]
pub(in crate::drm::intel) mod tests {
    use alloc::{boxed::Box, collections::BTreeMap};
    use core::cell::{Cell, RefCell};

    use super::*;
    struct Model<'a> {
        memory: &'a Memory,
        words: RefCell<BTreeMap<u32, u32>>,
        log: RefCell<Vec<(u32, u32)>>,
        clock: Cell<u64>,
        execute: bool,
        fail_write: Cell<Option<usize>>,
    }
    impl Model<'_> {
        fn ram(&self, physical: u64) -> Result<(&Ram, usize), Error> {
            let page = physical & !4095;
            let inside = (physical & 4095) as usize;
            for ram in [
                &*self.memory.tables,
                &self.memory.source,
                &self.memory.destination,
                &*self.memory.context,
                &self.memory.ring,
                &self.memory.batch,
                &self.memory.status,
            ] {
                if let Some(index) = ram.physical.iter().position(|&p| p == page) {
                    return Ok((ram, index * PAGE + inside));
                }
            }
            for ram in self.memory.extra_tables.iter() {
                if let Some(index) = ram.physical.iter().position(|&p| p == page) {
                    return Ok((ram, index * PAGE + inside));
                }
            }
            if let Some(switch) = &self.memory.switch {
                for ram in [&switch.context, &switch.ring] {
                    if let Some(index) = ram.physical.iter().position(|&p| p == page) {
                        return Ok((ram, index * PAGE + inside));
                    }
                }
            }
            Err(Error::Refused)
        }
        fn physical(&self, address: u64, buf: &mut [u8]) -> Result<(), Error> {
            let (ram, offset) = self.ram(address)?;
            ram.read(offset, buf)
        }
        fn translate(&self, virtual_address: u64, write: bool) -> Result<u64, Error> {
            let mut page = self.memory.tables.physical[0];
            for shift in [39, 30, 21, 12] {
                let mut data = [0; 8];
                self.physical(page + ((virtual_address >> shift) & 511) * 8, &mut data)?;
                let entry = u64::from_le_bytes(data);
                if entry & 1 == 0 || (shift == 12 && write && entry & 2 == 0) {
                    return Err(Error::Refused);
                }
                page = entry & (((1u64 << 39) - 1) & !4095);
            }
            Ok(page + (virtual_address & 4095))
        }
        fn switch_away(&self) -> Result<(), Error> {
            let Some(switch) = &self.memory.switch else {
                return Ok(());
            };
            let base = if self.memory.render { 0x2000 } else { 0x22000 };
            let words = self.words.borrow();
            assert_eq!(
                words.get(&(base + 0x518)).copied(),
                Some(switch.descriptor as u32)
            );
            assert_eq!(
                words.get(&(base + 0x51c)).copied(),
                Some((switch.descriptor >> 32) as u32)
            );
            drop(words);
            let mut tail = [0; 4];
            switch.context.read(PAGE + 7 * 4, &mut tail)?;
            assert_eq!(
                u32::from_le_bytes(tail),
                if self.memory.render { 78 * 4 } else { 120 }
            );
            let mut dispatch = [0; 12];
            switch.ring.read(
                if self.memory.render {
                    (36 + 23) * 4
                } else {
                    15 * 4
                },
                &mut dispatch,
            )?;
            assert_eq!(dispatch, [0; 12]);
            // HOST MODEL ONLY. Tags exercise opaque-image ownership/preservation,
            // not GPU register saving, EU execution or physical graphics state.
            if self.memory.render {
                self.memory
                    .context
                    .write(2 * PAGE, &alloc::vec![0x6a;12*PAGE])?;
            }
            self.memory
                .context
                .write(PAGE + 35 * 4, &0x5544u32.to_le_bytes())?;
            switch
                .context
                .write(lrc::SCRATCH as usize, &1u32.to_le_bytes())
        }
        fn gpu(&self) -> Result<(), Error> {
            // This is deliberately a HOST MODEL interpreting the actual private
            // page tables/batch. It cannot establish physical GPU execution.
            let mut ctx = [0; 4];
            self.memory.context.read(PAGE + 49 * 4, &mut ctx)?;
            let high = u32::from_le_bytes(ctx);
            self.memory.context.read(PAGE + 51 * 4, &mut ctx)?;
            let low = u32::from_le_bytes(ctx);
            if (u64::from(high) << 32) | u64::from(low) != self.memory.tables.physical[0] {
                return Err(Error::Refused);
            }
            if self.memory.idle {
                let mut dispatch = [0; 12];
                self.memory.ring.read(
                    if self.memory.render {
                        (36 + 23) * 4
                    } else {
                        15 * 4
                    },
                    &mut dispatch,
                )?;
                assert_eq!(dispatch, [0; 12]);
                return self
                    .memory
                    .context
                    .write(lrc::SCRATCH as usize, &1u32.to_le_bytes());
            }
            if self.memory.render {
                // HOST MODEL only: validate the actual immutable 3D page and
                // private mappings, then model the fixed rectangle transfer.
                // This does not emulate EU instructions or prove GPU rendering.
                for (i, expected) in intel_gt::rcs_page::PAGE.iter().enumerate() {
                    let mut bytes = [0; 4];
                    self.physical(self.translate(0x30000 + i as u64 * 4, false)?, &mut bytes)?;
                    if u32::from_le_bytes(bytes) != *expected {
                        return Err(Error::Refused);
                    }
                }
                for i in 0..PAYLOAD {
                    let mut value = [0];
                    self.physical(self.translate(0x11000 + i as u64, false)?, &mut value)?;
                    let (ram, offset) = self.ram(self.translate(0x21000 + i as u64, true)?)?;
                    ram.write(offset, &value)?;
                }
                return self
                    .memory
                    .context
                    .write(lrc::SCRATCH as usize, &1u32.to_le_bytes());
            }
            let mut words = [0u32; 14];
            for (i, w) in words.iter_mut().enumerate() {
                let mut d = [0; 4];
                self.physical(self.translate(0x30000 + i as u64 * 4, false)?, &mut d)?;
                *w = u32::from_le_bytes(d);
            }
            if words[0] != 0x11000001
                || words[1] != 0x22204
                || words[2] != 0x606
                || words[3] != 0x50800008
                || words[13] != 0x5000000
            {
                return Err(Error::Refused);
            }
            let width = words[6] & 0xffff;
            let height = words[6] >> 16;
            let pitch = words[10];
            let src = u64::from(words[11]) | (u64::from(words[12]) << 32);
            let dst = u64::from(words[7]) | (u64::from(words[8]) << 32);
            for y in 0..height {
                for x in 0..width * 4 {
                    let mut value = [0];
                    self.physical(
                        self.translate(src + u64::from(y * pitch + x), false)?,
                        &mut value,
                    )?;
                    let physical =
                        self.translate(dst + u64::from(y * (words[4] & 0xffff) + x), true)?;
                    let (ram, offset) = self.ram(physical)?;
                    ram.write(offset, &value)?;
                }
            }
            self.memory
                .context
                .write(lrc::SCRATCH as usize, &1u32.to_le_bytes())
        }
    }
    impl GtIo for Model<'_> {
        fn read(&self, r: u32) -> Result<u32, Error> {
            self.clock.set(self.clock.get() + 10_000);
            self.words
                .borrow()
                .get(&r)
                .copied()
                .ok_or(Error::Unavailable(r))
        }
        fn write(&self, r: u32, v: u32) -> Result<(), Error> {
            self.log.borrow_mut().push((r, v));
            let mut words = self.words.borrow_mut();
            if [0x2209c, 0x2229c, 0x220d0, 0x209c, 0x229c, 0x20d0].contains(&r) {
                let old = words.get(&r).copied().unwrap_or(0);
                let value = (old & !(v >> 16)) | (v & (v >> 16));
                words.insert(
                    r,
                    if [0x220d0, 0x20d0].contains(&r) && value & 1 != 0 {
                        value | 2
                    } else {
                        value
                    },
                );
            } else {
                words.insert(r, if r == 0x941c { 0 } else { v });
            }
            drop(words);
            if [0x22550, 0x2550].contains(&r) && self.execute {
                self.gpu()?;
                self.switch_away()?;
            }
            if self.fail_write.get() == Some(self.log.borrow().len()) {
                self.fail_write.set(None);
                return Err(Error::Unavailable(r)); // write may have landed.
            }
            Ok(())
        }
        fn now_us(&self) -> u64 {
            self.clock.get()
        }
        fn delay_us(&self, n: u32) {
            self.clock.set(self.clock.get() + u64::from(n));
        }
    }
    fn memory() -> Memory {
        let array = super::super::super::gtt::mock::MockPageTable::new(65536);
        let gtt = Arc::new(Gtt::over(Box::new(array)).unwrap());
        let mut m = Memory::allocate(gtt).unwrap();
        m.bind_and_build().unwrap();
        m
    }
    fn model(memory: &Memory, execute: bool) -> Model<'_> {
        Model {
            memory,
            words: RefCell::new(BTreeMap::from([
                (0x220b8, 0),
                (0x2209c, 1 << 9),
                (0x2229c, 0),
                (0x220d0, 0),
                (0x800c, 0),
                (0xa2a0, 0),
                (0x941c, 0),
                (0x20b8, 0),
                (0x209c, 1 << 9),
                (0x229c, 0),
                (0x20d0, 0),
                (0x8000, 0),
                (0xfdc, 0x80000000),
                (0x913c, 1),
                (0x5584, 0),
            ])),
            log: RefCell::new(Vec::new()),
            clock: Cell::new(0),
            execute,
            fail_write: Cell::new(None),
        }
    }
    pub(in crate::drm::intel) fn objects(
        source: Arc<SharedPages>,
        destination: Arc<SharedPages>,
        operation: bcs::Copy,
    ) -> Result<(), Error> {
        objects_kind(source, destination, operation, false, None)
    }
    pub(in crate::drm::intel) fn render_objects(
        source: Arc<SharedPages>,
        destination: Arc<SharedPages>,
    ) -> Result<(), Error> {
        objects_kind(
            source,
            destination,
            bcs::Copy {
                source: 0x11000,
                destination: 0x21000,
                source_bytes: PAYLOAD as u64,
                destination_bytes: PAYLOAD as u64,
                width: 64,
                height: 64,
                pitch: 256,
            },
            true,
            None,
        )
    }
    pub(in crate::drm::intel) fn objects_vm(
        source: Arc<SharedPages>,
        destination: Arc<SharedPages>,
        operation: bcs::Copy,
        vm: Arc<Vm>,
    ) -> Result<(), Error> {
        objects_kind(source, destination, operation, false, Some(vm))
    }
    fn objects_kind(
        source: Arc<SharedPages>,
        destination: Arc<SharedPages>,
        operation: bcs::Copy,
        render: bool,
        vm: Option<Arc<Vm>>,
    ) -> Result<(), Error> {
        let array = super::super::super::gtt::mock::MockPageTable::new(65536);
        let gtt = Arc::new(Gtt::over(Box::new(array)).unwrap());
        let mut memory = Memory::from_objects(gtt, source, destination, operation)?;
        if let Some(vm) = vm {
            memory.tables = vm.tables.clone();
            memory.table_charge = vm.charge.clone();
            memory.vm = Some(vm.clone());
        }
        if render {
            memory.context = Arc::new(Ram::allocate(intel_gt::rcs::CONTEXT_PAGES)?);
            memory.render = true;
        }
        memory.bind_and_build()?;
        let io = model(&memory, true);
        if render {
            memory.render_ring(&io)?;
        }
        let outcome = execute_and_quiesce(&io, &memory)?;
        drop(io);
        memory.release()?;
        outcome
    }
    #[test]
    fn rcs_driver_model_uses_real_private_page_context_and_reset_before_result_retirement() {
        let _context = crate::test_support::scheduler_test_context();
        let mut memory = memory();
        memory.release().unwrap();
        memory.context = Arc::new(Ram::allocate(intel_gt::rcs::CONTEXT_PAGES).unwrap());
        memory.render = true;
        memory.bind_and_build().unwrap();
        let io = model(&memory, true);
        memory.render_ring(&io).unwrap();
        execute_and_quiesce(&io, &memory).unwrap().unwrap();
        assert_eq!(
            io.log
                .borrow()
                .iter()
                .filter(|(r, v)| *r == 0x941c && *v == 2)
                .count(),
            2
        );
        assert!(!io.log.borrow().iter().any(|(r, _)| *r == 0x22550));
        drop(io);
        memory.release().unwrap();
        assert!(memory.bindings.is_empty());
    }
    #[test]
    fn rcs_landed_submission_faults_reset_the_render_domain_before_unbinding() {
        let _context = crate::test_support::scheduler_test_context();
        let mut baseline = memory();
        baseline.release().unwrap();
        baseline.context = Arc::new(Ram::allocate(16).unwrap());
        baseline.render = true;
        baseline.bind_and_build().unwrap();
        let io = model(&baseline, true);
        baseline.render_ring(&io).unwrap();
        let before = io.log.borrow().len();
        submit(&io, &baseline).unwrap();
        let count = io.log.borrow().len() - before;
        intel_gt::reset::stop_and_reset_rcs(&io).unwrap();
        drop(io);
        baseline.release().unwrap();
        for prefix in 1..=count {
            let mut m = memory();
            m.release().unwrap();
            m.context = Arc::new(Ram::allocate(16).unwrap());
            m.render = true;
            m.bind_and_build().unwrap();
            let io = model(&m, true);
            m.render_ring(&io).unwrap();
            let before = io.log.borrow().len();
            io.fail_write.set(Some(before + prefix));
            assert!(execute_and_quiesce(&io, &m).unwrap().is_err());
            assert_eq!(
                io.log
                    .borrow()
                    .iter()
                    .filter(|(r, v)| *r == 0x941c && *v == 2)
                    .count(),
                2
            );
            assert_eq!(m.bindings.len(), 3);
            drop(io);
            m.release().unwrap();
        }
    }
    #[test]
    fn completed_rcs_breadcrumb_does_not_authorize_retirement_after_ambiguous_reset() {
        let _context = crate::test_support::scheduler_test_context();
        let mut memory = memory();
        memory.release().unwrap();
        memory.context = Arc::new(Ram::allocate(intel_gt::rcs::CONTEXT_PAGES).unwrap());
        memory.render = true;
        memory.bind_and_build().unwrap();
        let io = model(&memory, true);
        memory.render_ring(&io).unwrap();
        let setup = io.log.borrow().len();
        submit(&io, &memory).unwrap();
        memory.verify().unwrap();
        let writes = io.log.borrow().len() - setup;
        io.fail_write.set(Some(setup + writes * 2 + 1));
        assert_eq!(execute_and_quiesce(&io, &memory), Err(Error::Quarantined));
        assert_eq!(memory.bindings.len(), 3);
        assert!(Arc::strong_count(&memory.context.pages) > 1);
    }
    #[test]
    fn native_copy_submission_model_walks_private_vm_compares_guards_and_retires_after_reset() {
        let _context = crate::test_support::scheduler_test_context();
        let mut memory = memory();
        let io = model(&memory, true);
        assert!(memory.verify().is_err());
        submit(&io, &memory).unwrap();
        memory.verify().unwrap();
        intel_gt::reset::stop_and_reset_bcs(&io).unwrap();
        assert_eq!(
            io.log.borrow().iter().filter(|(r, _)| *r == 0x941c).count(),
            2
        );
        drop(io);
        memory.release().unwrap();
        assert!(memory.bindings.is_empty());
    }
    #[test]
    fn missing_hardware_breadcrumb_is_not_copy_success_and_buffers_remain_owned() {
        let _context = crate::test_support::scheduler_test_context();
        let memory = memory();
        let io = model(&memory, false);
        assert_eq!(submit(&io, &memory), Err(Error::Timeout(0x22550)));
        assert!(memory.verify().is_err());
        assert_eq!(memory.bindings.len(), 3);
        assert!(Arc::strong_count(&memory.context.pages) > 1);
    }
    #[test]
    fn every_possibly_landed_submit_write_is_reset_before_scoped_unbinding() {
        let _context = crate::test_support::scheduler_test_context();
        let baseline = memory();
        let io = model(&baseline, true);
        submit(&io, &baseline).unwrap();
        let writes = io.log.borrow().len();
        drop(io);
        for prefix in 1..=writes {
            let mut memory = memory();
            let io = model(&memory, true);
            io.fail_write.set(Some(prefix));
            assert!(execute_and_quiesce(&io, &memory).unwrap().is_err());
            assert_eq!(
                io.log.borrow().iter().filter(|(r, _)| *r == 0x941c).count(),
                2
            );
            assert_eq!(memory.bindings.len(), 3);
            drop(io);
            memory.release().unwrap();
        }
    }
    #[test]
    fn ambiguous_reset_after_completed_copy_retains_every_dma_owner() {
        let _context = crate::test_support::scheduler_test_context();
        let memory = memory();
        let io = model(&memory, true);
        submit(&io, &memory).unwrap();
        memory.verify().unwrap();
        let writes = io.log.borrow().len();
        // Next invocation fails on reset's first write, even though completed
        // bytes/breadcrumb exist. Completion is NOT permission to free RAM.
        io.fail_write.set(Some(writes * 2 + 1));
        assert_eq!(execute_and_quiesce(&io, &memory), Err(Error::Quarantined));
        assert_eq!(memory.bindings.len(), 3);
        assert!(Arc::strong_count(&memory.context.pages) > 1);
    }
    #[test]
    fn private_vm_cannot_write_source_or_escape_to_unowned_system_ram() {
        let _context = crate::test_support::scheduler_test_context();
        let memory = memory();
        let io = model(&memory, false);
        assert!(io.translate(0x11000, true).is_err());
        assert!(io.translate(0x21000, true).is_ok());
        assert!(io.translate(0x50000, true).is_err());
        let physical = io.translate(0x50000, false).unwrap();
        assert_eq!(physical, memory.tables.physical[7]);
    }
    #[test]
    fn captured_context_capability_uses_uabi_classes_not_legacy_ring_selector() {
        assert_eq!(captured_classes(false, false), 0);
        assert_eq!(captured_classes(false, true), 1);
        assert_eq!(captured_classes(true, false), 2);
        assert_eq!(captured_classes(true, true), 3);
    }
    pub(in crate::drm::intel) fn check_user_layout(job: Arc<UserJob>) -> Result<(), Error> {
        job.validate()?;
        let array = super::super::super::gtt::mock::MockPageTable::new(65536);
        let gtt = Arc::new(Gtt::over(Box::new(array)).unwrap());
        let mut memory = Memory::allocate(gtt)?;
        memory.render = job.render;
        if job.render {
            memory.context = Arc::new(Ram::allocate(intel_gt::rcs::CONTEXT_PAGES)?);
        }
        memory.selftest = false;
        memory.batch_address = sparse::normalize(job.batch)?;
        memory.user = Some(job.clone());
        memory.bind_and_build()?;
        let io = model(&memory, false);
        if job.render {
            memory.render_ring(&io)?;
        }
        for (object, ram) in job.objects.iter().zip(&memory.resident) {
            let va = sparse::normalize(object.address)?;
            for (i, &physical) in ram.physical.iter().enumerate() {
                assert_eq!(
                    io.translate(va + i as u64 * PAGE as u64, object.writable)?,
                    physical
                );
            }
        }
        let mut words = [0; PAGE];
        memory.ring.read(0, &mut words)?;
        let slot = if job.render { 36 + 23 } else { 15 };
        assert_eq!(
            u32::from_le_bytes(words[slot * 4..slot * 4 + 4].try_into().unwrap()),
            0x18800101
        );
        assert_eq!(
            u32::from_le_bytes(words[(slot + 1) * 4..(slot + 2) * 4].try_into().unwrap()),
            memory.batch_address as u32
        );
        assert_eq!(
            u32::from_le_bytes(words[(slot + 2) * 4..(slot + 3) * 4].try_into().unwrap()),
            (memory.batch_address >> 32) as u32
        );
        drop(io);
        memory.release()
    }
    #[test]
    fn sparse_vm_walks_iris_high_regions_and_crosses_all_directory_boundaries() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let mut memory = memory();
        for va in [
            0x1ff000,
            0x3ffff000,
            0x7ffffff000,
            0x1_0000_0000,
            0x8000_0000_0000,
            0xffff_8000_0000_0000,
        ] {
            let stash = sparse::populate(
                &memory.tables,
                &[sparse::Mapping {
                    address: va,
                    pages: &memory.destination.physical,
                    writable: true,
                    pat: 3,
                }],
                None,
            )
            .unwrap();
            stash.publish(&memory.tables).unwrap();
            memory.extra_tables = Arc::new(stash.tables);
            let io = model(&memory, false);
            for (i, &physical) in memory.destination.physical.iter().enumerate() {
                assert_eq!(
                    io.translate(va + i as u64 * PAGE as u64, true).unwrap(),
                    physical
                );
            }
            assert!(io.translate(0x10000, true).is_err());
            assert_eq!(
                io.translate(0x10000, false).unwrap(),
                memory.tables.physical[7]
            );
        }
        memory.release().unwrap();
    }
    #[test]
    fn sparse_vm_rejects_overlap_overflow_and_noncanonical_before_root_publication() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let mut memory = memory();
        let mut before = [0; PAGE];
        memory.tables.read(0, &mut before).unwrap();
        for va in [
            0,
            1,
            0x10000,
            0x1_0000_0000_0000,
            0x0001_8000_0000_0000,
            0xffff_ffff_ffff_f000,
        ] {
            assert!(
                sparse::populate(
                    &memory.tables,
                    &[
                        sparse::Mapping {
                            address: 0x10000,
                            pages: &memory.source.physical,
                            writable: false,
                            pat: 3
                        },
                        sparse::Mapping {
                            address: va,
                            pages: &memory.destination.physical,
                            writable: true,
                            pat: 3
                        },
                    ],
                    None
                )
                .is_err()
            );
            let mut after = [0; PAGE];
            memory.tables.read(0, &mut after).unwrap();
            assert_eq!(before, after);
        }
        memory.release().unwrap();
    }
    fn build_switch(m: &mut Memory) {
        let dummy = memory();
        let io = model(&dummy, false);
        m.switch
            .as_mut()
            .unwrap()
            .build(&m.gtt, m.render, m.tables.physical[0], &io)
            .unwrap();
    }
    #[test]
    fn context_switch_then_reset_retains_opaque_rcs_image_and_restores_next_job() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = super::super::super::gem_exec::tests::file();
        let saved = SavedContext::new(&file, true).unwrap();
        let mut memory = memory();
        memory.release().unwrap();
        memory.render = true;
        memory.context = saved.ram.clone();
        memory.saved = Some(saved.clone());
        memory.switch = Some(Box::new(SwitchAway::new(true).unwrap()));
        memory.bind_and_build().unwrap();
        build_switch(&mut memory);
        let io = model(&memory, true);
        memory.render_ring(&io).unwrap();
        assert!(!saved.valid.load(Ordering::Acquire));
        execute_and_quiesce(&io, &memory).unwrap().unwrap();
        assert!(saved.valid.load(Ordering::Acquire));
        let mut opaque = [0; 4096];
        saved.ram.read(2 * PAGE, &mut opaque).unwrap();
        assert!(opaque.iter().all(|b| *b == 0x6a));
        drop(io);
        memory.release().unwrap();
        memory.bind_and_build().unwrap();
        saved.ram.read(2 * PAGE, &mut opaque).unwrap();
        assert!(opaque.iter().all(|b| *b == 0x6a));
        let mut ctrl = [0; 4];
        saved.ram.read(PAGE + 3 * 4, &mut ctrl).unwrap();
        assert_eq!(u32::from_le_bytes(ctrl), 0x90008);
        saved.ram.read(PAGE + 35 * 4, &mut ctrl).unwrap();
        assert_eq!(u32::from_le_bytes(ctrl), 0x5544);
        build_switch(&mut memory);
        let io = model(&memory, true);
        memory.render_ring(&io).unwrap();
        execute_and_quiesce(&io, &memory).unwrap().unwrap();
        drop(io);
        memory.release().unwrap();
    }
    #[test]
    fn switch_completion_and_safe_reset_are_both_required_before_image_publication() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = super::super::super::gem_exec::tests::file();
        let saved = SavedContext::new(&file, false).unwrap();
        let mut m = memory();
        m.release().unwrap();
        m.context = saved.ram.clone();
        m.saved = Some(saved.clone());
        m.switch = Some(Box::new(SwitchAway::new(false).unwrap()));
        m.bind_and_build().unwrap();
        build_switch(&mut m);
        let io = model(&m, false);
        assert!(execute_and_quiesce(&io, &m).unwrap().is_err());
        assert!(!saved.valid.load(Ordering::Acquire));
        drop(io);
        m.release().unwrap();
        m.bind_and_build().unwrap();
        build_switch(&mut m);
        let io = model(&m, true);
        execute_and_quiesce(&io, &m).unwrap().unwrap();
        assert!(saved.valid.load(Ordering::Acquire));
        let count = io.log.borrow().len();
        io.fail_write.set(Some(count + 1));
        assert!(execute_and_quiesce(&io, &m).unwrap().is_err());
        assert!(!saved.valid.load(Ordering::Acquire));
        drop(io);
        m.release().unwrap();
    }
    #[test]
    fn reset_default_capture_uses_two_idle_contexts_without_copy_or_shader_dispatch() {
        let _scheduler = crate::test_support::scheduler_test_context();
        for render in [false, true] {
            let mut m = memory();
            m.release().unwrap();
            m.render = render;
            m.idle = true;
            m.selftest = false;
            m.context = Arc::new(Ram::allocate(if render { 16 } else { 4 }).unwrap());
            m.switch = Some(Box::new(SwitchAway::new(render).unwrap()));
            m.source.write(0, &[0x73; 4096]).unwrap();
            m.destination.write(0, &[0x25; 4096]).unwrap();
            m.bind_and_build().unwrap();
            build_switch(&mut m);
            let io = model(&m, true);
            if render {
                m.render_ring(&io).unwrap();
            }
            execute_and_quiesce(&io, &m).unwrap().unwrap();
            let mut bytes = [0; 4096];
            m.source.read(0, &mut bytes).unwrap();
            assert_eq!(bytes, [0x73; 4096]);
            m.destination.read(0, &mut bytes).unwrap();
            assert_eq!(bytes, [0x25; 4096]);
            let captured = m.context.clone();
            drop(io);
            m.release().unwrap();
            let mut marker = [0; 4];
            captured.read(lrc::SCRATCH as usize, &mut marker).unwrap();
            assert_eq!(u32::from_le_bytes(marker), 1);
            // Local model image only, never publish it as native DEFAULTS.
        }
    }
    #[test]
    fn two_context_submit_fault_prefixes_invalidate_state_and_reset_before_unbinding() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let file = super::super::super::gem_exec::tests::file();
        let mut base = memory();
        base.release().unwrap();
        base.saved = Some(SavedContext::new(&file, false).unwrap());
        base.context = base.saved.as_ref().unwrap().ram.clone();
        base.switch = Some(Box::new(SwitchAway::new(false).unwrap()));
        base.bind_and_build().unwrap();
        build_switch(&mut base);
        let io = model(&base, true);
        submit(&io, &base).unwrap();
        let prefixes = io.log.borrow().len();
        drop(io);
        base.release().unwrap();
        for prefix in 1..=prefixes {
            let mut m = memory();
            m.release().unwrap();
            let saved = SavedContext::new(&file, false).unwrap();
            m.context = saved.ram.clone();
            m.saved = Some(saved.clone());
            m.switch = Some(Box::new(SwitchAway::new(false).unwrap()));
            m.bind_and_build().unwrap();
            build_switch(&mut m);
            let io = model(&m, true);
            io.fail_write.set(Some(prefix));
            assert!(execute_and_quiesce(&io, &m).unwrap().is_err());
            assert!(!saved.valid.load(Ordering::Acquire));
            assert_eq!(m.bindings.len(), 3);
            assert_eq!(m.switch.as_ref().unwrap().bindings.len(), 2);
            assert_eq!(
                io.log
                    .borrow()
                    .iter()
                    .filter(|(r, v)| *r == 0x941c && *v == 4)
                    .count(),
                2
            );
            drop(io);
            m.release().unwrap();
        }
    }
}
