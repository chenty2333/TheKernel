// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Narrow N305 display MSI owner for Pipe-A vblank and TC1/TC2 hotplug.
//!
//! MSI is only attempted from the already opt-in native modeset path. The
//! owner fails closed unless PCI MSI is disabled, every known Gen12 display
//! source is masked/idle, the GT class interrupt enables are zero, the GT
//! owner is not busy or quarantined, and the shared Gen11 master is disabled.
//! The hard-IRQ path only reads/acks the owned fixed MMIO sources and publishes
//! atomic vblank/HPD bits; GMBUS, allocation, logging and modesetting remain in
//! task context.
//!
//! Source/license boundary: offsets, bit definitions and display-source
//! semantics follow the public Display 12/13 PRM and the MIT-licensed
//! `intel_display_irq.c` (`gen11_display_irq_handler`, `gen8_de_irq_handler`,
//! `gen8_read_and_ack_pch_irqs`) plus `intel_hotplug_irq.c`
//! (`icp_irq_handler`, `icp_hpd_irq_setup`, `icp_tc_hpd_detection_setup`).
//! The checked GT-class enable offsets are from the MIT-licensed
//! `intel_gmd_interrupt_regs.h` and `gt/intel_gt_regs.h`; `intel_gt_irq.c` is
//! consulted only for which enable registers are sources, not for copied
//! handler code. The top-level `i915_irq.c` dispatcher is GPL and is not
//! ported: the N305 adapter owns one MSI, admits only the display master source,
//! and fails closed rather than servicing unrelated GT/PCU sources.

use alloc::{format, string::String, vec::Vec};
use core::{
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering, compiler_fence},
    time::Duration,
};

use axtask::WaitQueue;
use intel_display::dkl_phy::TcPort;
#[cfg(target_os = "none")]
use spin::Mutex;

#[cfg(target_os = "none")]
use super::pci::N305DisplayMsi;
#[cfg(target_os = "none")]
use super::{pci, regs::RegisterWindow};

const GFX_MSTR_IRQ: u32 = 0x19_0010;
const GEN11_MASTER_IRQ: u32 = 1 << 31;
const GEN11_DISPLAY_IRQ: u32 = 1 << 16;
const GEN11_DISPLAY_INT_CTL: u32 = 0x4_4200;
const DISPLAY_IRQ_ENABLE: u32 = 1 << 31;
const DISPLAY_PIPE_A: u32 = 1 << 16;
const DISPLAY_DE_HPD: u32 = 1 << 21;
const DISPLAY_PCH: u32 = 1 << 23;
const PIPE_A_IMR: u32 = 0x4_4404;
const PIPE_A_IIR: u32 = 0x4_4408;
const PIPE_A_IER: u32 = 0x4_440c;
const PIPE_VBLANK: u32 = 1;
const DE_HPD_IMR: u32 = 0x4_4474;
const DE_HPD_IIR: u32 = 0x4_4478;
const DE_HPD_IER: u32 = 0x4_447c;
const SDE_IMR: u32 = 0xc_4004;
const SDE_IIR: u32 = 0xc_4008;
const SDE_IER: u32 = 0xc_400c;
const SHOTPLUG_CTL_TC: u32 = 0xc_4034;
const SHPD_FILTER_CNT: u32 = 0xc_4038;
const GEN11_TC_HOTPLUG_CTL: u32 = 0x4_4038;
const HPD_FILTER_250_US: u32 = 0x000f8;
const GT_CLASS_ENABLES: [u32; 7] = [
    0x19_0030, // GEN11_RENDER_COPY_INTR_ENABLE
    0x19_0034, // GEN11_VCS_VECS_INTR_ENABLE
    0x19_0038, // GEN11_GUC_SG_INTR_ENABLE
    0x19_003c, // GEN11_GPM_WGBOXPERF_INTR_ENABLE
    0x19_0040, // GEN11_CRYPTO_RSVD_INTR_ENABLE
    0x19_0044, // GEN11_GUNIT_CSME_INTR_ENABLE
    0x19_0048, // GEN12_CCS_RSVD_INTR_ENABLE
];
const DE_TOP_IMR: u32 = 0x4_4004;
const DE_TOP_IIR: u32 = 0x4_4008;
const DE_TOP_IER: u32 = 0x4_400c;
const PORT_IMR: u32 = 0x4_4444;
const PORT_IIR: u32 = 0x4_4448;
const PORT_IER: u32 = 0x4_444c;
const MISC_IMR: u32 = 0x4_4464;
const MISC_IIR: u32 = 0x4_4468;
const MISC_IER: u32 = 0x4_446c;
const PCU_IMR: u32 = 0x4_44e4;
const PCU_IIR: u32 = 0x4_44e8;
const PCU_IER: u32 = 0x4_44ec;
const GU_MISC_IMR: u32 = 0x4_44f4;
const GU_MISC_IIR: u32 = 0x4_44f8;
const GU_MISC_IER: u32 = 0x4_44fc;

