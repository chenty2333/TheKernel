//! i915 display microcontroller firmware acquisition.
//!
//! Source: Linux v7.2.3 `display/intel_dmc.c` firmware naming and deferred
//! request order. The firmware bytes are retained for the later parser/load
//! stages; this module does not yet parse the DMC package or write MMIO.

use alloc::vec::Vec;
use axdriver::prelude::firmware;
use spin::Mutex;
use intel_display::dmc::{self as dmc_map, DmcPlatform};

static PLATFORM: Mutex<Option<DmcPlatform>> = Mutex::new(None);
static FIRMWARE: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Register the display-13 DMC request for the identified ADL-P/N device.
/// The source driver defers firmware access until it can use the rootfs.
pub(super) fn request_for_device(device: &'static super::id::DisplayDevice) {
    let Ok(device) = intel_display::device::Device::identify(0x8086, device.device_id, 0)
    else {
        return;
    };
    let platform = match device.platform {
        intel_display::device::Platform::AlderLakeP => DmcPlatform::AlderLakeP,
        intel_display::device::Platform::AlderLakeN => DmcPlatform::AlderLakeN,
    };
    *PLATFORM.lock() = Some(platform);
    if !firmware::on_rootfs_ready(load_after_rootfs) {
        axlog::warn!("intel-dmc: rootfs-ready callback table full; firmware not requested");
    }
}

/// Firmware is requested only in rootfs-ready context, then retained for the
/// parser/loader. If the preferred ADL-P/N name is absent, try i915's fallback.
// upstream: intel_dmc.c dmc_load_work_fn()
fn load_after_rootfs() {
    let Some(platform) = *PLATFORM.lock() else {
        return;
    };
    let files = dmc_map::firmware_files(platform);
    let firmware = firmware::request(files.preferred, files.max_size).or_else(|| {
        files
            .fallback
            .and_then(|path| firmware::request(path, files.max_size))
    });
    let Some(firmware) = firmware else {
        axlog::warn!("intel-dmc: no firmware image for {platform:?}");
        return;
    };
    axlog::info!(
        "intel-dmc: retained {} bytes for {platform:?}; package validation/load pending",
        firmware.len()
    );
    *FIRMWARE.lock() = Some(firmware);
}

/// Take the retained firmware image when the DMC parser/loader is ready.
pub(super) fn take_firmware() -> Option<Vec<u8>> {
    FIRMWARE.lock().take()
}
