// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! The GPU's one MSI, and the interrupt path into the translated i915.
//!
//! The i915 port owns the GPU's interrupts. Its top-level handler is reached
//! through the single MSI this module reserves for the GPU function, and i915's
//! `request_irq()` publishes that handler here.
//!
//! There is no display in this build, so no display interrupt source is ever
//! enabled. Linux `intel_display_irq_reset()` and `intel_display_irq_postinstall()`
//! return early when `!HAS_DISPLAY`. The firmware may still have left display
//! sources armed, though, and such a source would hold the shared master bit
//! asserted. [`upstream::install`] therefore quiesces them before the MSI is
//! unmasked: the display master is switched off and every `DE_TOP` source is
//! masked, cleared and disabled. That is the write set of Linux
//! `gen11_display_irq_reset()`, applied here because the firmware may not have
//! left the display in the state a Linux boot would find it in.

use alloc::{format, string::String};
use core::sync::atomic::{Ordering, compiler_fence};

use spin::Mutex;

use super::{pci, regs::RegisterWindow};

/// GT-side master interrupt control (`GEN11_GFX_MSTR_IRQ`).
const GFX_MSTR_IRQ: u32 = 0x19_0010;
/// Display master: while clear, no display source reaches the GT.
const GEN11_DISPLAY_INT_CTL: u32 = 0x4_4200;
const DE_TOP_IMR: u32 = 0x4_4004;
const DE_TOP_IIR: u32 = 0x4_4008;
const DE_TOP_IER: u32 = 0x4_400c;
/// Graphics misc unit (`GEN11_GU_MISC_IIR`), which is a GT-path source.
const GU_MISC_IIR: u32 = 0x4_44f8;

/// The GPU's PCI function and the MSI vector the translated driver owns.
static UPSTREAM_MSI_OWNER: Mutex<Option<(pci::Bdf, usize)>> = Mutex::new(None);

/// The MSI vector reserved for `bdf`, if the GPU function already has one.
/// PCI MSI enable consumes this vector instead of programming a new one.
pub(super) fn msi_vector_for(bdf: pci::Bdf) -> Option<usize> {
    let owner = *UPSTREAM_MSI_OWNER.lock();
    owner.and_then(|(owner, vector)| (owner == bdf).then_some(vector))
}

fn raw_read(base: usize, offset: u32) -> Result<u32, String> {
    if !offset.is_multiple_of(4) || offset as usize + 4 > super::regs::PROBE_WINDOW {
        return Err(format!("GPU interrupt register out of range: {offset:#x}"));
    }
    compiler_fence(Ordering::SeqCst);
    // SAFETY: the register is inside the GPU's mapped BAR0 window, checked above.
    let value = unsafe { ((base + offset as usize) as *const u32).read_volatile() };
    compiler_fence(Ordering::SeqCst);
    Ok(value)
}

/// Write one interrupt register and check that it reads back as written.
fn checked_write(base: usize, offset: u32, value: u32) -> Result<(), String> {
    raw_write(base, offset, value)?;
    (raw_read(base, offset)? == value)
        .then_some(())
        .ok_or_else(|| format!("GPU interrupt register {offset:#x} readback differs"))
}

/// Write a register whose readback is not the written value (W1C status).
fn raw_write(base: usize, offset: u32, value: u32) -> Result<(), String> {
    if !offset.is_multiple_of(4) || offset as usize + 4 > super::regs::PROBE_WINDOW {
        return Err(format!("GPU interrupt register out of range: {offset:#x}"));
    }
    compiler_fence(Ordering::SeqCst);
    // SAFETY: the same bounded window as `raw_read`.
    unsafe { ((base + offset as usize) as *mut u32).write_volatile(value) };
    compiler_fence(Ordering::SeqCst);
    Ok(())
}

/// The GPU register window's base, which is where the GT master is written.
fn gpu_base() -> Option<usize> {
    super::upstream_gt::gpu_window().map(RegisterWindow::base)
}

pub(super) mod upstream {
    use core::ffi::{c_int, c_void};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use intel_gt::i915_irq_upstream::{DisplayIrqOps, install_display_irq_ops};
    use tk_linux_kapi::kernel_core::{IrqCoreOps, install_irq_core_ops};
    use intel_gt::linux_i915_private::DrmI915Private;

    use super::{
        DE_TOP_IER, DE_TOP_IIR, DE_TOP_IMR, GEN11_DISPLAY_INT_CTL, GFX_MSTR_IRQ,
        GU_MISC_IIR, UPSTREAM_MSI_OWNER, checked_write, gpu_base, raw_read, raw_write,
    };
    use crate::drm::intel::pci::{self, DisplayMsi};