const SOURCE_BLOCKS: [(u32, u32, u32); 11] = [
    (DE_TOP_IMR, DE_TOP_IER, DE_TOP_IIR),
    (0x4_4404, 0x4_440c, 0x4_4408), // pipe A
    (0x4_4414, 0x4_441c, 0x4_4418), // pipe B
    (0x4_4424, 0x4_442c, 0x4_4428), // pipe C
    (0x4_4434, 0x4_443c, 0x4_4438), // pipe D
    (PORT_IMR, PORT_IER, PORT_IIR),
    (MISC_IMR, MISC_IER, MISC_IIR),
    (DE_HPD_IMR, DE_HPD_IER, DE_HPD_IIR),
    (SDE_IMR, SDE_IER, SDE_IIR),
    (PCU_IMR, PCU_IER, PCU_IIR),
    (GU_MISC_IMR, GU_MISC_IER, GU_MISC_IIR),
];

const OWNED_CONTROL_REGS: [u32; 10] = [
    GEN11_DISPLAY_INT_CTL,
    PIPE_A_IMR,
    PIPE_A_IER,
    DE_HPD_IMR,
    DE_HPD_IER,
    SDE_IMR,
    SDE_IER,
    GEN11_TC_HOTPLUG_CTL,
    SHOTPLUG_CTL_TC,
    SHPD_FILTER_CNT,
];

struct Events {
    base: AtomicUsize,
    online: AtomicBool,
    faulted: AtomicBool,
    vblank: AtomicU32,
    hpd: AtomicU32,
    hpd_de_mask: AtomicU32,
    hpd_sde_mask: AtomicU32,
    wake_sequence: AtomicU32,
}

impl Events {
    const fn new() -> Self {
        Self {
            base: AtomicUsize::new(0),
            online: AtomicBool::new(false),
            faulted: AtomicBool::new(false),
            vblank: AtomicU32::new(0),
            hpd: AtomicU32::new(0),
            hpd_de_mask: AtomicU32::new(0),
            hpd_sde_mask: AtomicU32::new(0),
            wake_sequence: AtomicU32::new(0),
        }
    }
}

static EVENTS: Events = Events::new();
static INSTALL_ATTEMPTED: AtomicBool = AtomicBool::new(false);
static IRQ_WAITERS: WaitQueue = WaitQueue::new();
#[cfg(target_os = "none")]
static IRQ_WINDOW: Mutex<Option<RegisterWindow>> = Mutex::new(None);

/// Whether the dedicated MSI owner is live and has not faulted.
pub(super) fn online() -> bool {
    EVENTS.online.load(Ordering::Acquire) && !EVENTS.faulted.load(Ordering::Acquire)
}

/// Whether a task-context owner should log and use the timer/poll fallback.
pub(super) fn faulted() -> bool {
    EVENTS.faulted.load(Ordering::Acquire)
}

/// Monotonic interrupt/fault generation used to close wait/IRQ races.
pub(super) fn event_sequence() -> u32 {
    EVENTS.wake_sequence.load(Ordering::Acquire)
}

/// Wait for any owned display event or the bounded timeout. The condition is
/// generation-based, so an interrupt between snapshot and listener setup is
/// observed rather than lost. A fault also wakes the timer fallback owner.
pub(super) fn wait_for_event(observed: u32, delay: Duration) -> Result<bool, String> {
    IRQ_WAITERS
        .wait_timeout_until(delay, || event_sequence() != observed || !online())
        .map(|timed_out| !timed_out)
        .map_err(|error| format!("display IRQ wait failed: {error:?}"))
}

fn notify_task_context() {
    EVENTS.wake_sequence.fetch_add(1, Ordering::Release);
    // WaitQueue's notify path is IRQ-safe with rescheduling disabled; the
    // actual DRM/EDID work remains in the awakened task.
    let _ = IRQ_WAITERS.notify_one(false);
}

/// Current interrupt-counted Pipe-A vblank sequence, meaningful only online.
pub(super) fn vblank_sequence() -> Option<u32> {
    online().then(|| EVENTS.vblank.load(Ordering::Acquire))
}

/// Consume the task-context HPD notification. A periodic EDID poll remains
/// armed independently, so an IRQ failure never masks a cable transition.
pub(super) fn take_hpd_pending() -> bool {
    EVENTS.hpd.swap(0, Ordering::AcqRel) != 0
}

fn port_hpd_masks(port: TcPort) -> Result<(u32, u32, u32), String> {
    let pin = match port {
        TcPort::Tc1 => 0,
        TcPort::Tc2 => 1,
        _ => return Err(String::from("display IRQ only admits TC1/TC2")),
    };
    Ok((1 << (16 + pin), 1 << (24 + pin), 8 << (pin * 4)))
}

fn raw_read(base: usize, offset: u32) -> Result<u32, String> {
    if !offset.is_multiple_of(4) || offset as usize + 4 > super::regs::PROBE_WINDOW {
        return Err(format!("display IRQ register out of range: {offset:#x}"));
    }
    compiler_fence(Ordering::SeqCst);
    // SAFETY: `RegisterWindow` is a stable live mapping of the complete
    // `PROBE_WINDOW`; every caller uses this aligned, in-range table offset.
    let value = unsafe { ((base + offset as usize) as *const u32).read_volatile() };
    compiler_fence(Ordering::SeqCst);
    Ok(value)
}

