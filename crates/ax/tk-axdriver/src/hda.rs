//! Intel HDA platform seam for codec playback and existing display-ELD routing.
use core::ptr::NonNull;

use axalloc::{UsageKind, global_allocator};
use axdriver_base::{DevError, DevResult};
use axdriver_hda::{Controller, Hal, regs::Bus};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use spin::Mutex;
use tk_rdif_audio::{Playback, PlaybackError, PlaybackToken};

use crate::drivers::{BusProbeResult, DriverProbe};
pub struct PlatformHal;
// SAFETY: x86 coherent identity DMA; owned page-aligned contiguous allocations.
unsafe impl Hal for PlatformHal {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)> {
        let address = global_allocator()
            .alloc_pages(pages, 4096, UsageKind::Dma)
            .ok()?;
        Some((
            axhal::mem::virt_to_phys(address.into()).as_usize() as u64,
            NonNull::new(address as *mut u8)?,
        ))
    }
    unsafe fn release(_address: u64, pointer: NonNull<u8>, pages: usize) {
        global_allocator().dealloc_pages(pointer.as_ptr() as usize, pages, UsageKind::Dma);
    }
}
pub struct Window {
    base: usize,
    size: usize,
}
impl Bus for Window {
    fn read(&mut self, offset: usize, width: usize) -> u32 {
        if ![1, 2, 4].contains(&width)
            || !offset.is_multiple_of(width)
            || offset.checked_add(width).is_none_or(|end| end > self.size)
        {
            return u32::MAX;
        }
        // SAFETY: probe mapped the BAR; access is width-aligned and bounded.
        unsafe {
            match width {
                1 => ((self.base + offset) as *const u8).read_volatile() as u32,
                2 => ((self.base + offset) as *const u16).read_volatile() as u32,
                _ => ((self.base + offset) as *const u32).read_volatile(),
            }
        }
    }
    fn write(&mut self, offset: usize, width: usize, value: u32) {
        if ![1, 2, 4].contains(&width)
            || !offset.is_multiple_of(width)
            || offset.checked_add(width).is_none_or(|end| end > self.size)
        {
            return;
        }
        // SAFETY: same bounded width-correct device window as read.
        unsafe {
            match width {
                1 => ((self.base + offset) as *mut u8).write_volatile(value as u8),
                2 => ((self.base + offset) as *mut u16).write_volatile(value as u16),
                _ => ((self.base + offset) as *mut u32).write_volatile(value),
            }
        }
    }
    fn delay_us(&mut self, micros: u32) {
        axhal::time::busy_wait(core::time::Duration::from_micros(u64::from(micros)));
    }
    fn now_ns(&self) -> u64 {
        axhal::time::monotonic_time_nanos()
    }
}
type Sound = Controller<PlatformHal, Window>;
static DEVICE: Mutex<Option<Sound>> = Mutex::new(None);
pub struct HdaDriver;
impl DriverProbe for HdaDriver {
    fn probe_pci(
        root: &mut PciRoot,
        bdf: DeviceFunction,
        info: &DeviceFunctionInfo,
    ) -> BusProbeResult {
        if !axdriver_hda::ids::matches(info.vendor_id, info.class, info.subclass, info.prog_if) {
            return BusProbeResult::NotMatched;
        }
        let mut slot = DEVICE.lock();
        if slot.is_some() {
            return BusProbeResult::Claimed;
        }
        let Ok(BarInfo::Memory { address, size, .. }) = root.bar_info(bdf, 0) else {
            return BusProbeResult::Claimed;
        };
        if address == 0 || size < 0x400 {
            return BusProbeResult::Claimed;
        }
        let Ok(base) = axklib::mem::iomap((address as usize).into(), size as usize) else {
            return BusProbeResult::Claimed;
        };
        match Sound::new(Window {
            base: base.as_usize(),
            size: size as usize,
        }) {
            Ok(device) => {
                let route = device.route();
                log::info!(
                    "hda: {bdf} codec={:08x} address={} analog route {:?}; S16LE stereo 48000 Hz; \
                     未在硬件上验证",
                    route.vendor,
                    route.codec,
                    route
                        .path
                        .iter()
                        .map(|w| w.node)
                        .collect::<alloc::vec::Vec<_>>()
                );
                *slot = Some(device);
            }
            Err(error) => log::warn!(
                "hda: {bdf} initialization stopped: {error:?}; no sound endpoint registered"
            ),
        }
        BusProbeResult::Claimed
    }
}
pub fn available() -> bool {
    DEVICE.lock().is_some()
}
fn with_device<T>(f: impl FnOnce(&mut Sound) -> DevResult<T>) -> DevResult<T> {
    f(DEVICE.lock().as_mut().ok_or(DevError::Unsupported)?)
}
pub fn prepare(period: u32, periods: u32) -> DevResult {
    with_device(|device| {
        let config = Playback::configurations(device)
            .iter()
            .copied()
            .find(|config| {
                config
                    .period_bytes()
                    .and_then(|bytes| u32::try_from(bytes).ok())
                    == Some(period)
                    && u32::from(config.period_count) == periods
            })
            .ok_or(DevError::InvalidParam)?;
        Playback::prepare(device, config).map_err(playback_error)
    })
}
pub fn submit(bytes: &[u8]) -> DevResult<u16> {
    with_device(|device| {
        Playback::submit(device, bytes)
            .map(|PlaybackToken(token)| token)
            .map_err(playback_error)
    })
}
pub fn complete() -> DevResult<Option<u16>> {
    with_device(|device| {
        Playback::complete(device)
            .map(|token| token.map(|PlaybackToken(token)| token))
            .map_err(playback_error)
    })
}
pub fn release() -> DevResult {
    with_device(|device| Playback::release(device).map_err(playback_error))
}

pub fn abort() -> DevResult {
    with_device(|device| Playback::abort(device).map_err(playback_error))
}

/// Permanently stop the HDA controller. It releases DMA allocations only when
/// all engines acknowledge stop and the controller is held in reset.
pub fn shutdown() -> DevResult {
    with_device(|device| Playback::shutdown(device).map_err(playback_error))
}

/// Deliver/unpublish the active Intel display sink ELD and select its matching
/// digital codec route. Display lifetime code must call this only after a
/// powered, stable DDI link is active; link teardown calls it before power-down.
pub fn set_display_eld(port: u8, eld: Option<&[u8]>) -> DevResult {
    with_device(|device| device.set_display_eld(port, eld))
}

fn playback_error(error: PlaybackError) -> DevError {
    match error {
        PlaybackError::Unsupported => DevError::Unsupported,
        PlaybackError::InvalidPeriod => DevError::InvalidParam,
        PlaybackError::BadState => DevError::BadState,
        PlaybackError::Busy => DevError::ResourceBusy,
        PlaybackError::Again => DevError::Again,
        PlaybackError::Device => DevError::Io,
    }
}
