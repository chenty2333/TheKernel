// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Independently opted-in N305 BCS execution adapter. Never reached from the
//! display modeset flag; display D0 is not used as the GT/media A0 stepping.
#[cfg(target_os = "none")]
use alloc::{format, string::String};
use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};

use intel_gt::{Error, GtIo};
use axsync::Mutex;

#[cfg(target_os = "none")]
use super::pci;
use super::{
    gmbus::{MonotonicTimer, PollTimer},
    regs::RegisterWindow,
};

struct Bus {
    window: RegisterWindow,
    awake: AtomicBool,
    render_awake: AtomicBool,
}
impl Bus {
    fn allowed(&self, r: u32, write: bool) -> bool {
        if !r.is_multiple_of(4) || r as usize + 4 > self.window.len() {
            return false;
        }
        if [
            intel_gt::uncore::GT_REQUEST,
            intel_gt::uncore::RENDER_REQUEST,
        ]
        .contains(&r)
        {
            return true;
        }
        if [intel_gt::uncore::GT_ACK, intel_gt::uncore::RENDER_ACK].contains(&r) {
            return !write;
        }
        if [0xb024, 0x209c, 0x9550].contains(&r) {
            return self.render_awake.load(Ordering::Acquire) && (!write || r != 0x209c);
        }
        self.awake.load(Ordering::Acquire)
            && match r {
                0xc000 | 0x800c | 0xa2a0 | 0x22030 | 0x22034 => !write,
                0x941c | 0x2209c | 0x2229c | 0x220d0 => true,
                0xfdc | 0x9424 | 0x480c | 0x400c | 0x22080 | 0x22098 | 0x220a8 | 0x220b0 | 0x220b4 | 0x220c4
                | 0x223a0 | 0x22510 | 0x22514 | 0x22518 | 0x2251c | 0x22550 => true,
                0x220b8 | 0x9138 | 0x913c => !write,
                _ => false,
            }
    }
}
impl GtIo for Bus {
    fn read(&self, r: u32) -> Result<u32, Error> {
        if !self.allowed(r, false) {
            return Err(Error::Unavailable(r));
        }
        // SAFETY: private audited aligned allowlist in the live mapped BAR;
        // forcewake is owned before every GT-gated access, not just sampled.
        let value = unsafe { ((self.window.base() + r as usize) as *const u32).read_volatile() };
        compiler_fence(Ordering::SeqCst);
        if value == u32::MAX {
            Err(Error::Unavailable(r))
        } else {
            Ok(value)
        }
    }
    fn write(&self, r: u32, value: u32) -> Result<(), Error> {
        if !self.allowed(r, true) || (r == 0x941c && value != 1 << 2) {
            return Err(Error::Refused);
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: same bounded owned GT allowlist. No display/global reset is
        // allowed; GDRST is restricted to the Gen11+ BCS domain (bit2).
        unsafe { ((self.window.base() + r as usize) as *mut u32).write_volatile(value) };
        compiler_fence(Ordering::SeqCst);
        Ok(())
    }
    fn now_us(&self) -> u64 {
        MonotonicTimer.now_micros()
    }
    fn delay_us(&self, micros: u32) {
        let start = self.now_us();
        while self.now_us().saturating_sub(start) < u64::from(micros) {
            MonotonicTimer.pause();
        }
    }
}
struct Owner {
    bdf: super::pci::Bdf,
    bus: Bus,
    lost: bool,
    // Published before an ELSQ load; retained through any ambiguous reset/DMA.
    memory: Option<copy::Memory>,
}
pub(super) mod copy;
static READY: AtomicBool = AtomicBool::new(false);
pub(super) fn registered() -> bool { READY.load(Ordering::Acquire) }
static OWNER: Mutex<Option<Owner>> = Mutex::new(None);

/// Independent boot hook; default path never writes forcewake or resets GT.
#[cfg(target_os = "none")]
pub(super) fn init_at_boot() {
    if axhal::boot::command_line_value("intel.gt") != Some("1") {
        return;
    }
    let windows = super::mapped_windows();
    let result = if windows.len() != 1 {
        Err(String::from(
            "intel.gt=1 refused: require one mapped N305 GPU",
        ))
    } else {
        let (bdf, window) = windows[0];
        initialize(bdf, window)
    };
    let text = result.unwrap_or_else(|s| s);
    axlog::info!("{text}");
    if let Some((bdf, _)) = windows.first() {
        super::GT_REPORT.lock().push((*bdf, text));
    }
}
#[cfg(target_os = "none")]
fn initialize(bdf: pci::Bdf, window: RegisterWindow) -> Result<String, String> {
    let mut owner = OWNER.lock();
    if owner.is_some() {
        return Err(String::from(
            "GT owner already initialized; no repeated reset",
        ));
    }
    let ecam =
        pci::Ecam::platform().ok_or_else(|| String::from("GT PCI facts unavailable; no writes"))?;
    let info = pci::DeviceInfo::read(&ecam, bdf)
        .ok_or_else(|| String::from("GT PCI device unavailable; no writes"))?;
    // Local i915 intel_step.c::adlp_n_revids[0] COMMON_STEP(A0), not display D0.
    if (info.vendor_id, info.device_id, info.revision) != (0x8086, 0x46d0, 0) {
        return Err(String::from(
            "GT requires exact N305 Gen12/media A0; no writes",
        ));
    }
    let bus = Bus {
        window,
        awake: AtomicBool::new(false),
        render_awake: AtomicBool::new(false),
    };
    if let Err(error) = intel_gt::uncore::acquire_gt(&bus) {
        *owner = Some(Owner {
            bdf,
            bus,
            lost: true,
            memory: None,
        });
        return Err(format!(
            "GT forcewake failed: {error:?}; terminal owner, no submission"
        ));
    }
    bus.awake.store(true, Ordering::Release);
    let result = (|| {
        // Do not race an unowned GuC submission controller. No firmware has
        // been loaded by this driver; hardware must corroborate MIA reset.
        if bus.read(0xc000)? & 1 == 0 {
            return Err(Error::Refused);
        }
        intel_gt::reset::stop_and_reset_bcs(&bus)
    })();
    match result {
        Ok(()) => {
            let mut device = Owner {
                bdf,
                bus,
                lost: false,
                memory: None,
            };
            let copied = copy::run(&mut device, bdf);
            if copied.is_ok() { READY.store(true, Ordering::Release); }
            if copied.is_err() {
                device.lost = true;
            }
            *owner = Some(device);
            copied
                .map(|_| {
                    String::from(
                        "intel-gt: BCS_COPY_BYTES_AND_GUARDS_VERIFIED after hardware breadcrumb \
                         and reset retirement; GT/media A0; not RCS/Mesa rendering",
                    )
                })
                .map_err(|e| {
                    format!(
                        "intel-gt: BCS selftest failed {e:?}; unsafe-to-retire DMA owners retained, terminal \
                         submission"
                    )
                })
        }
        Err(error) => {
            let released = intel_gt::uncore::release_gt(&bus).is_ok();
            if released {
                bus.awake.store(false, Ordering::Release);
            }
            *owner = Some(Owner {
                bdf,
                bus,
                lost: true,
                memory: None,
            });
            Err(format!(
                "intel-gt: initialization failed {error:?}; wake-release-verified={released}; no \
                 BCS submission"
            ))
        }
    }
}

#[cfg(target_os = "none")]
pub(super) fn submit_copy(source: alloc::sync::Arc<crate::mm::SharedPages>, destination: alloc::sync::Arc<crate::mm::SharedPages>, operation: intel_gt::bcs::Copy) -> Result<(), Error> {
    if !registered() { return Err(Error::Refused); }
    let mut state = OWNER.lock();
    let owner = state.as_mut().ok_or(Error::Refused)?;
    let result = copy::objects(owner, source, destination, operation);
    if result.is_err() { owner.lost = true; }
    result
}
#[cfg(not(target_os = "none"))]
pub(super) fn submit_copy(_source: alloc::sync::Arc<crate::mm::SharedPages>, _destination: alloc::sync::Arc<crate::mm::SharedPages>, _operation: intel_gt::bcs::Copy) -> Result<(), Error> {
    Err(Error::Refused) // No host/native CPU-copy fallback.
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    #[test]
    fn native_gt_window_requires_owned_wake_and_rejects_display_or_global_reset() {
        let mut words = vec![0u32; 0x130048 / 4];
        // SAFETY: aligned owned model memory, stable allocation and lifetime;
        // this test is the only writer. No actual PCI/hardware address is used.
        let window =
            unsafe { RegisterWindow::from_mapped(words.as_mut_ptr() as usize, words.len() * 4) };
        let bus = Bus {
            window,
            awake: AtomicBool::new(false),
            render_awake: AtomicBool::new(false),
        };
        assert_eq!(bus.write(0x2209c, 0x01000100), Err(Error::Refused));
        assert_eq!(bus.read(0x22030), Err(Error::Unavailable(0x22030)));
        assert_eq!(bus.read(intel_gt::uncore::GT_ACK), Ok(0));
        bus.awake.store(true, Ordering::Release);
        assert_eq!(bus.write(0x941c, 1), Err(Error::Refused));
        assert_eq!(bus.write(0x941c, 8), Err(Error::Refused));
        assert_eq!(bus.write(0x46038, u32::MAX), Err(Error::Refused));
        bus.write(0x941c, 4).unwrap();
        assert_eq!(words[0x941c / 4], 4);
        assert_eq!(words[0x46038 / 4], 0);
    }
}
