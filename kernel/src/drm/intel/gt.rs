// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Independently opted-in N305 BCS/RCS execution adapter. Never reached from the
//! display modeset flag; display D0 is not used as the GT/media A0 stepping.
#[cfg(target_os = "none")]
use alloc::{format, string::String};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering, compiler_fence};

use axsync::Mutex;
use intel_gt::{Error, GtIo};

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
    rcs_owned: AtomicBool,
    media_present: AtomicU8,
    media_awake: AtomicU8,
}
impl Bus {
    fn acquire_idle_media(&self) -> Result<(), Error> {
        let present = intel_gt::uncore::media_mask(self.read(0x9140)?);
        self.media_present.store(present, Ordering::Release);
        for index in 0..intel_gt::uncore::MEDIA.len() {
            if present & (1 << index) != 0 {
                intel_gt::uncore::acquire_media(self, index)?;
                self.media_awake.fetch_or(1 << index, Ordering::Release);
            }
        }
        self.assert_media_idle()
    }
    fn assert_media_idle(&self) -> Result<(), Error> {
        if self.read(intel_gt::uncore::GT_ACK)? & 1 == 0
            || self.read(intel_gt::uncore::RENDER_ACK)? & 1 == 0
        {
            return Err(Error::Refused);
        }
        intel_gt::uncore::idle_media(
            self,
            self.media_present.load(Ordering::Acquire),
            self.media_awake.load(Ordering::Acquire),
        )
    }
    fn prepare_shared(&self) -> Result<(), Error> {
        self.assert_media_idle()?;
        intel_gt::bcs::prepare(self)
    }
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
        if [0x2358, 0x235c].contains(&r) {
            return !write
                && self.awake.load(Ordering::Acquire)
                && self.render_awake.load(Ordering::Acquire);
        }
        if [intel_gt::uncore::GT_ACK, intel_gt::uncore::RENDER_ACK].contains(&r) {
            return !write;
        }
        for (index, (_, request, ack, mode)) in intel_gt::uncore::MEDIA.iter().enumerate() {
            if r == *request {
                return true;
            }
            if r == *ack {
                return !write;
            }
            if [*mode, *mode - 0x9c + 0x30, *mode - 0x9c + 0x34].contains(&r) {
                return !write && self.media_awake.load(Ordering::Acquire) & (1 << index) != 0;
            }
        }
        if [0x2030, 0x2034].contains(&r) {
            return !write && self.render_awake.load(Ordering::Acquire);
        }
        if [0xb024, 0x209c, 0x9550].contains(&r) {
            return self.render_awake.load(Ordering::Acquire)
                && (!write || r != 0x209c || self.rcs_owned.load(Ordering::Acquire));
        }
        if (0x224d0..0x22500).contains(&r) {
            return self.awake.load(Ordering::Acquire);
        }
        if (0x24d0..0x2500).contains(&r) {
            return self.rcs_owned.load(Ordering::Acquire)
                && self.render_awake.load(Ordering::Acquire);
        }
        if self.rcs_owned.load(Ordering::Acquire) && self.render_awake.load(Ordering::Acquire) {
            if matches!(r, 0x2030 | 0x2034 | 0x8000) {
                return !write;
            }
            if matches!(
                r,
                0x209c
                    | 0x229c
                    | 0x20d0
                    | 0x2080
                    | 0x2098
                    | 0x20a8
                    | 0x20b0
                    | 0x20b4
                    | 0x20c4
                    | 0x23a0
                    | 0x2510
                    | 0x2514
                    | 0x2518
                    | 0x251c
                    | 0x2550
                    | 0x20ec
                    | 0xe4f4
                    | 0xe18c
                    | 0xe48c
                    | 0x2050
                    | 0x20e0
                    | 0xb004
                    | 0x20a0
            ) {
                return true;
            }
            if matches!(r, 0x20b8 | 0x5584) {
                return !write;
            }
        }
        self.awake.load(Ordering::Acquire)
            && match r {
                0xc000 | 0x800c | 0xa2a0 | 0x22030 | 0x22034 => !write,
                0x941c | 0x2209c | 0x2229c | 0x220d0 => true,
                0xfdc | 0x9424 | 0x480c | 0x400c | 0x22080 | 0x22098 | 0x220a8 | 0x220b0
                | 0x220b4 | 0x220c4 | 0x223a0 | 0x22510 | 0x22514 | 0x22518 | 0x2251c | 0x22550 => {
                    true
                }
                0x220b8 | 0x9134 | 0x9138 | 0x913c | 0x9140 | 0xa26c | 0x44074 | 0xd00 => !write,
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
        if !self.allowed(r, true)
            || (r == 0x941c
                && value != 1 << 2
                && !(value == 1 << 1 && self.rcs_owned.load(Ordering::Acquire)))
        {
            return Err(Error::Refused);
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: same bounded owned GT allowlist. No display/global reset is
        // allowed; GDRST is restricted to BCS (bit2) or owned RCS (bit1).
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
    render_ready: bool,
    // Published before an ELSQ load; retained through any ambiguous reset/DMA.
    memory: Option<copy::Memory>,
}
pub(super) mod copy;
static READY: AtomicBool = AtomicBool::new(false);
pub(super) fn registered() -> bool {
    READY.load(Ordering::Acquire)
}
static OWNER: Mutex<Option<Owner>> = Mutex::new(None);

/// Capability probes use only a successfully bootstrapped, still-live owner.
/// No query wakes/resets hardware or invents a fused topology/clock.
pub(super) fn topology() -> Result<intel_gt::info::Topology, Error> {
    let state = OWNER.lock();
    let owner = state
        .as_ref()
        .filter(|o| registered() && !o.lost)
        .ok_or(Error::Refused)?;
    intel_gt::info::Topology::read(&owner.bus)
}
pub(super) fn clock_frequency() -> Result<u32, Error> {
    let state = OWNER.lock();
    let owner = state
        .as_ref()
        .filter(|o| registered() && !o.lost)
        .ok_or(Error::Refused)?;
    intel_gt::info::clock_frequency(&owner.bus)
}
pub(super) fn timestamp() -> Result<u64, Error> {
    let state = OWNER.lock();
    let owner = state
        .as_ref()
        .filter(|o| registered() && !o.lost)
        .ok_or(Error::Refused)?;
    if owner.bus.read(intel_gt::uncore::GT_ACK)? & 1 == 0
        || owner.bus.read(intel_gt::uncore::RENDER_ACK)? & 1 == 0
    {
        return Err(Error::Refused);
    }
    intel_gt::info::timestamp(&owner.bus)
}
pub(super) fn context_isolation_classes() -> u32 {
    let owner = OWNER.lock();
    if !owner.as_ref().is_some_and(|o| registered() && !o.lost) {
        return 0;
    }
    copy::isolation_classes()
}
pub(super) fn render_registered() -> bool {
    OWNER
        .lock()
        .as_ref()
        .is_some_and(|o| registered() && !o.lost && o.render_ready)
}

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
        rcs_owned: AtomicBool::new(false),
        media_present: AtomicU8::new(0x80),
        media_awake: AtomicU8::new(0),
    };
    if let Err(error) = intel_gt::uncore::acquire_gt(&bus) {
        *owner = Some(Owner {
            bdf,
            bus,
            lost: true,
            render_ready: false,
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
                render_ready: false,
                memory: None,
            };
            let copied = copy::run(&mut device, bdf).and_then(|()| {
                if axhal::boot::command_line_value("intel.rcs") == Some("1") {
                    copy::render_test(&mut device)?;
                    device.render_ready = true;
                    axlog::info!(
                        "intel-gt: RCS_SHADER_BYTES_AND_GUARDS_VERIFIED after hardware completion \
                         and reset; not Mesa acceptance"
                    );
                }
                Ok(())
            });
            if copied.is_ok() {
                READY.store(true, Ordering::Release);
            }
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
                        "intel-gt: BCS selftest failed {e:?}; unsafe-to-retire DMA owners \
                         retained, terminal submission"
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
                render_ready: false,
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
pub(super) fn submit_copy(
    source: alloc::sync::Arc<crate::mm::SharedPages>,
    destination: alloc::sync::Arc<crate::mm::SharedPages>,
    operation: intel_gt::bcs::Copy,
    vm: alloc::sync::Arc<copy::Vm>,
    saved: alloc::sync::Arc<copy::SavedContext>,
) -> Result<(), Error> {
    if !registered() {
        return Err(Error::Refused);
    }
    let mut state = OWNER.lock();
    let owner = state.as_mut().ok_or(Error::Refused)?;
    let result = copy::objects(owner, source, destination, operation, vm, saved);
    if result.is_err() {
        owner.lost = true;
    }
    result
}
#[cfg(target_os = "none")]
pub(super) fn submit_render(
    source: alloc::sync::Arc<crate::mm::SharedPages>,
    destination: alloc::sync::Arc<crate::mm::SharedPages>,
    vm: alloc::sync::Arc<copy::Vm>,
    saved: alloc::sync::Arc<copy::SavedContext>,
) -> Result<(), Error> {
    if !registered() {
        return Err(Error::Refused);
    }
    let mut state = OWNER.lock();
    let owner = state.as_mut().ok_or(Error::Refused)?;
    if !owner.render_ready {
        return Err(Error::Refused);
    }
    let result = copy::render_objects(owner, source, destination, vm, saved);
    if result.is_err() {
        owner.lost = true;
    }
    result
}
#[cfg(not(target_os = "none"))]
pub(super) fn submit_render(
    _source: alloc::sync::Arc<crate::mm::SharedPages>,
    _destination: alloc::sync::Arc<crate::mm::SharedPages>,
    _vm: alloc::sync::Arc<copy::Vm>,
    _saved: alloc::sync::Arc<copy::SavedContext>,
) -> Result<(), Error> {
    Err(Error::Refused)
}
#[cfg(not(target_os = "none"))]
pub(super) fn submit_copy(
    _source: alloc::sync::Arc<crate::mm::SharedPages>,
    _destination: alloc::sync::Arc<crate::mm::SharedPages>,
    _operation: intel_gt::bcs::Copy,
    _vm: alloc::sync::Arc<copy::Vm>,
    _saved: alloc::sync::Arc<copy::SavedContext>,
) -> Result<(), Error> {
    Err(Error::Refused) // No host/native CPU-copy fallback.
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    #[test]
    fn unknown_or_busy_media_is_refused_before_shared_policy_and_media_writes_are_forbidden() {
        let mut words = vec![0u32; 0x200000 / 4];
        // SAFETY: aligned private stable model memory, not a hardware BAR.
        let window =
            unsafe { RegisterWindow::from_mapped(words.as_mut_ptr() as usize, words.len() * 4) };
        let bus = Bus {
            window,
            awake: AtomicBool::new(true),
            render_awake: AtomicBool::new(true),
            rcs_owned: AtomicBool::new(false),
            media_present: AtomicU8::new(0x80),
            media_awake: AtomicU8::new(0),
        };
        words[intel_gt::uncore::GT_ACK as usize / 4] = 1;
        words[intel_gt::uncore::RENDER_ACK as usize / 4] = 1;
        words[0x9138 / 4] = 1;
        words[0x913c / 4] = 1;
        words[0xfdc / 4] = 1 << 31;
        assert_eq!(bus.prepare_shared(), Err(Error::Refused));
        bus.media_present.store(1, Ordering::Release);
        bus.media_awake.store(1, Ordering::Release);
        words[0xd50 / 4] = 1;
        assert_eq!(bus.prepare_shared(), Err(Error::Refused));
        assert_eq!(words[0x9550 / 4], 0);
        assert_eq!(words[0x400c / 4], 0);
        assert_eq!(bus.write(0x1c009c, 1 << 8), Err(Error::Refused));
        assert_eq!(bus.write(0x1c0030, 0), Err(Error::Refused));
        words[0x1c009c / 4] = 1 << 9;
        bus.assert_media_idle().unwrap();
        words[0x1c0030 / 4] = 8;
        assert_eq!(bus.assert_media_idle(), Err(Error::Refused));
    }
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
            rcs_owned: AtomicBool::new(false),
            media_present: AtomicU8::new(0x80),
            media_awake: AtomicU8::new(0),
        };
        assert_eq!(bus.write(0x2209c, 0x01000100), Err(Error::Refused));
        assert_eq!(bus.read(0x22030), Err(Error::Unavailable(0x22030)));
        assert_eq!(bus.read(intel_gt::uncore::GT_ACK), Ok(0));
        bus.awake.store(true, Ordering::Release);
        assert_eq!(bus.write(0x941c, 1), Err(Error::Refused));
        assert_eq!(bus.write(0x941c, 8), Err(Error::Refused));
        assert_eq!(bus.write(0x46038, u32::MAX), Err(Error::Refused));
        assert_eq!(bus.write(0x941c, 2), Err(Error::Refused));
        assert_eq!(bus.write(0x2550, 1), Err(Error::Refused));
        bus.render_awake.store(true, Ordering::Release);
        assert_eq!(bus.write(0x2550, 1), Err(Error::Refused));
        bus.rcs_owned.store(true, Ordering::Release);
        bus.write(0x941c, 2).unwrap();
        bus.write(0x2550, 1).unwrap();
        assert_eq!(bus.write(0x941c, 6), Err(Error::Refused));
        assert_eq!(bus.write(0xc000, 0), Err(Error::Refused));
        bus.write(0x941c, 4).unwrap();
        assert_eq!(words[0x941c / 4], 4);
        assert_eq!(words[0x46038 / 4], 0);
    }
}
