//! One VirtIO playback stream. Mixing and conversion belong to userspace.

use axdriver_base::{DevError, DevResult};
use axdriver_virtio::{PcmFeatures, PcmFormat, PcmFormats, PcmRate, PcmRates, VirtIOSound};
use spin::Mutex;

use crate::{
    drivers::{BusProbeResult, DriverProbe},
    virtio::VirtIoHalImpl,
};

type Sound = VirtIOSound<VirtIoHalImpl, axdriver_virtio::PciTransport>;
static DEVICE: Mutex<Option<Sound>> = Mutex::new(None);
static STREAM: Mutex<Option<u32>> = Mutex::new(None);

pub struct VirtIoSoundDriver;

impl DriverProbe for VirtIoSoundDriver {
    #[cfg(bus = "pci")]
    fn probe_pci(
        root: &mut axdriver_pci::PciRoot,
        bdf: axdriver_pci::DeviceFunction,
        info: &axdriver_pci::DeviceFunctionInfo,
    ) -> BusProbeResult {
        let Some(transport) =
            axdriver_virtio::probe_pci_sound_device::<VirtIoHalImpl>(root, bdf, info)
        else {
            return BusProbeResult::NotMatched;
        };
        let mut slot = DEVICE.lock();
        if slot.is_some() {
            return BusProbeResult::Claimed;
        }
        match Sound::new(transport) {
            Ok(mut device) => {
                device.set_control_timeout(axhal::time::monotonic_time_nanos, 250_000_000);
                let stream = device.output_streams().ok().and_then(|streams| {
                    streams.into_iter().find(|&id| {
                        device
                            .formats_supported(id)
                            .is_ok_and(|f| f.contains(PcmFormats::S16))
                            && device
                                .rates_supported(id)
                                .is_ok_and(|r| r.contains(PcmRates::RATE_48000))
                            && device
                                .channel_range_supported(id)
                                .is_ok_and(|r| r.contains(&2))
                    })
                });
                device.ack_interrupt();
                *STREAM.lock() = stream;
                // Keep every published DMA queue alive, including unsupported devices.
                *slot = Some(device);
                if stream.is_some() {
                    info!("registered VirtIO sound playback: stereo S16LE 48000 Hz");
                } else {
                    warn!("VirtIO sound has no supported playback stream");
                }
            }
            Err(error) => warn!("VirtIO sound initialization failed: {error:?}"),
        }
        BusProbeResult::Claimed
    }
}

pub fn available() -> bool {
    STREAM.lock().is_some()
}

fn with_device<T>(
    f: impl FnOnce(&mut Sound, u32) -> axdriver_virtio::VirtIoResult<T>,
) -> DevResult<T> {
    let mut device = DEVICE.lock();
    let stream = STREAM.lock().ok_or(DevError::Unsupported)?;
    let device = device.as_mut().ok_or(DevError::Unsupported)?;
    let result = f(device, stream).map_err(|e| match e {
        axdriver_virtio::VirtIoError::QueueFull | axdriver_virtio::VirtIoError::NotReady => {
            DevError::Again
        }
        axdriver_virtio::VirtIoError::InvalidParam => DevError::InvalidParam,
        _ => DevError::Io,
    });
    device.ack_interrupt();
    result
}

pub fn prepare(period: u32, periods: u32) -> DevResult {
    with_device(|d, id| {
        d.pcm_set_params(
            id,
            period * periods,
            period,
            PcmFeatures::empty(),
            2,
            PcmFormat::S16,
            PcmRate::Rate48000,
        )?;
        d.pcm_prepare(id)?;
        d.pcm_start(id)
    })
}

pub fn submit(bytes: &[u8]) -> DevResult<u16> {
    with_device(|d, id| d.pcm_xfer_nb(id, bytes))
}

/// Retires a completed DMA period and checks the device's completion status.
pub fn complete() -> DevResult<Option<u16>> {
    with_device(|d, _| match d.pcm_completed() {
        Some(token) => {
            d.pcm_xfer_ok(token)?;
            Ok(Some(token))
        }
        None => Ok(None),
    })
}

/// Call only after every submitted period has been retired.
pub fn release() -> DevResult {
    with_device(|d, id| {
        d.pcm_stop(id)?;
        d.pcm_release(id)
    })
}