    type Handler = unsafe extern "C" fn(c_int, *mut c_void) -> c_int;

    /// `dev_id` is published before the handler, so a reader that sees the
    /// handler also sees its argument.
    static HANDLER: AtomicUsize = AtomicUsize::new(0);
    static DEV_ID: AtomicUsize = AtomicUsize::new(0);
    static IRQ_NUMBER: AtomicUsize = AtomicUsize::new(0);
    static MSI_BEFORE: spin::Mutex<Option<pci::DisplayMsiBefore>> = spin::Mutex::new(None);
    static BDF: spin::Mutex<Option<pci::Bdf>> = spin::Mutex::new(None);

    const IRQ_HANDLED: c_int = 1;
    const ENODEV: c_int = 19;
    const EBUSY: c_int = 16;
    const GEN11_GU_MISC_GSE: u32 = 1 << 27;

    /// Whether the ADL-P display interrupt master is enabled and the i915 IRQ
    /// path currently owns a live handler. The display parent uses this exact
    /// condition before synchronizing its shared GPU vector.
    pub(in crate::drm::intel) unsafe fn display_irq_enabled(
        drm: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    ) -> bool {
        if drm.is_null() || HANDLER.load(Ordering::Acquire) == 0 {
            return false;
        }
        let i915 = unsafe { (*drm).dev_private.cast::<DrmI915Private>() };
        if i915.is_null() || core::ptr::addr_of_mut!((*i915).drm) != drm {
            return false;
        }
        let display = unsafe { (*i915).display.cast::<intel_gt::intel_display_core_types_upstream::IntelDisplay>() };
        if display.is_null()
            || !unsafe { intel_gt::intel_display_device_upstream::intel_display_device_present(display) }
        {
            return false;
        }
        let Some(base) = gpu_base() else { return false };
        raw_read(base, GEN11_DISPLAY_INT_CTL).is_ok_and(|value| {
            value & intel_gt::intel_display_regs_upstream::GEN11_DISPLAY_IRQ_ENABLE != 0
        })
    }

    pub(in crate::drm::intel) unsafe fn display_irq_synchronize(
        drm: *mut tk_drm_upstream::drm_device_types::DrmDevice,
    ) {
        if drm.is_null() {
            return;
        }
        let i915 = unsafe { (*drm).dev_private.cast::<DrmI915Private>() };
        if i915.is_null() || core::ptr::addr_of_mut!((*i915).drm) != drm {
            return;
        }
        let irq = IRQ_NUMBER.load(Ordering::Acquire);
        if irq != 0 {
            axhal::irq::synchronize_hardirq(irq);
        }
    }

    /// The hard-IRQ entry the MSI calls: forward to the translated handler.
    fn gpu_msi_handler() {
        forward();
    }

    /// Run the handler i915 registered, if any.
    pub(in crate::drm::intel) fn forward() -> bool {
        let handler = HANDLER.load(Ordering::Acquire);
        if handler == 0 {
            return false;
        }
        // SAFETY: HANDLER only ever holds a `Handler` stored by `request_irq`.
        let handler: Handler = unsafe { core::mem::transmute::<usize, Handler>(handler) };
        let dev_id = DEV_ID.load(Ordering::Acquire) as *mut c_void;
        let irq = IRQ_NUMBER.load(Ordering::Acquire) as c_int;
        // SAFETY: `request_irq` publishes the handler and `dev_id` as a pair;
        // `free_irq` masks the MSI and synchronizes hard IRQs before it retires them.
        let _ = unsafe { handler(irq, dev_id) } == IRQ_HANDLED;
        true
    }