fn checked_read(base: usize, offset: u32) -> Result<u32, String> {
    let value = raw_read(base, offset)?;
    (value != u32::MAX)
        .then_some(value)
        .ok_or_else(|| format!("display IRQ register {offset:#x} is unreadable"))
}

fn checked_write(base: usize, offset: u32, value: u32) -> Result<(), String> {
    if !offset.is_multiple_of(4) || offset as usize + 4 > super::regs::PROBE_WINDOW {
        return Err(format!("display IRQ register out of range: {offset:#x}"));
    }
    compiler_fence(Ordering::SeqCst);
    // SAFETY: the target transaction writes only the exact, audited N305
    // display/interrupt registers and only after their owner checks passed.
    unsafe { ((base + offset as usize) as *mut u32).write_volatile(value) };
    compiler_fence(Ordering::SeqCst);
    (raw_read(base, offset)? == value)
        .then_some(())
        .ok_or_else(|| format!("display IRQ register {offset:#x} readback differs"))
}

fn raw_write(base: usize, offset: u32, value: u32) -> Result<(), String> {
    if !offset.is_multiple_of(4) || offset as usize + 4 > super::regs::PROBE_WINDOW {
        return Err(format!("display IRQ register out of range: {offset:#x}"));
    }
    compiler_fence(Ordering::SeqCst);
    // SAFETY: the same bounded register table as `checked_write`; the caller
    // uses this for W1C and status/control registers whose readback is not the
    // written value.
    unsafe { ((base + offset as usize) as *mut u32).write_volatile(value) };
    compiler_fence(Ordering::SeqCst);
    Ok(())
}

fn clear_w1c(base: usize, offset: u32, bits: u32) -> Result<(), String> {
    raw_write(base, offset, bits)?;
    if checked_read(base, offset)? & bits != 0 {
        return Err(format!("display IRQ W1C source {offset:#x} did not clear"));
    }
    Ok(())
}

fn write_display_gate(base: usize, enabled: bool) -> Result<(), String> {
    raw_write(
        base,
        GEN11_DISPLAY_INT_CTL,
        if enabled { DISPLAY_IRQ_ENABLE } else { 0 },
    )?;
    let value = checked_read(base, GEN11_DISPLAY_INT_CTL)?;
    let actual = value & DISPLAY_IRQ_ENABLE != 0;
    (actual == enabled)
        .then_some(())
        .ok_or_else(|| String::from("display interrupt gate readback differs"))
}

fn enable_master(base: usize, write_attempted: &mut bool) -> Result<(), String> {
    // The GFX master write is the last fallible source-tree step. Once it is
    // attempted, MSI delivery may already be posted even if status readback
    // and the following source rollback look clean; the caller must retain the
    // static callback, vector, and mapping in that case.
    *write_attempted = true;
    raw_write(base, GFX_MSTR_IRQ, GEN11_MASTER_IRQ)?;
    let value = checked_read(base, GFX_MSTR_IRQ)?;
    if value & GEN11_MASTER_IRQ == 0 {
        return Err(format!(
            "shared graphics master failed to enable (readback {value:#010x})"
        ));
    }
    Ok(())
}

fn may_release_failed_owner(rollback_verified: bool, master_write_attempted: bool) -> bool {
    rollback_verified && !master_write_attempted
}

fn disable_master(base: usize) -> Result<(), String> {
    raw_write(base, GFX_MSTR_IRQ, 0)?;
    if checked_read(base, GFX_MSTR_IRQ)? & GEN11_MASTER_IRQ != 0 {
        return Err(String::from("shared graphics master did not disable"));
    }
    Ok(())
}

fn source_image_idle(mut read: impl FnMut(u32) -> Option<u32>) -> bool {
    if read(GFX_MSTR_IRQ) != Some(0) || read(GEN11_DISPLAY_INT_CTL) != Some(0) {
        return false;
    }
    for offset in GT_CLASS_ENABLES {
        if read(offset) != Some(0) {
            return false;
        }
    }
    for (imr, ier, iir) in SOURCE_BLOCKS {
        if read(imr) != Some(u32::MAX) || read(ier) != Some(0) || read(iir) != Some(0) {
            return false;
        }
    }
    true
}

