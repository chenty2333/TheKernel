//! Low-level driver for the Broadcom VideoCore IV V3D 3D engine
//! (Raspberry Pi 0-3 and Zero 2W).
//!
//! `#![no_std]`, OS-independent. The crate implements:
//!
//! * hardware bring-up: V3D clock via the firmware mailbox, identity check,
//!   binner overflow memory, interrupts;
//! * the submission path: bin control list (BCL) and render control list
//!   (RCL) primitives, cache flushing, and completion interrupts
//!   (`FLDONE` / `FRDONE`);
//! * command list builders for the clear-only job shape ([`cl`]).
//!
//! The interface mirrors the upstream Linux `vc4` driver's job flow
//! (binner thread 0 then render thread 1), without the DRM layer: callers
//! own the command lists and the binner memory, and consume completion via
//! [`V3dCore::handle_irq`] or by polling [`V3dCore::seqno`].

#![cfg_attr(not(test), no_std)]

use core::ptr::{read_volatile, write_volatile};

use rdif_base::DriverGeneric;

pub mod cl;
pub mod mailbox;
pub mod regs;

use regs::*;

/// Resources supplied by the OS glue.
#[derive(Clone, Copy, Debug)]
pub struct V3dResources {
    /// ARM-view physical address of the V3D block (`0x3fc0_0000`).
    pub base: usize,
    /// Bus (physical) address of the binner memory (tile state + tile alloc
    /// + overflow). Must have constant top 4 bits across its whole extent.
    pub binner_bus_addr: u32,
    /// Size of the binner memory region in bytes.
    pub binner_size: u32,
    /// Binner overflow address register value (bus address of the overflow
    /// area, or 0 if overflow handling is not wired up yet).
    pub binner_overflow_addr: u32,
    /// Interrupt number (42 on BCM283x) if wired up.
    pub irq: Option<usize>,
}

/// Lifecycle state of the V3D core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum V3dState {
    /// Hardware not yet brought up.
    Off,
    /// Hardware initialized and ready to accept jobs.
    Ready,
    /// A bin job is running on thread 0.
    Binning,
    /// A render job is running on thread 1.
    Rendering,
}

/// V3D 3D engine core.
pub struct V3dCore {
    resources: V3dResources,
    state: V3dState,
    seqno: u64,
    finished_seqno: u64,
}

impl DriverGeneric for V3dCore {
    fn name(&self) -> &str {
        "bcm283x-v3d"
    }
}

impl V3dCore {
    /// Creates a core over the given resources. Does not touch hardware.
    pub fn new(resources: V3dResources) -> Self {
        Self {
            resources,
            state: V3dState::Off,
            seqno: 0,
            finished_seqno: 0,
        }
    }