    /// Quiesce the firmware's display sources and the display master; see the
    /// module comment. Runs before the MSI is unmasked.
    fn quiesce_display_interrupts(base: usize) -> Result<(), &'static str> {
        checked_write(base, GEN11_DISPLAY_INT_CTL, 0).map_err(|_| "display master did not clear")?;
        raw_write(base, DE_TOP_IMR, u32::MAX).map_err(|_| "DE_TOP_IMR write refused")?;
        raw_write(base, DE_TOP_IER, 0).map_err(|_| "DE_TOP_IER write refused")?;
        raw_write(base, DE_TOP_IIR, u32::MAX).map_err(|_| "DE_TOP_IIR clear refused")?;
        Ok(())
    }

    /// The probe calls this after identity-DMA registration and before
    /// `i915_pci_probe()`. Reserve/reuse exactly one MSI vector now so both
    /// Linux's early `pci_enable_msi()` and later `request_irq()` observe the
    /// same owner and vector.
    pub(in crate::drm::intel) fn install(bdf: pci::Bdf) -> Result<(), &'static str> {
        let base = gpu_base().ok_or("GPU register window is not registered")?;
        quiesce_display_interrupts(base)?;
        *BDF.lock() = Some(bdf);
        let vector = match super::msi_vector_for(bdf) {
            Some(vector) => vector,
            None => reserve_upstream_msi(bdf)?,
        };
        let device = super::super::upstream_gt::native_pci_device(bdf);
        if tk_linux_kapi::dma::set_device_irq(device, vector).is_err() {
            disable_msi(bdf);
            return Err("DMA device did not accept the reserved MSI vector");
        }
        if install_irq_core_ops(&IRQ_CORE_OPS).is_err() {
            disable_msi(bdf);
            return Err("LinuxKPI IRQ owner is already installed");
        }
        if install_display_irq_ops(&DISPLAY_IRQ_OPS).is_err() {
            disable_msi(bdf);
            return Err("upstream display IRQ callbacks are already installed");
        }
        Ok(())
    }

    /// Reserve and program the GPU's MSI, retaining the exact PCI before-image
    /// until the driver's matching `pci_disable_msi()` teardown.
    fn reserve_upstream_msi(bdf: pci::Bdf) -> Result<usize, &'static str> {
        let requester = tk_vtd::PciRequester {
            segment: axhal::pci::ecam_segment(),
            bus: bdf.bus,
            device: bdf.device,
            function: bdf.function,
        };
        let Some((address, data, vector)) = axhal::irq::allocate_msi(requester, gpu_msi_handler)
        else {
            return Err("upstream GT MSI vector could not be reserved");
        };
        let mut ecam = match pci::Ecam::platform() {
            Some(ecam) => ecam,
            None => {
                let _ = axhal::irq::unregister(vector);
                return Err("upstream GT MSI needs PCI ECAM");
            }
        };
        let before = match ecam.prepare_display_msi(bdf, address, data) {
            Ok(before) => before,
            Err(error) => {
                if !error.rollback_verified {
                    *UPSTREAM_MSI_OWNER.lock() = Some((bdf, vector));
                } else {
                    let _ = axhal::irq::unregister(vector);
                }
                axlog::warn!("intel-gt: upstream MSI setup refused: {}", error.message);
                return Err("upstream GT MSI setup refused");
            }
        };
        if let Err(error) = ecam
            .activate_display_msi(&before)
            .and_then(|()| ecam.unmask_display_msi(&before))
        {
            if let Err(rollback) = ecam.restore_display_msi(&before) {
                *MSI_BEFORE.lock() = Some(before);
                *UPSTREAM_MSI_OWNER.lock() = Some((bdf, vector));
                axlog::warn!("intel-gt: upstream MSI rollback unverified: {error}; {rollback}");
                return Err("upstream GT MSI rollback was not verified");
            }
            let _ = axhal::irq::unregister(vector);
            axlog::warn!("intel-gt: upstream MSI setup failed: {error}; PCI state restored");
            return Err("upstream GT MSI could not be activated");
        }
        *MSI_BEFORE.lock() = Some(before);
        *UPSTREAM_MSI_OWNER.lock() = Some((bdf, vector));
        Ok(vector)
    }

    /// Retire the GPU's MSI. Refused while a `request_irq()` handler is live,
    /// and whenever the PCI before-image is missing (see `reserve_upstream_msi`).
    pub(in crate::drm::intel) fn disable_msi(bdf: pci::Bdf) {
        let upstream_owner = *UPSTREAM_MSI_OWNER.lock();
        if upstream_owner.is_none_or(|(owner, _)| owner != bdf) {
            return;
        }
        if HANDLER.load(Ordering::Acquire) != 0 {
            axlog::warn!("intel-gt: retaining MSI owner while request_irq handler remains live");
            return;
        }
        let Some(before) = *MSI_BEFORE.lock() else {
            // An unverified partial prepare has no complete before-image and
            // intentionally keeps its callback/vector quarantined.
            axlog::warn!("intel-gt: retaining MSI vector without a verified before-image");
            return;
        };
        let Some(mut ecam) = pci::Ecam::platform() else {
            axlog::warn!("intel-gt: retaining MSI vector; PCI ECAM unavailable at teardown");
            return;
        };
        if let Err(error) = ecam.restore_display_msi(&before) {
            axlog::warn!("intel-gt: retaining MSI vector; PCI restore failed: {error}");
            return;
        }
        if let Some((owner_bdf, vector)) = UPSTREAM_MSI_OWNER.lock().take() {
            if owner_bdf == bdf {
                let _ = axhal::irq::unregister(vector);
            } else {
                *UPSTREAM_MSI_OWNER.lock() = Some((owner_bdf, vector));
            }
        }
        *MSI_BEFORE.lock() = None;
    }

    static IRQ_CORE_OPS: IrqCoreOps = IrqCoreOps { request_irq, free_irq };

    /// `request_irq()` for the GPU function. MSI was routed by `install()` and
    /// this callback only publishes the translated handler on that same vector.
    unsafe extern "C" fn request_irq(irq: usize, handler: Handler, dev_id: *mut c_void) -> c_int {
        if HANDLER.load(Ordering::Acquire) != 0 {
            return -EBUSY;
        }
        let Some(bdf) = *BDF.lock() else { return -ENODEV };
        if super::msi_vector_for(bdf) != Some(irq) {
            return -ENODEV;
        }
        IRQ_NUMBER.store(irq, Ordering::Release);
        DEV_ID.store(dev_id as usize, Ordering::Release);
        HANDLER.store(handler as usize, Ordering::Release);
        0
    }

    /// `free_irq()`: mask the MSI route, retire the handler, then mask the GT
    /// master so no hard-IRQ path re-enables it.
    unsafe extern "C" fn free_irq(irq: usize, dev_id: *mut c_void) {
        if DEV_ID.load(Ordering::Acquire) != dev_id as usize || IRQ_NUMBER.load(Ordering::Acquire) != irq {
            return;
        }
        // The source reset precedes this callback, but a hard handler already
        // executing may re-enable the root on exit. Mask the MSI route first.
        let before = *MSI_BEFORE.lock();
        let masked = match (pci::Ecam::platform(), before.as_ref()) {
            (Some(mut ecam), Some(before)) => ecam.mask_display_msi(before).is_ok(),
            _ => false,
        };
        let base = gpu_base();
        if !masked {
            axlog::warn!("intel-gt: MSI mask unverified; masking the GT master");
            if let Some(base) = base {
                let _ = checked_write(base, GFX_MSTR_IRQ, 0);
            }
        }
        HANDLER.store(0, Ordering::Release);
        // Match Linux free_irq(): after this returns no hard-IRQ invocation
        // may still hold the translated handler or its dev_id.
        axhal::irq::synchronize_hardirq(irq);
        DEV_ID.store(0, Ordering::Release);
        IRQ_NUMBER.store(0, Ordering::Release);
        if let Some(base) = base {
            let _ = checked_write(base, GFX_MSTR_IRQ, 0);
        }
    }

    static DISPLAY_IRQ_OPS: DisplayIrqOps = DisplayIrqOps {
        display_irq_handler,
        gu_misc_irq_ack,
        gu_misc_irq_handler,
        display_irq_reset,
        display_irq_postinstall,
        pmu_irq_stats,
    };

    /// Delegate display-source acknowledge and dispatch to the typed upstream
    /// ADL-P implementation. Gen11 uses its own master-control snapshot and
    /// does not consume the legacy state argument.
    unsafe extern "C" fn display_irq_handler(display: *mut c_void) {
        let _ = unsafe {
            intel_gt::intel_display_irq_upstream::intel_display_irq_handler(
                display.cast(),
                core::ptr::null(),
            )
        };
    }

    /// `gen11_gu_misc_irq_ack()`: GU_MISC is a GT-path source, so it is acked here.
    unsafe extern "C" fn gu_misc_irq_ack(_display: *mut c_void, master_ctl: u32) -> u32 {
        const GEN11_GU_MISC_IRQ: u32 = 1 << 29;
        let Some(base) = gpu_base() else { return 0 };
        if master_ctl & GEN11_GU_MISC_IRQ == 0 {
            return 0;
        }
        let iir = raw_read(base, GU_MISC_IIR).unwrap_or(0);
        if iir != 0 {
            let _ = raw_write(base, GU_MISC_IIR, iir);
        }
        iir
    }

    /// `gen11_gu_misc_irq_handler()`: GSE is the OpRegion ASLE request, which
    /// TheKernel does not service.
    unsafe extern "C" fn gu_misc_irq_handler(_display: *mut c_void, iir: u32) {
        if iir & GEN11_GU_MISC_GSE != 0 {
            axlog::warn!("intel-gpu: OpRegion ASLE interrupt ignored");
        }
    }

    unsafe extern "C" fn display_irq_reset(display: *mut c_void) {
        unsafe { intel_gt::intel_display_irq_upstream::intel_display_irq_reset(display.cast()) };
    }
    unsafe extern "C" fn display_irq_postinstall(display: *mut c_void) {
        unsafe { intel_gt::intel_display_irq_upstream::intel_display_irq_postinstall(display.cast()) };
    }

    /// `pmu_irq_stats()` only feeds i915 PMU counters; CONFIG_PERF_EVENTS
    /// for i915 is off.
    unsafe extern "C" fn pmu_irq_stats(_i915: *mut DrmI915Private) {}
}