#[cfg(target_os = "none")]
pub(super) fn install_n305(
    ecam: &mut pci::Ecam,
    bdf: pci::Bdf,
    port: TcPort,
    window: RegisterWindow,
) -> Result<(), String> {
    if INSTALL_ATTEMPTED.swap(true, Ordering::AcqRel) {
        return Err(String::from("display IRQ owner already attempted"));
    }
    if !super::gt::display_irq_owner_idle() {
        return Err(String::from(
            "display IRQ owner refused: the GT owner is busy or holds uncertain DMA",
        ));
    }
    let base = window.base();
    if window.len() < super::regs::PROBE_WINDOW
        || !source_image_idle(|offset| raw_read(base, offset).ok())
        || checked_read(base, DE_TOP_IMR)? != u32::MAX
    {
        return Err(String::from(
            "display IRQ owner refused: active/unmasked firmware or shared display/GT source",
        ));
    }
    let (gen11_hpd, sde_hpd, hpd_enable) = port_hpd_masks(port)?;

    let (message_address, message_data, vector) = axhal::irq::allocate_msi(
        tk_vtd::PciRequester {
            segment: 0,
            bus: bdf.bus,
            device: bdf.device,
            function: bdf.function,
        },
        display_irq_handler,
    )
    .ok_or_else(|| String::from("display MSI vector could not be reserved"))?;
    // The callback's raw base is backed by this permanent window owner; a
    // later DRM registration/worker failure cannot invalidate an installed
    // MSI callback or leave it pointing at a dropped mapping token.
    *IRQ_WINDOW.lock() = Some(window);
    EVENTS.base.store(base, Ordering::Release);
    EVENTS.vblank.store(0, Ordering::Release);
    EVENTS.hpd.store(0, Ordering::Release);
    EVENTS.hpd_de_mask.store(gen11_hpd, Ordering::Release);
    EVENTS.hpd_sde_mask.store(sde_hpd, Ordering::Release);
    EVENTS.wake_sequence.store(0, Ordering::Release);
    EVENTS.faulted.store(false, Ordering::Release);
    EVENTS.online.store(false, Ordering::Release);

    let msi = match ecam.prepare_n305_display_msi(bdf, message_address, message_data) {
        Ok(before) => before,
        Err(error) => {
            if !error.rollback_verified {
                EVENTS.faulted.store(true, Ordering::Release);
                return Err(error.message);
            }
            EVENTS.base.store(0, Ordering::Release);
            IRQ_WINDOW.lock().take();
            let _ = axhal::irq::unregister(vector);
            return Err(error.message);
        }
    };
    let before = match OWNED_CONTROL_REGS
        .into_iter()
        .map(|offset| raw_read(base, offset).map(|value| (offset, value)))
        .collect::<Result<alloc::vec::Vec<_>, _>>()
    {
        Ok(before) => before,
        Err(error) => match ecam.restore_n305_display_msi(&msi) {
            Ok(()) => {
                EVENTS.base.store(0, Ordering::Release);
                IRQ_WINDOW.lock().take();
                let _ = axhal::irq::unregister(vector);
                return Err(error);
            }
            Err(rollback) => {
                EVENTS.faulted.store(true, Ordering::Release);
                return Err(format!(
                    "{error}; PCI MSI restoration unverified, retaining IRQ owner: {rollback}"
                ));
            }
        },
    };
    let before_value = |offset: u32| -> Result<u32, String> {
        before
            .iter()
            .find_map(|(register, value)| (*register == offset).then_some(*value))
            .ok_or_else(|| String::from("display IRQ source before-image is incomplete"))
    };
    let (old_gen11_tc, old_shotplug_tc) = match (
        before_value(GEN11_TC_HOTPLUG_CTL),
        before_value(SHOTPLUG_CTL_TC),
    ) {
        (Ok(gen11), Ok(shotplug)) => (gen11, shotplug),
        (Err(error), _) | (_, Err(error)) => match ecam.restore_n305_display_msi(&msi) {
            Ok(()) => {
                EVENTS.base.store(0, Ordering::Release);
                IRQ_WINDOW.lock().take();
                let _ = axhal::irq::unregister(vector);
                return Err(error);
            }
            Err(rollback) => {
                EVENTS.faulted.store(true, Ordering::Release);
                return Err(format!(
                    "{error}; PCI MSI restoration unverified, retaining IRQ owner: {rollback}"
                ));
            }
        },
    };

    let changed = (|| {
        checked_write(base, GEN11_TC_HOTPLUG_CTL, old_gen11_tc | hpd_enable)?;
        checked_write(base, SHOTPLUG_CTL_TC, old_shotplug_tc | hpd_enable)?;
        checked_write(base, SHPD_FILTER_CNT, HPD_FILTER_250_US)?;
        checked_write(base, PIPE_A_IER, PIPE_VBLANK)?;
        checked_write(base, PIPE_A_IMR, u32::MAX & !PIPE_VBLANK)?;
        checked_write(base, DE_HPD_IER, gen11_hpd)?;
        checked_write(base, DE_HPD_IMR, u32::MAX & !gen11_hpd)?;
        checked_write(base, SDE_IER, sde_hpd)?;
        checked_write(base, SDE_IMR, u32::MAX & !sde_hpd)?;
        write_display_gate(base, true)?;
        ecam.activate_n305_display_msi(&msi)?;
        // Release the vector mask before the root is the final routing write.
        // A failure up to this point is still rollbackable without an IRQ in
        // flight because GFX_MSTR_IRQ remains disabled.
        ecam.unmask_n305_display_msi(&msi)?;
        Ok::<(), String>(())
    })();
    if let Err(error) = changed {
        return match rollback_install(base, ecam, &msi, &before, hpd_enable) {
            Ok(()) => {
                EVENTS.base.store(0, Ordering::Release);
                IRQ_WINDOW.lock().take();
                let _ = axhal::irq::unregister(vector);
                Err(format!("{error}; display IRQ before-image restored"))
            }
            Err(failure) => {
                EVENTS.faulted.store(true, Ordering::Release);
                EVENTS.online.store(false, Ordering::Release);
                Err(format!(
                    "{error}; display IRQ rollback unverified, retaining callback/window: \
                     {failure}"
                ))
            }
        };
    }

    // A PVM-capable message stays masked until the global/display source tree
    // has been installed; the final master write may now cause an owned IRQ.
    EVENTS.online.store(true, Ordering::Release);
    let mut master_write_attempted = false;
    if let Err(error) = enable_master(base, &mut master_write_attempted) {
        EVENTS.online.store(false, Ordering::Release);
        EVENTS.faulted.store(true, Ordering::Release);
        return match rollback_install(base, ecam, &msi, &before, hpd_enable) {
            Ok(()) => {
                if may_release_failed_owner(true, master_write_attempted) {
                    EVENTS.base.store(0, Ordering::Release);
                    IRQ_WINDOW.lock().take();
                    let _ = axhal::irq::unregister(vector);
                    Err(format!("{error}; display IRQ before-image restored"))
                } else {
                    Err(format!(
                        "{error}; display IRQ sources/MSI restored, but the shared-master write \
                         may have posted an MSI; retaining callback/vector/window"
                    ))
                }
            }
            Err(failure) => Err(format!(
                "{error}; display IRQ rollback unverified, retaining callback/window: {failure}"
            )),
        };
    }
    Ok(())
}