    fn read(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.resources.base + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.resources.base + offset) as *mut u32, value) }
    }

    /// Brings up the hardware: requests the V3D clock from the firmware,
    /// verifies the identity register, clears the binner overflow
    /// configuration, and enables the completion interrupts.
    pub fn init(&mut self) {
        if self.state != V3dState::Off {
            return;
        }
        if !mailbox::set_clock_rate(mailbox::CLK_V3D, 250_000_000) {
            log::warn!("bcm283x-v3d: firmware rejected V3D clock request");
        }

        let ident0 = self.read(V3D_IDENT0);
        assert_eq!(
            ident0, V3D_EXPECTED_IDENT0,
            "bcm283x-v3d: V3D_IDENT0 read 0x{ident0:08x} instead of 0x{V3D_EXPECTED_IDENT0:08x}"
        );
        let ident1 = self.read(V3D_IDENT1);
        log::info!(
            "bcm283x-v3d: revision {} slices {} qpus {}",
            (ident1 >> 0) & 0xf,
            (ident1 >> 4) & 0xf,
            (ident1 >> 8) & 0xf,
        );

        // Take all memory that would have been reserved for user QPU
        // programs (no user QPU interface here).
        self.write(V3D_VPMBASE, 0);
        // Reset the binner overflow address/size.
        self.write(V3D_BPOA, self.resources.binner_overflow_addr);
        self.write(V3D_BPOS, self.resources.binner_size);

        // Clear pending interrupts and enable completion interrupts.
        self.write(V3D_INTCTL, V3D_DRIVER_IRQS);
        self.write(V3D_INTENA, V3D_INT_FLDONE | V3D_INT_FRDONE);

        self.state = V3dState::Ready;
        log::info!("bcm283x-v3d: hardware ready");
    }

    /// Flushes the V3D L2 and slice caches (mirrors `vc4_flush_caches`).
    pub fn flush_caches(&self) {
        self.write(V3D_L2CACTL, V3D_L2CACTL_L2CCLR);
        self.write(
            V3D_SLCACTL,
            V3D_SLCACTL_T1CC | V3D_SLCACTL_T0CC | V3D_SLCACTL_UCC | V3D_SLCACTL_ICC,
        );
    }

    /// Flushes only the texture caches (mirrors `vc4_flush_texture_caches`).
    pub fn flush_texture_caches(&self) {
        self.write(V3D_L2CACTL, V3D_L2CACTL_L2CCLR);
        self.write(V3D_SLCACTL, V3D_SLCACTL_T1CC | V3D_SLCACTL_T0CC);
    }

    /// Submits a control list to the given thread (0 = binner, 1 = renderer).
    ///
    /// Writing the end register is what starts the job.
    pub fn submit_cl(&mut self, thread: u32, start: u32, end: u32) -> u64 {
        assert!(thread <= 1, "V3D has exactly two threads");
        assert!(self.state != V3dState::Off, "V3D not initialized");
        if thread == 0 {
            self.state = V3dState::Binning;
        } else {
            self.state = V3dState::Rendering;
        }
        self.seqno += 1;
        let seqno = self.seqno;
        self.write(V3D_CTNCA + (thread as usize) * 4, start);
        self.write(V3D_CTNEA + (thread as usize) * 4, end);
        log::trace!("bcm283x-v3d: submitted thread {thread} cl {start:#x}..{end:#x} seqno {seqno}");
        seqno
    }

    /// Submits a full binned frame: the binner CL on thread 0 followed by the
    /// render CL on thread 1 (as soon as the binner completes). The caller
    /// provides the command list memory (bus addresses) and the binner
    /// memory layout.
    pub fn submit_frame(&mut self, bin_cl: Option<(u32, u32)>, rcl: (u32, u32)) -> u64 {
        self.flush_caches();
        match bin_cl {
            Some((start, end)) => {
                let seqno = self.submit_cl(0, start, end);
                self.submit_cl(1, rcl.0, rcl.1);
                seqno
            }
            None => {
                // No binning needed: run the RCL directly on the render thread.
                self.flush_texture_caches();
                self.submit_cl(1, rcl.0, rcl.1)
            }
        }
    }

    /// Handles the V3D interrupt; returns the handled IRQ mask. Call from
    /// interrupt context.
    pub fn handle_irq(&mut self) -> u32 {
        let intctl = self.read(V3D_INTCTL);
        if intctl == 0 {
            return 0;
        }
        // Acknowledge (write-1-to-clear).
        self.write(V3D_INTCTL, intctl);
        if intctl & V3D_INT_FLDONE != 0 {
            log::trace!("bcm283x-v3d: bin job done");
            if self.state == V3dState::Binning {
                self.state = V3dState::Rendering;
            }
        }
        if intctl & V3D_INT_FRDONE != 0 {
            log::trace!("bcm283x-v3d: render job done");
            self.finished_seqno += 1;
            self.state = V3dState::Ready;
        }
        intctl
    }

    /// Waits until the given seqno has finished (busy poll).
    pub fn wait_for_seqno(&self, seqno: u64) {
        while self.finished_seqno < seqno {
            core::hint::spin_loop();
        }
    }

    /// Sequence number of the last submitted job.
    pub fn seqno(&self) -> u64 {
        self.seqno
    }

    /// Sequence number of the last finished job.
    pub fn finished_seqno(&self) -> u64 {
        self.finished_seqno
    }

    /// Current lifecycle state.
    pub fn state(&self) -> V3dState {
        self.state
    }

    /// Resets the interrupt controller state (used after a hang).
    pub fn irq_reset(&mut self) {
        self.write(V3D_INTCTL, V3D_DRIVER_IRQS);
        self.write(V3D_INTENA, V3D_DRIVER_IRQS);
    }
}

/// In-crate selftest: identity + a halt-only job through both threads.
///
/// The caller must supply command list memory (bus + virtual addresses).
pub fn selftest(core: &mut V3dCore, cl_bus_addr: u32, cl_vaddr: usize, cl_capacity: usize) -> bool {
    core.init();
    let cl = unsafe { core::slice::from_raw_parts_mut(cl_vaddr as *mut u8, cl_capacity) };
    let len = cl::build_halt_cl(cl);
    // Binner thread halt: exercises CTNCA/CTNEA(0) and FLDONE.
    let seqno = core.submit_cl(0, cl_bus_addr, cl_bus_addr + len as u32);
    let mut handled = false;
    for _ in 0..10_000 {
        if core.handle_irq() & V3D_INT_FLDONE != 0 {
            handled = true;
            break;
        }
    }
    core.wait_for_seqno(seqno);
    core.state == V3dState::Ready && handled
}
