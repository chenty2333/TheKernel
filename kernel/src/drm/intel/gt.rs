// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Independently opted-in N305 BCS execution adapter. Never reached from the
//! display modeset flag; display D0 is not used as the GT/media A0 stepping.
#[cfg(target_os = "none")]
use alloc::{format, string::String};
use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};

use intel_gt::{Error, GtIo};
use spin::Mutex;

#[cfg(target_os = "none")]
use super::pci;
use super::{
    gmbus::{MonotonicTimer, PollTimer},
    regs::RegisterWindow,
};

struct Bus {
    window: RegisterWindow,
    awake: AtomicBool,
}
impl Bus {
    fn allowed(&self, r: u32, write: bool) -> bool {
        if !r.is_multiple_of(4) || r as usize + 4 > self.window.len() {
            return false;
        }
        if r == intel_gt::uncore::GT_REQUEST {
            return true;
        }
        if r == intel_gt::uncore::GT_ACK {
            return !write;
        }
        self.awake.load(Ordering::Acquire)
            && match r {
                0xc000 | 0x800c | 0xa2a0 | 0x22030 | 0x22034 => !write,
                0x941c | 0x2209c | 0x2229c | 0x220d0 => true,
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
    bus: Bus,
    lost: bool,
}
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
    };
    if let Err(error) = intel_gt::uncore::acquire_gt(&bus) {
        *owner = Some(Owner { bus, lost: true });
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
            *owner = Some(Owner { bus, lost: false });
            Ok(String::from(
                "intel-gt: owned GT wake and BCS-only stop/reset initialized, GT/media A0; BCS \
                 copy NOT submitted; context/address-space setup follows; 未在硬件上验证",
            ))
        }
        Err(error) => {
            let released = intel_gt::uncore::release_gt(&bus).is_ok();
            if released {
                bus.awake.store(false, Ordering::Release);
            }
            *owner = Some(Owner { bus, lost: true });
            Err(format!(
                "intel-gt: initialization failed {error:?}; wake-release-verified={released}; no \
                 BCS submission"
            ))
        }
    }
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