#[cfg(target_os = "none")]
fn rollback_install(
    base: usize,
    ecam: &mut pci::Ecam,
    msi: &pci::DisplayMsiBefore,
    before: &[(u32, u32)],
    hpd_enable: u32,
) -> Result<(), String> {
    disable_master(base)?;
    write_display_gate(base, false)?;
    // An HPD/vblank that arrived during the masked setup is ours. Acknowledge
    // only these owned W1C bits; unknown status remains unmodified.
    let pipe_iir = checked_read(base, PIPE_A_IIR)? & PIPE_VBLANK;
    if pipe_iir != 0 {
        clear_w1c(base, PIPE_A_IIR, pipe_iir)?;
    }
    let hpd_iir = checked_read(base, DE_HPD_IIR)? & ((1 << 16) | (1 << 17));
    if hpd_iir != 0 {
        clear_w1c(base, DE_HPD_IIR, hpd_iir)?;
    }
    let sde_iir = checked_read(base, SDE_IIR)? & ((1 << 24) | (1 << 25));
    if sde_iir != 0 {
        clear_w1c(base, SDE_IIR, sde_iir)?;
    }
    for (offset, value) in before.iter().rev() {
        match *offset {
            GEN11_DISPLAY_INT_CTL => write_display_gate(base, *value & DISPLAY_IRQ_ENABLE != 0)?,
            GEN11_TC_HOTPLUG_CTL | SHOTPLUG_CTL_TC => {
                let current = checked_read(base, *offset)?;
                let restored = (current & !hpd_enable) | (*value & hpd_enable);
                checked_write(base, *offset, restored)?;
            }
            _ => checked_write(base, *offset, *value)?,
        }
    }
    ecam.restore_n305_display_msi(msi)
}

#[cfg(target_os = "none")]
fn display_irq_handler() {
    let base = EVENTS.base.load(Ordering::Acquire);
    if base == 0 || !EVENTS.online.load(Ordering::Acquire) {
        return;
    }
    let io = VolatileIo { base };
    let _ = dispatch(&io, &EVENTS);
}

trait IrqIo {
    fn read(&self, offset: u32) -> Option<u32>;
    fn write(&self, offset: u32, value: u32) -> bool;
}

/// Adapts the translated Gen11 hotplug pin decoder to the IRQ owner's exact
/// read-only control-register view. Its callback only publishes the two
/// admitted TC pins; task-context connector work remains deferred.
struct Gen11HotplugIo<'a, I> {
    io: &'a I,
    events: &'a Events,
    failed: bool,
}

