//! i915 display microcontroller firmware acquisition.
//!
//! Source: Linux v7.2.3 `display/intel_dmc.c` firmware naming, deferred
//! request order, and main-program package parsing. The validated DMC image
//! is retained for the MMIO load stage.

use axdriver::prelude::firmware;
use intel_display::dmc::{self as dmc_map, DmcPlatform};
use spin::Mutex;

use super::regs::{Meaning, Register, RegisterWindow, Registers};

static PLATFORM: Mutex<Option<DmcPlatform>> = Mutex::new(None);
static STEPPING: Mutex<Option<(u8, u8)>> = Mutex::new(None);
static PROGRAM: Mutex<Option<intel_display::dmc::DmcFirmware>> = Mutex::new(None);
static POWER_READY: Mutex<Option<(RegisterWindow, DmcPlatform)>> = Mutex::new(None);
static LOAD_LOCK: Mutex<()> = Mutex::new(());
static LOAD_RESULT: Mutex<
    Option<Result<intel_display::dmc::DmcLoadReport, intel_display::dmc::DmcLoadError>>,
> = Mutex::new(None);

struct DmcMmio<'a>(&'a RegisterWindow);

impl intel_display::RegisterIo for DmcMmio<'_> {
    fn read32(&self, offset: u32) -> Result<u32, intel_display::Error> {
        if !dmc_map::dmc_mmio_offset_valid(offset) {
            return Err(intel_display::Error::Unavailable(offset));
        }
        self.0
            .read(Register::read_only(
                "INTEL_DMC",
                offset,
                Meaning::BringUp,
                None,
            ))
            .ok_or(intel_display::Error::Unavailable(offset))
    }

    fn write32(&self, offset: u32, value: u32) -> Result<(), intel_display::Error> {
        if !dmc_map::dmc_mmio_offset_valid(offset)
            || !self.0.write(
                Register::read_write("INTEL_DMC", offset, Meaning::BringUp, None),
                value,
            )
        {
            return Err(intel_display::Error::Unavailable(offset));
        }
        Ok(())
    }
}

fn try_load_program()
-> Option<Result<intel_display::dmc::DmcLoadReport, intel_display::dmc::DmcLoadError>> {
    let _guard = LOAD_LOCK.lock();
    if let Some(result) = *LOAD_RESULT.lock() {
        return Some(result);
    }
    let (window, platform) = (*POWER_READY.lock())?;
    let firmware = PROGRAM.lock().clone()?;
    let result = dmc_map::load_program(&firmware, platform, &DmcMmio(&window));
    *LOAD_RESULT.lock() = Some(result);
    Some(result)
}

/// Record the display-power-ready boundary. Firmware upload is only allowed
/// after the opt-in hardware path has enabled the parent display power well.
pub(super) fn display_power_ready(window: RegisterWindow, platform: DmcPlatform) {
    *POWER_READY.lock() = Some((window, platform));
    if let Some(result) = try_load_program() {
        match result {
            Ok(report) => axlog::info!(
                "intel-dmc: uploaded {} programs, {} payload dwords, {} init MMIO writes",
                report.programs,
                report.payload_dwords,
                report.mmio_writes,
            ),
            Err(error) => axlog::warn!("intel-dmc: firmware upload/readback failed: {error:?}"),
        }
    }
}

/// Register the display-13 DMC request for the identified ADL-P/N device.
/// The source driver defers firmware access until it can use the rootfs.
pub(super) fn request_for_device(device: &'static super::id::DisplayDevice, revision: u8) {
    let Ok(device) = intel_display::device::Device::identify(0x8086, device.device_id, revision)
    else {
        return;
    };
    let (step, substep) = if device.exact_step {
        let step = match device.step {
            intel_display::device::Step::A0 => b'A',
            intel_display::device::Step::B0 => b'B',
            intel_display::device::Step::C0 => b'C',
            intel_display::device::Step::D0 => b'D',
            intel_display::device::Step::Future => b'*',
        };
        (step, b'0')
    } else {
        (b'*', b'*')
    };
    let platform = match device.platform {
        intel_display::device::Platform::AlderLakeP => DmcPlatform::AlderLakeP,
        intel_display::device::Platform::AlderLakeN => DmcPlatform::AlderLakeN,
    };
    *PLATFORM.lock() = Some(platform);
    *STEPPING.lock() = Some((step, substep));
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
    let (step, substep) = (*STEPPING.lock()).unwrap_or((b'*', b'*'));
    let program = match dmc_map::parse_firmware(&firmware, platform, step, substep) {
        Ok(program) => program,
        Err(error) => {
            axlog::warn!("intel-dmc: firmware package rejected for {platform:?}: {error:?}");
            return;
        }
    };
    axlog::info!(
        "intel-dmc: parsed {} DMC programs for {platform:?} (version {:#010x})",
        program.programs.len(),
        program.version,
    );
    *PROGRAM.lock() = Some(program);
    if let Some(Err(error)) = try_load_program() {
        axlog::warn!("intel-dmc: firmware upload/readback failed: {error:?}");
    }
}