impl<I: IrqIo> intel_display::intel_hotplug_irq_full::HotplugIrqIo for Gen11HotplugIo<'_, I> {
    fn read(&mut self, reg: intel_display::intel_hotplug_irq_full::IrqRegister) -> u32 {
        if reg != intel_display::intel_hotplug_irq_full::IrqRegister::Gen11TcHotplugControl {
            self.failed = true;
            return u32::MAX;
        }
        match self.io.read(GEN11_TC_HOTPLUG_CTL) {
            Some(value) => value,
            None => {
                self.failed = true;
                u32::MAX
            }
        }
    }

    fn write(&mut self, _reg: intel_display::intel_hotplug_irq_full::IrqRegister, _value: u32) {
        self.failed = true;
    }

    fn rmw(
        &mut self,
        reg: intel_display::intel_hotplug_irq_full::IrqRegister,
        clear: u32,
        set: u32,
    ) -> u32 {
        let value = self.read(reg);
        (value & !clear) | set
    }

    fn posting_read(&mut self, _reg: intel_display::intel_hotplug_irq_full::IrqRegister) {}
    fn lock(&mut self, _lock: intel_display::intel_hotplug_irq_full::IrqLock) {}
    fn unlock(&mut self, _lock: intel_display::intel_hotplug_irq_full::IrqLock) {}
    fn assert_lock_held(&mut self, _lock: intel_display::intel_hotplug_irq_full::IrqLock) {}
    fn warn(&mut self, _message: &'static str, _value: u32) {}
    fn warn_once(&mut self, _message: &'static str, _value: u32) {}
    fn log(&mut self, _event: intel_display::intel_hotplug_irq_full::IrqLog) {}

    fn hpd_irq_handler(&mut self, pin_mask: u32, _long_mask: u32) {
        use intel_display::intel_hotplug_irq_full::{HPD_PORT_TC1, HPD_PORT_TC2};
        let mut pending = 0;
        if pin_mask & (1 << HPD_PORT_TC1) != 0 {
            pending |= 1;
        }
        if pin_mask & (1 << HPD_PORT_TC2) != 0 {
            pending |= 2;
        }
        self.events.hpd.fetch_or(pending, Ordering::Release);
    }

    fn dp_aux_irq_handler(&mut self) {}
    fn gmbus_irq_handler(&mut self) {}
    fn update_interrupts(
        &mut self,
        _block: intel_display::intel_hotplug_irq_full::InterruptBlock,
        _mask: u32,
        _enabled: u32,
    ) {
    }
    fn hpd_init_early(&mut self) {}
    fn xelpdp_pica_aux_mask(&self) -> u32 {
        0
    }
}

fn dispatch_gen11_tc_hotplug(io: &impl IrqIo, events: &Events, trigger: u32) -> bool {
    use intel_display::intel_hotplug_irq_full::{
        IntelHotplugIrq, Platform, intel_hpd_gen11_pin_map,
    };

    let mut source = IntelHotplugIrq::new(
        Platform {
            display_ver: 13,
            ..Platform::default()
        },
        Vec::new(),
    );
    source.hpd = Some(intel_hpd_gen11_pin_map());
    let mut adapter = Gen11HotplugIo {
        io,
        events,
        failed: false,
    };
    source.gen11_hpd_irq_handler(&mut adapter, trigger);
    !adapter.failed
}

struct VolatileIo {
    base: usize,
}

impl IrqIo for VolatileIo {
    fn read(&self, offset: u32) -> Option<u32> {
        if !offset.is_multiple_of(4) || offset as usize + 4 > super::regs::PROBE_WINDOW {
            return None;
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: `base` is published only from the retained, live register
        // window; the fixed interrupt dispatch table bounds every offset.
        let value = unsafe { ((self.base + offset as usize) as *const u32).read_volatile() };
        compiler_fence(Ordering::SeqCst);
        (value != u32::MAX).then_some(value)
    }

    fn write(&self, offset: u32, value: u32) -> bool {
        if !offset.is_multiple_of(4) || offset as usize + 4 > super::regs::PROBE_WINDOW {
            return false;
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: the fixed IRQ dispatcher writes only the bounded source
        // acknowledgement and gate registers proved at install time.
        unsafe { ((self.base + offset as usize) as *mut u32).write_volatile(value) };
        compiler_fence(Ordering::SeqCst);
        true
    }
}

fn dispatch(io: &impl IrqIo, events: &Events) -> bool {
    let fault = || {
        let _ = io.write(GFX_MSTR_IRQ, 0);
        let _ = io.write(GEN11_DISPLAY_INT_CTL, 0);
        events.faulted.store(true, Ordering::Release);
        events.online.store(false, Ordering::Release);
        notify_task_context();
        false
    };
    // A final install/rollback failure can leave a posted MSI in flight. The
    // callback and window are deliberately retained, but an offline owner
    // must never acknowledge or re-enable a source during the fallback.
    if !events.online.load(Ordering::Acquire) {
        return fault();
    }
    // The native owner has exclusively admitted the MSI root and all GT
    // source enables. Read, but never clear or rewrite, its status on the
    // normal path; only disable the shared root on an unexpected source.
    let Some(master) = io.read(GFX_MSTR_IRQ) else {
        return fault();
    };
    if master & GEN11_MASTER_IRQ == 0 || master & !GEN11_MASTER_IRQ != GEN11_DISPLAY_IRQ {
        return fault();
    }
    let Some(display) = io.read(GEN11_DISPLAY_INT_CTL) else {
        return fault();
    };
    let display_sources = display & !DISPLAY_IRQ_ENABLE;
    if display_sources == 0
        || display_sources & !(DISPLAY_PIPE_A | DISPLAY_DE_HPD | DISPLAY_PCH) != 0
    {
        return fault();
    }
    if !io.write(GEN11_DISPLAY_INT_CTL, 0) {
        return fault();
    }

    let mut handled = false;
    if display_sources & DISPLAY_PIPE_A != 0 {
        let Some(status) = io.read(PIPE_A_IIR) else {
            return fault();
        };
        let owned = status & PIPE_VBLANK;
        if owned == 0 || status & !PIPE_VBLANK != 0 || !io.write(PIPE_A_IIR, owned) {
            return fault();
        }
        events.vblank.fetch_add(1, Ordering::Release);
        handled = true;
    }
    if display_sources & DISPLAY_DE_HPD != 0 {
        let Some(status) = io.read(DE_HPD_IIR) else {
            return fault();
        };
        let allowed = events.hpd_de_mask.load(Ordering::Acquire);
        let owned = status & allowed;
        if allowed == 0 || owned == 0 || status & !allowed != 0 || !io.write(DE_HPD_IIR, owned) {
            return fault();
        }
        if !dispatch_gen11_tc_hotplug(io, events, owned) {
            return fault();
        }
        handled = true;
    }
    if display_sources & DISPLAY_PCH != 0 {
        let Some(status) = io.read(SDE_IIR) else {
            return fault();
        };
        let allowed = events.hpd_sde_mask.load(Ordering::Acquire);
        let owned = status & allowed;
        if allowed == 0 || owned == 0 || status & !allowed != 0 || !io.write(SDE_IIR, owned) {
            return fault();
        }
        events.hpd.fetch_or(owned >> 24, Ordering::Release);
        handled = true;
    }
    if !handled || !events.online.load(Ordering::Acquire) {
        return fault();
    }
    if !io.write(GEN11_DISPLAY_INT_CTL, DISPLAY_IRQ_ENABLE) {
        return fault();
    }
    if !events.online.load(Ordering::Acquire) {
        return fault();
    }
    if !io.write(GFX_MSTR_IRQ, GEN11_MASTER_IRQ) {
        return fault();
    }
    notify_task_context();
    true
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeMap, vec::Vec};
    use core::sync::atomic::Ordering;

    use spin::Mutex;

    use super::*;

    fn idle_source_image() -> BTreeMap<u32, u32> {
        let mut values = BTreeMap::new();
        values.insert(GFX_MSTR_IRQ, 0);
        values.insert(GEN11_DISPLAY_INT_CTL, 0);
        values.insert(GEN11_TC_HOTPLUG_CTL, 0);
        for register in GT_CLASS_ENABLES {
            values.insert(register, 0);
        }
        for (imr, ier, iir) in SOURCE_BLOCKS {
            values.insert(imr, u32::MAX);
            values.insert(ier, 0);
            values.insert(iir, 0);
        }
        values
    }

    #[test]
    fn source_owner_refuses_any_shared_active_or_unmasked_firmware_irq() {
        let idle = idle_source_image();
        assert!(source_image_idle(|offset| idle.get(&offset).copied()));
        for offset in [GFX_MSTR_IRQ, GEN11_DISPLAY_INT_CTL, GT_CLASS_ENABLES[0]] {
            let mut changed = idle.clone();
            changed.insert(offset, 1);
            assert!(
                !source_image_idle(|r| changed.get(&r).copied()),
                "{offset:#x}"
            );
        }
        for (imr, ier, iir) in SOURCE_BLOCKS {
            for (offset, value) in [(imr, 0), (ier, 1), (iir, 1)] {
                let mut changed = idle.clone();
                changed.insert(offset, value);
                assert!(
                    !source_image_idle(|r| changed.get(&r).copied()),
                    "{offset:#x}"
                );
            }
        }
        assert!(!source_image_idle(|_| None));
    }

    #[test]
    fn gen11_hotplug_decode_uses_translated_pin_map_and_long_pulse_state() {
        let io = Model {
            words: Mutex::new(BTreeMap::from([(GEN11_TC_HOTPLUG_CTL, 2)])),
            ..Default::default()
        };
        let events = Events::new();
        assert!(dispatch_gen11_tc_hotplug(&io, &events, 1 << 16));
        assert_eq!(events.hpd.load(Ordering::Acquire), 1);
        assert!(
            io.writes.lock().is_empty(),
            "IRQ decode must only read the HPD control register"
        );
    }

    #[derive(Default)]
    struct Model {
        words: Mutex<BTreeMap<u32, u32>>,
        writes: Mutex<Vec<(u32, u32)>>,
        fail: Mutex<Option<u32>>,
    }

    impl IrqIo for Model {
        fn read(&self, offset: u32) -> Option<u32> {
            self.words.lock().get(&offset).copied()
        }
        fn write(&self, offset: u32, value: u32) -> bool {
            self.writes.lock().push((offset, value));
            if *self.fail.lock() == Some(offset) {
                return false;
            }
            if [PIPE_A_IIR, DE_HPD_IIR, SDE_IIR].contains(&offset) {
                let mut words = self.words.lock();
                *words.entry(offset).or_default() &= !value;
            } else if offset == GFX_MSTR_IRQ {
                let mut words = self.words.lock();
                let sources = words.get(&offset).copied().unwrap_or(0) & !GEN11_MASTER_IRQ;
                words.insert(offset, sources | (value & GEN11_MASTER_IRQ));
            } else if offset == GEN11_DISPLAY_INT_CTL {
                let mut words = self.words.lock();
                let sources = words.get(&offset).copied().unwrap_or(0) & !DISPLAY_IRQ_ENABLE;
                words.insert(offset, sources | (value & DISPLAY_IRQ_ENABLE));
            } else {
                self.words.lock().insert(offset, value);
            }
            true
        }
    }

    #[test]
    fn irq_dispatch_acks_only_vblank_and_selected_tc_hpd_then_reenables_root() {
        let mut words = BTreeMap::new();
        words.insert(GFX_MSTR_IRQ, GEN11_MASTER_IRQ | GEN11_DISPLAY_IRQ);
        words.insert(
            GEN11_DISPLAY_INT_CTL,
            DISPLAY_IRQ_ENABLE | DISPLAY_PIPE_A | DISPLAY_DE_HPD | DISPLAY_PCH,
        );
        words.insert(PIPE_A_IIR, PIPE_VBLANK);
        words.insert(DE_HPD_IIR, 1 << 16);
        words.insert(GEN11_TC_HOTPLUG_CTL, 2);
        words.insert(SDE_IIR, 1 << 24);
        let io = Model {
            words: Mutex::new(words),
            ..Default::default()
        };
        let events = Events::new();
        events.hpd_de_mask.store(1 << 16, Ordering::Release);
        events.hpd_sde_mask.store(1 << 24, Ordering::Release);
        events.online.store(true, Ordering::Release);
        assert!(dispatch(&io, &events));
        assert_eq!(events.vblank.load(Ordering::Acquire), 1);
        assert_eq!(events.hpd.load(Ordering::Acquire), 1);
        let writes = io.writes.lock().clone();
        assert!(writes.contains(&(PIPE_A_IIR, PIPE_VBLANK)));
        assert!(writes.contains(&(DE_HPD_IIR, 1 << 16)));
        assert!(writes.contains(&(SDE_IIR, 1 << 24)));
        assert_eq!(writes.last(), Some(&(GFX_MSTR_IRQ, GEN11_MASTER_IRQ)));
    }

    #[test]
    fn irq_dispatch_faults_on_an_unselected_tc_hpd_without_acknowledging_it() {
        for (source_register, source_bit) in [(DE_HPD_IIR, 1 << 17), (SDE_IIR, 1 << 25)] {
            let mut words = BTreeMap::new();
            words.insert(GFX_MSTR_IRQ, GEN11_MASTER_IRQ | GEN11_DISPLAY_IRQ);
            words.insert(
                GEN11_DISPLAY_INT_CTL,
                DISPLAY_IRQ_ENABLE
                    | if source_register == DE_HPD_IIR {
                        DISPLAY_DE_HPD
                    } else {
                        DISPLAY_PCH
                    },
            );
            words.insert(source_register, source_bit);
            let io = Model {
                words: Mutex::new(words),
                ..Default::default()
            };
            let events = Events::new();
            events.hpd_de_mask.store(1 << 16, Ordering::Release);
            events.hpd_sde_mask.store(1 << 24, Ordering::Release);
            events.online.store(true, Ordering::Release);

            assert!(!dispatch(&io, &events));
            assert!(events.faulted.load(Ordering::Acquire));
            assert!(!io.writes.lock().contains(&(source_register, source_bit)));
            assert_eq!(
                io.words.lock().get(&GFX_MSTR_IRQ).copied().unwrap_or(0) & GEN11_MASTER_IRQ,
                0
            );
        }
    }

    #[test]
    fn late_posted_msi_on_a_fallback_owner_is_not_acked_or_reenabled() {
        let mut words = BTreeMap::new();
        words.insert(GFX_MSTR_IRQ, GEN11_MASTER_IRQ | GEN11_DISPLAY_IRQ);
        words.insert(GEN11_DISPLAY_INT_CTL, DISPLAY_IRQ_ENABLE | DISPLAY_PIPE_A);
        words.insert(PIPE_A_IIR, PIPE_VBLANK);
        let io = Model {
            words: Mutex::new(words),
            ..Default::default()
        };
        let events = Events::new();

        assert!(!dispatch(&io, &events));
        assert!(events.faulted.load(Ordering::Acquire));
        let writes = io.writes.lock().clone();
        assert!(!writes.iter().any(|(offset, _)| *offset == PIPE_A_IIR));
        assert!(!writes.contains(&(GFX_MSTR_IRQ, GEN11_MASTER_IRQ)));
        assert_eq!(
            io.words.lock().get(&GFX_MSTR_IRQ).copied().unwrap_or(0) & GEN11_MASTER_IRQ,
            0
        );
    }

    #[test]
    fn irq_dispatch_disables_shared_master_on_unknown_gt_or_display_source() {
        for (master, display) in [
            (
                GEN11_MASTER_IRQ | GEN11_DISPLAY_IRQ | 1,
                DISPLAY_IRQ_ENABLE | DISPLAY_PIPE_A,
            ),
            (
                GEN11_MASTER_IRQ | GEN11_DISPLAY_IRQ,
                DISPLAY_IRQ_ENABLE | (1 << 22),
            ),
        ] {
            let mut words = BTreeMap::new();
            words.insert(GFX_MSTR_IRQ, master);
            words.insert(GEN11_DISPLAY_INT_CTL, display);
            words.insert(PIPE_A_IIR, PIPE_VBLANK);
            let io = Model {
                words: Mutex::new(words),
                ..Default::default()
            };
            let events = Events::new();
            events.online.store(true, Ordering::Release);
            assert!(!dispatch(&io, &events));
            assert!(events.faulted.load(Ordering::Acquire));
            assert_eq!(
                io.words.lock().get(&GFX_MSTR_IRQ).copied().unwrap_or(0) & GEN11_MASTER_IRQ,
                0
            );
        }
    }

    #[test]
    fn failed_owner_can_only_release_before_any_shared_master_write_attempt() {
        assert!(may_release_failed_owner(true, false));
        assert!(!may_release_failed_owner(false, false));
        assert!(!may_release_failed_owner(true, true));
        assert!(!may_release_failed_owner(false, true));
    }
}
