//! N305 firmware-preserving KMS: no clock/link/timing/WM programming.
//! Uses the MIT i915 readouts in tk-intel-display; adapter/ownership policy is
//! original TheKernel code. Only pipe-A TC1/TC2 legacy HDMI, opaque linear XR24,
//! no scaling/color/DSC/VRR is admitted. Hardware writes remain opt-in.
use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{Ordering, fence};

use intel_display::{
    Error, RegisterIo,
    color::ColorIo,
    device::Port,
    display::{Pipe, ReadoutIo},
    dkl_phy::{DklIo, TcPort},
    dpll_mgr::PllReadoutIo,
    scaler::ScalerIo,
    tc::TcIo,
};
use spin::Mutex;

use super::{
    gmbus::PollTimer,
    gtt::{Binding, Gtt},
    regs::{Meaning, Register, Registers},
};
use crate::{
    drm::{
        DisplayAdapter, DrmError, DrmResult, DumbRequest, GemBacking, Mode, Scanout, fence::Fence,
    },
    mm::{SharedFixedView, SharedPages},
};

static HIP_LOCK: Mutex<()> = Mutex::new(());
fn reg(offset: u32, writable: bool) -> Register {
    if writable {
        Register::read_write("N305_FASTBOOT", offset, Meaning::BringUp, None)
    } else {
        Register::read_only("N305_FASTBOOT", offset, Meaning::BringUp, None)
    }
}
fn read(r: &impl Registers, offset: u32) -> Result<u32, Error> {
    r.read(reg(offset, false))
        .filter(|v| *v != u32::MAX)
        .ok_or(Error::Unavailable(offset))
}
fn write(r: &impl Registers, offset: u32, value: u32) -> Result<(), Error> {
    if r.write(reg(offset, true), value) {
        Ok(())
    } else {
        Err(Error::Unavailable(offset))
    }
}
fn message(e: Error) -> String {
    format!("N305 fastboot refused/recovered: {e:?}")
}
/// A reference here means driver requests are actually held, not sampled-on bits.
struct PinnedIo<'a, R> {
    registers: &'a R,
    port: TcPort,
}
impl<R: Registers> RegisterIo for PinnedIo<'_, R> {
    fn read32(&self, offset: u32) -> Result<u32, Error> {
        read(self.registers, offset)
    }
    fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
        // Discovery only mutates/restores the shared HIP selector, never PHY.
        if offset != 0x1010a0 {
            return Err(Error::Refused);
        }
        write(self.registers, offset, value)
    }
}
impl<R: Registers> ReadoutIo for PinnedIo<'_, R> {
    fn pipe_powered(&self, p: Pipe) -> bool {
        p == Pipe::A
    }
}
impl<R: Registers> ScalerIo for PinnedIo<'_, R> {
    fn scalers_powered(&self, p: Pipe) -> bool {
        p == Pipe::A
    }
}
impl<R: Registers> DklIo for PinnedIo<'_, R> {
    fn with_dkl_lock<T>(&self, op: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        let _lock = HIP_LOCK.lock();
        op()
    }
}
impl<R: Registers> ColorIo for PinnedIo<'_, R> {
    fn with_color_lock<T>(&self, op: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        op()
    }
}
impl<R: Registers> PllReadoutIo for PinnedIo<'_, R> {
    fn with_display_core_if_enabled<T>(
        &self,
        op: impl FnOnce() -> Result<T, Error>,
    ) -> Result<Option<T>, Error> {
        op().map(Some)
    }
}
impl<R: Registers> TcIo for PinnedIo<'_, R> {
    fn display_core_powered(&self) -> bool {
        true
    }
    fn tc_port_powered(&self, p: TcPort) -> bool {
        p == self.port
    }
    fn tc_cold_blocked(&self, p: TcPort) -> bool {
        p == self.port
    }
}
#[derive(Clone, Copy)]
struct PowerPin {
    offsets: [u32; 3],
    before: [u32; 3],
    masks: [u32; 3],
}
impl PowerPin {
    fn acquire(r: &impl Registers, port: TcPort) -> Result<Self, Error> {
        // i915 XELPD power map: PW1, PW2 and PWA; DDI_IO and legacy AUX.
        // AUX is the ADL-P legacy TC-cold blocker. No cold-exit/enable sequence
        // is attempted: every required well must already be live before writes.
        if port.index() > 1 || read(r, 0x45504)? & super::power::DC_STATE_MASK != 0 {
            return Err(Error::Refused);
        }
        let offsets = [0x45404, 0x45454, 0x45444];
        let masks = [
            2 | (2 << 2) | (2 << 10),
            2 << ((3 + port.index()) * 2),
            2 << ((3 + port.index()) * 2),
        ];
        let mut before = [0; 3];
        for i in 0..3 {
            before[i] = read(r, offsets[i])?;
            if before[i] & (masks[i] >> 1) != masks[i] >> 1 {
                return Err(Error::Refused);
            }
        }
        let pin = Self {
            offsets,
            before,
            masks,
        };
        for i in 0..3 {
            // Never echo RO state bits; preserve all existing requestors.
            let result = write(r, offsets[i], (before[i] & 0xaaaa_aaaa) | masks[i])
                .and_then(|()| pin.verify(r));
            if let Err(e) = result {
                pin.restore(r)?;
                return Err(e);
            }
        }
        Ok(pin)
    }
    fn verify(&self, r: &impl Registers) -> Result<(), Error> {
        // During acquisition some driver requests have not yet been installed;
        // state must stay live. Persistent capture checks requests separately.
        for i in 0..3 {
            if read(r, self.offsets[i])? & (self.masks[i] >> 1) != self.masks[i] >> 1 {
                return Err(Error::Refused);
            }
        }
        Ok(())
    }
    fn held(&self, r: &impl Registers) -> Result<(), Error> {
        for i in 0..3 {
            if read(r, self.offsets[i])? & (self.masks[i] | (self.masks[i] >> 1))
                != self.masks[i] | (self.masks[i] >> 1)
            {
                return Err(Error::Refused);
            }
        }
        Ok(())
    }
    fn restore(&self, r: &impl Registers) -> Result<(), Error> {
        for i in (0..3).rev() {
            let current =
                read(r, self.offsets[i]).map_err(|_| Error::RestoreFailed(self.offsets[i]))?;
            let payload =
                ((current & !self.masks[i]) | (self.before[i] & self.masks[i])) & 0xaaaa_aaaa;
            write(r, self.offsets[i], payload)
                .map_err(|_| Error::RestoreFailed(self.offsets[i]))?;
            if read(r, self.offsets[i])
                .ok()
                .map(|v| v & (self.masks[i] | (self.masks[i] >> 1)))
                != Some(self.before[i] & (self.masks[i] | (self.masks[i] >> 1)))
            {
                return Err(Error::RestoreFailed(self.offsets[i]));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct Firmware {
    pipe: intel_display::pipe_config::PipeConfig,
    plane: intel_display::universal_plane::InitialPlaneConfig,
    ddi: intel_display::ddi::DdiFunction,
    pll: intel_display::dpll_mgr::DklPllReadout,
    phy: intel_display::tc::DklPhyState,
    fia: intel_display::tc::FiaState,
    clock: intel_display::ddi::TcClockState,
    hdmi: intel_display::hdmi::HdmiReadout,
    wm: [intel_display::watermark::PlaneWatermarks; 6],
    ddb: [intel_display::watermark::DdbEntry; 6],
    dbuf: intel_display::watermark::DbufState,
    color: intel_display::color::ColorConfig,
    scalers: [intel_display::scaler::Scaler; 2],
    refclk: u32,
    pixel_clock: u32,
}
fn capture(r: &impl Registers, pin: &PowerPin, port: TcPort) -> Result<Firmware, Error> {
    use intel_display::{
        color, ddi, dpll_mgr, hdmi, pipe_config, scaler, tc, universal_plane, watermark,
    };
    pin.held(r)?;
    let io = PinnedIo { registers: r, port };
    let pipe = pipe_config::read_pipe_config(&io, Pipe::A)?.ok_or(Error::Refused)?;
    pipe.timings.validate()?;
    if pipe.transconf & (3 << 30) != 3 << 30
        || pipe.transconf & (3 << 21) != 0
        || !pipe.dss.uncompressed()
        || pipe.vrr.enabled
        || pipe.vrr.raw_control & ((1 << 29) | (1 << 27) | (1 << 28)) != 0
        || pipe.misc.output != pipe_config::OutputFormat::Rgb
        || pipe.misc.bpc != Some(8)
        || pipe.misc.raw & ((1 << 23) | (1 << 4)) != 0
        || pipe.pixel_multiplier != 1
        || pipe.source != (pipe.timings.hdisplay, pipe.timings.vdisplay)
    {
        return Err(Error::Refused);
    }
    // No secondary running pipe, hidden sprite/cursor or compression owner.
    let power = read(r, 0x45404)?;
    for i in 1..4 {
        if power & (1 << ((5 + i) * 2)) != 0 && read(r, 0x70008 + i * 0x1000)? & (3 << 30) != 0 {
            return Err(Error::Refused);
        }
    }
    for i in 1..5 {
        if read(r, 0x70180 + i * 0x100)? & (1 << 31) != 0 {
            return Err(Error::Refused);
        }
    }
    if read(r, 0x70080)? & 0x3f != 0 {
        return Err(Error::Refused);
    }
    let plane = universal_plane::skl_get_initial_plane_config(
        &io,
        Pipe::A,
        universal_plane::Plane::PRIMARY,
    )?
    .ok_or(Error::Refused)?;
    if !plane.native_linear_xrgb()
        || plane.offset != 0
        || (plane.width, plane.height) != pipe.source
        || read(r, 0x7018c)? != 0
        || read(r, 0x701ac)? & 0xffff_f000 != plane.surface_raw & 0xffff_f000
    {
        return Err(Error::Refused);
    }
    let ddi = ddi::read_function_control(&io, Pipe::A)?;
    let expected = if port == TcPort::Tc1 {
        Port::Tc1
    } else {
        Port::Tc2
    };
    if !ddi.enabled
        || ddi.port != Some(expected)
        || ddi.mode != ddi::DdiMode::Hdmi
        || ddi.bpp != Some(24)
        || ddi.port_sync
        || ddi.hdmi_scrambling
        || ddi.high_tmds_ratio
    {
        return Err(Error::Refused);
    }
    if !tc::adlp_tc_phy_is_ready(&io, port)?
        || !tc::adlp_tc_phy_is_owned(&io, port)?
        || read(r, tc::buffer_register(port))? & (1 << 31) == 0
    {
        return Err(Error::Refused);
    }
    let fia = tc::read_fia_state(&io, port)?;
    let clock = ddi::read_tc_clock_state(&io, port)?;
    if !clock.enabled || clock.pll != ddi::TcPllKind::Dkl {
        return Err(Error::Refused);
    }
    let refclk = match read(r, 0x51004)? >> 29 {
        0 => 24000,
        1 => 19200,
        2 => 38400,
        _ => return Err(Error::Refused),
    };
    let pll = dpll_mgr::dkl_pll_get_hw_state(&io, port, refclk, false)?.ok_or(Error::Refused)?;
    if pll.enable & ((1 << 31) | (1 << 30) | (1 << 27) | (1 << 26))
        != (1 << 31) | (1 << 30) | (1 << 27) | (1 << 26)
    {
        return Err(Error::Refused);
    }
    let pixel_clock = dpll_mgr::icl_ddi_mg_pll_get_freq(&pll.state, refclk)?;
    if !(25000..=300000).contains(&pixel_clock) {
        return Err(Error::Refused);
    }
    let phy = tc::read_dkl_phy_state(&io, port)?;
    if phy.uc_dw27 & (1 << 15) == 0 {
        return Err(Error::Refused);
    }
    let hdmi = hdmi::read_hdmi_state(&io, Pipe::A)?;
    // Preserve normal AVI/SPD/GCP only; HDR, 3D, VSC/AS/PPS are not this path.
    if hdmi.control & ((1 << 28) | (1 << 24) | (1 << 23) | (1 << 20) | (1 << 8) | (1 << 4)) != 0 {
        return Err(Error::Refused);
    }
    for packet in hdmi.frames.iter().flatten() {
        if let intel_display::hdmi_packet::Infoframe::Avi(avi) = packet.unpack()?
            && (avi.colorspace != 0 || avi.pixel_repeat != 0)
        {
            return Err(Error::Refused);
        }
    }
    let color = color::intel_color_get_config(&io, Pipe::A, false, &mut [], &mut [])?;
    if !color.bypassed() {
        return Err(Error::Refused);
    }
    let scalers = scaler::read_all_scalers(&io, Pipe::A)?.ok_or(Error::Refused)?;
    if scalers.iter().any(|s| s.enabled()) {
        return Err(Error::Refused);
    }
    let wm = watermark::skl_pipe_wm_get_hw_state(&io, Pipe::A)?;
    let ddb = watermark::skl_pipe_ddb_get_hw_state(&io, Pipe::A)?;
    let dbuf = watermark::read_dbuf_state(&io)?;
    if dbuf
        .ctl
        .iter()
        .any(|v| v & (1 << 30) != 0 && v & (1 << 31) == 0)
    {
        return Err(Error::Refused);
    }
    if dbuf.enabled_slices == 0 || !wm[0].levels[0].enable || ddb[0].blocks() == 0 {
        return Err(Error::Refused);
    }
    pin.held(r)?;
    Ok(Firmware {
        pipe,
        plane,
        ddi,
        pll,
        phy,
        fia,
        clock,
        hdmi,
        wm,
        ddb,
        dbuf,
        color,
        scalers,
        refclk,
        pixel_clock,
    })
}

/// Firmware PTE pages must be stolen memory outside allocator RAM. The CPU
/// boot aperture must denote this same GGTT extent, never an unrelated GOP.
fn ownership(
    gtt: &Gtt,
    f: &Firmware,
    boot: &axhal::boot::BootFramebuffer,
    aperture: u64,
    stolen: core::ops::Range<u64>,
    allocatable: &[(u64, u64)],
) -> Result<(), Error> {
    let address = u64::from(f.plane.surface_raw & 0xffff_f000);
    if boot.address != aperture.checked_add(address).ok_or(Error::Refused)?
        || boot.width != f.plane.width
        || boot.height != f.plane.height
        || boot.pitch != f.plane.pitch
        || boot.bpp != 32
        || boot.red.position != 16
        || boot.red.size != 8
        || boot.green.position != 8
        || boot.green.size != 8
        || boot.blue.position != 0
        || boot.blue.size != 8
        || address == 0
        || f.plane.main_size == 0
        || boot.byte_len().map(|n| n as u64) != Some(f.plane.main_size)
    {
        return Err(Error::Refused);
    }
    for i in 0..f.plane.main_size.div_ceil(4096) {
        let pte = gtt.entry(address + i * 4096).map_err(|_| Error::Refused)?;
        let p = pte.address();
        let end = p.checked_add(4096).ok_or(Error::Refused)?;
        if !pte.is_present()
            || pte.is_local_memory()
            || pte.raw() & !(super::gtt::PTE_ADDRESS_MASK | 1) != 0
            || p < stolen.start
            || end > stolen.end
            || allocatable
                .iter()
                .any(|&(base, size)| p < base.saturating_add(size) && end > base)
        {
            return Err(Error::Refused);
        }
    }
    Ok(())
}
fn mode(f: &Firmware) -> Result<Mode, Error> {
    let t = f.pipe.timings;
    let hz = u64::from(f.pixel_clock) * 1_000_000 / (u64::from(t.htotal) * u64::from(t.vtotal));
    if !(25_000..=240_000).contains(&hz) {
        return Err(Error::Refused);
    }
    Ok(Mode {
        width: t.hdisplay,
        height: t.vdisplay,
        refresh_millihz: hz.try_into().map_err(|_| Error::Refused)?,
    })
}
struct RamBacking {
    pages: Arc<SharedPages>,
}
impl GemBacking for RamBacking {
    fn shared_pages(&self) -> DrmResult<Arc<SharedPages>> {
        Ok(self.pages.clone())
    }
}
struct Bound {
    pages: Arc<SharedPages>,
    _pin: SharedFixedView,
    binding: Binding,
}
struct State {
    current: Option<Bound>,
    quarantine: Vec<Bound>,
    unbound_quarantine: Vec<(Arc<SharedPages>, SharedFixedView)>,
    lost: bool,
    frame_progress: Option<(u32, u64)>,
}
struct Native<R, T> {
    registers: R,
    timer: T,
    gtt: Arc<Gtt>,
    power: PowerPin,
    firmware: Firmware,
    port: TcPort,
    mode: Mode,
    pci: axdriver_display::DisplayPciIdentity,
    state: Mutex<State>,
}
fn frame_count(r: &impl Registers) -> Result<u32, Error> {
    // All-ones is a valid frame just before wrap, unlike configuration words.
    r.read(reg(0x70040, false))
        .ok_or(Error::Unavailable(0x70040))
}
fn latch(r: &impl Registers, timer: &impl PollTimer, address: u32) -> Result<(), Error> {
    let initial = frame_count(r)?;
    let start = timer.now_micros();
    write(r, 0x7019c, address)?;
    if read(r, 0x7019c)? != address {
        return Err(Error::Refused);
    }
    // A store/posted read is not completion. Observe a hardware frame AFTER
    // SURFLIVE matches, then another fresh frame; old DMA may retire only then.
    let mut live_frame = None;
    for _ in 0..1_000_000 {
        let frame = frame_count(r)?;
        if read(r, 0x701ac)? & 0xffff_f000 == address & 0xffff_f000 && frame != initial {
            if let Some(seen) = live_frame {
                if frame != seen {
                    fence(Ordering::SeqCst);
                    return Ok(());
                }
            } else {
                live_frame = Some(frame);
            }
        }
        if timer.now_micros().saturating_sub(start) > 150_000 {
            break;
        }
        timer.pause();
    }
    Err(Error::Refused)
}
impl<R: Registers + Send + Sync, T: PollTimer + Send + Sync> DisplayAdapter for Native<R, T> {
    fn driver_name(&self) -> &'static str {
        "thekernel_intel"
    }
    fn platform_name(&self) -> Option<&'static str> {
        Some("n305-native-fastboot")
    }
    fn preferred_mode(&self) -> Mode {
        self.mode
    }
    fn fixed_mode(&self) -> Option<Mode> {
        Some(self.mode)
    }
    fn supports_cursor(&self) -> bool {
        false
    }
    fn pci_identity(&self) -> Option<axdriver_display::DisplayPciIdentity> {
        Some(self.pci)
    }
    fn hardware_vblank_counter(&self) -> DrmResult<Option<u32>> {
        let mut state = self.state.lock();
        if state.lost {
            return Err(DrmError::DeviceLost);
        }
        if self.power.held(&self.registers).is_err() {
            state.lost = true;
            return Err(DrmError::DeviceLost);
        }
        let counter = match frame_count(&self.registers) {
            Ok(v) => v,
            Err(_) => {
                state.lost = true;
                return Err(DrmError::DeviceLost);
            }
        };
        let now = self.timer.now_micros();
        match state.frame_progress {
            Some((previous, since)) if previous == counter => {
                if now.saturating_sub(since) > 150_000 {
                    state.lost = true;
                    return Err(DrmError::DeviceLost);
                }
            }
            _ => state.frame_progress = Some((counter, now)),
        }
        Ok(Some(counter))
    }
    fn validate_atomic_state(&self, active: bool, dpms_on: bool, gamma_lut: bool) -> DrmResult<()> {
        if active && dpms_on && !gamma_lut {
            Ok(())
        } else {
            Err(DrmError::Unsupported)
        }
    }
    fn create_dumb(
        &self,
        request: DumbRequest,
        pitch: u32,
        size: u64,
        owner: Arc<dyn Send + Sync>,
    ) -> DrmResult<Arc<dyn GemBacking>> {
        if request.bpp != 32
            || request.width != self.mode.width
            || pitch != self.firmware.plane.pitch
            || request.height < self.mode.height
            || request.height > self.mode.height.saturating_mul(2)
            || size != u64::from(pitch) * u64::from(request.height)
        {
            return Err(DrmError::Unsupported);
        }
        let aligned = usize::try_from(size)
            .map_err(|_| DrmError::Overflow)?
            .checked_add(4095)
            .ok_or(DrmError::Overflow)?
            & !4095;
        let pages = SharedPages::new_fixed(aligned, axhal::paging::PageSize::Size4K)
            .map_err(|_| DrmError::NoMemory)?;
        pages
            .retain_allocation_owner(owner)
            .map_err(|_| DrmError::NoMemory)?;
        let pages = Arc::try_new(pages).map_err(|_| DrmError::NoMemory)?;
        Arc::try_new(RamBacking { pages })
            .map(|b| b as Arc<dyn GemBacking>)
            .map_err(|_| DrmError::NoMemory)
    }
    fn present(&self, s: Scanout) -> DrmResult<Arc<Fence>> {
        if s.mode != self.mode
            || s.width != self.mode.width
            || s.height != self.mode.height
            || s.framebuffer_width != s.width
            || s.pitch != self.firmware.plane.pitch
            || s.bpp != 32
            || s.format != intel_display::universal_plane::XRGB8888
            || s.framebuffer_offset != 0
            || !s.offset.is_multiple_of(4096)
            || s.offset != u64::from(s.source_y) * u64::from(s.pitch)
            || s.source_x != 0
            || s.framebuffer_height < s.height
        {
            return Err(DrmError::Unsupported);
        }
        let end = s
            .offset
            .checked_add(u64::from(s.pitch) * u64::from(s.height))
            .ok_or(DrmError::Overflow)?;
        let pages = s.backing.shared_pages()?;
        if end > s.backing_size
            || end > pages.total_bytes() as u64
            || !pages.is_fixed()
            || pages.is_external()
            || pages.page_size() != axhal::paging::PageSize::Size4K
        {
            return Err(DrmError::Invalid);
        }
        let mut state = self.state.lock();
        if state.lost {
            return Err(DrmError::DeviceLost);
        }
        // Allocate every recovery/quarantine slot before touching hardware.
        state
            .quarantine
            .try_reserve(2)
            .map_err(|_| DrmError::NoMemory)?;
        state
            .unbound_quarantine
            .try_reserve(1)
            .map_err(|_| DrmError::NoMemory)?;
        let complete = Fence::new(false);
        if self.power.held(&self.registers).is_err() {
            state.lost = true;
            complete.signal_error();
            return Err(DrmError::DeviceLost);
        }
        let mut observed = match capture(&self.registers, &self.power, self.port) {
            Ok(v) => v,
            Err(_) => {
                state.lost = true;
                complete.signal_error();
                return Err(DrmError::DeviceLost);
            }
        };
        observed.plane.surface_raw = self.firmware.plane.surface_raw;
        if observed != self.firmware {
            state.lost = true;
            complete.signal_error();
            return Err(DrmError::DeviceLost);
        }
        let before = match read(&self.registers, 0x7019c) {
            Ok(v) => v,
            Err(_) => {
                state.lost = true;
                complete.signal_error();
                return Err(DrmError::DeviceLost);
            }
        };
        let mut next = None;
        let address = if let Some(current) = &state.current
            && Arc::ptr_eq(&current.pages, &pages)
        {
            current.binding.address
        } else {
            let pin = pages.fixed_view().map_err(|_| DrmError::Invalid)?;
            let mut physical = Vec::new();
            physical
                .try_reserve_exact(pages.len())
                .map_err(|_| DrmError::NoMemory)?;
            for i in 0..pages.len() {
                let p = pages.paddr_at(i).map_err(|_| DrmError::Invalid)?.as_usize() as u64;
                if p >= 1 << 39 {
                    return Err(DrmError::Unsupported);
                }
                physical.push(p);
            }
            let binding = match self.gtt.bind_pages(&physical) {
                Ok(b) => b,
                Err(
                    super::gtt::GttError::CheckpointUnavailable
                    | super::gtt::GttError::ApertureExhausted { .. },
                ) => {
                    complete.signal_error();
                    return Err(DrmError::NoMemory);
                }
                Err(_) => {
                    state.unbound_quarantine.push((pages, pin));
                    state.lost = true;
                    complete.signal_error();
                    return Err(DrmError::DeviceLost);
                }
            };
            let address = binding.address;
            next = Some(Bound {
                pages,
                _pin: pin,
                binding,
            });
            address
        };
        fence(Ordering::SeqCst); // publish CPU pixels/PTEs before plane arm.
        let surface = (address + s.offset) as u32;
        if latch(&self.registers, &self.timer, surface).is_err() {
            let recovered = latch(&self.registers, &self.timer, before).is_ok();
            if let Some(new) = next {
                if recovered {
                    // SAFETY: restored surface progressed through fresh frames.
                    if unsafe { self.gtt.release_binding(&new.binding) }.is_err() {
                        state.quarantine.push(new);
                        state.lost = true;
                    }
                } else {
                    state.quarantine.push(new);
                }
            }
            if !recovered {
                state.lost = true;
            }
            complete.signal_error();
            return Err(if recovered && !state.lost {
                DrmError::Busy
            } else {
                DrmError::DeviceLost
            });
        }
        if let Some(new) = next
            && let Some(old) = state.current.replace(new)
        {
            // SAFETY: new SURFLIVE followed by another hardware frame.
            if unsafe { self.gtt.release_binding(&old.binding) }.is_err() {
                state.quarantine.push(old);
                state.lost = true;
                complete.signal_error();
                return Err(DrmError::DeviceLost);
            }
        }
        complete.signal();
        Ok(complete)
    }
}

#[cfg(target_os = "none")]
pub(super) fn init(
    bdf: super::pci::Bdf,
    window: super::regs::RegisterWindow,
    gtt: Arc<Gtt>,
) -> Result<String, String> {
    use super::{dma::firmware_bytes, pci::ConfigSpace};
    super::dma::require_direct(bdf).map_err(message)?;
    if axhal::boot::command_line_value("intel.modeset.fail_write").is_some() {
        return Err(String::from(
            "combo-only fail_write injection is not a TC fastboot command; refused before writes",
        ));
    }
    let setup = || -> Result<_, Error> {
        let ecam = super::pci::Ecam::platform().ok_or(Error::Refused)?;
        let info = super::pci::DeviceInfo::read(&ecam, bdf).ok_or(Error::Refused)?;
        if !super::i915_port::native_device_supported(info.vendor_id, info.device_id, info.revision)
        {
            return Err(Error::Refused);
        }
        let boot = axhal::boot::framebuffer().ok_or(Error::Refused)?;
        let aperture = match info.bars[2].kind {
            super::pci::BarKind::Memory { address, .. } if address != 0 => address,
            _ => return Err(Error::Refused),
        };
        let ctl = read(&window, 0x60400)?;
        let port = match intel_display::ddi::decode_function_control(ctl).port {
            Some(Port::Tc1) => TcPort::Tc1,
            Some(Port::Tc2) => TcPort::Tc2,
            _ => return Err(Error::Refused),
        };
        // Acquire the actual board route via read-only ASLS, not a captured
        // prior Linux state or hardcoded claim that every TC port is HDMI.
        let asls = ecam
            .read_u32(bdf, intel_display::opregion::ASLS)
            .ok_or(Error::Refused)?;
        let data = firmware_bytes(u64::from(asls), intel_display::opregion::SIZE)?;
        let op = intel_display::opregion::OpRegion::parse(&data, u64::from(asls))?;
        let external = op
            .external_vbt()?
            .map(|v| firmware_bytes(v.physical, v.size))
            .transpose()?;
        let vbt = op.vbt(external.as_deref())?;
        if !vbt.checksum_valid() {
            return Err(Error::InvalidHeader);
        }
        let definitions = vbt.parse_general_definitions()?;
        let route = definitions
            .encoder(if port == TcPort::Tc1 {
                Port::Tc1
            } else {
                Port::Tc2
            })?
            .ok_or(Error::Refused)?;
        if !route.supports_hdmi()
            || route.usb_type_c
            || route.thunderbolt
            || route.lspcon
            || route.dynamic_port_over_tc
            || route.hdmi_level_shift != 5
        {
            return Err(Error::Refused);
        }
        let preflight = read(&window, 0x45404)?;
        if preflight & ((1 << 0) | (1 << 2) | (1 << 10)) != (1 << 0) | (1 << 2) | (1 << 10)
            || read(&window, 0x70180)? & (1 << 31) == 0
            || read(&window, 0x701a4)? != 0
        {
            return Err(Error::Refused);
        }
        let stolen_base =
            u64::from(read(&window, 0x1080c0)?) | (u64::from(read(&window, 0x1080c4)?) << 32);
        let stolen_base = stolen_base & 0xffff_ffff_fff0_0000;
        let gmch = ecam.read_u32(bdf, 0x50).ok_or(Error::Refused)?;
        let code = (gmch >> 8) & 255;
        // Original decoder of documented Gen9+ GMS facts; no GPL source text.
        let mb = match code {
            1..=0xef => code * 32,
            0xf0..=0xfe => (code - 0xef) * 4,
            _ => return Err(Error::Refused),
        };
        let stolen = stolen_base
            ..stolen_base
                .checked_add(u64::from(mb) * 1024 * 1024)
                .ok_or(Error::Refused)?;
        if stolen.start == 0 {
            return Err(Error::Refused);
        }
        // Firmware PTEs must avoid usable RAM altogether, not merely free
        // allocator entries: kernel/module reservations are not stolen memory.
        let allocatable: Vec<_> = axhal::mem::phys_ram_ranges()
            .iter()
            .map(|&(base, size)| (base as u64, size as u64))
            .collect();
        let pin = PowerPin::acquire(&window, port)?;
        let admitted = (|| {
            let first = capture(&window, &pin, port)?;
            if first.plane.pitch != (first.plane.width * 4).div_ceil(64) * 64 {
                return Err(Error::Refused);
            }
            ownership(&gtt, &first, &boot, aperture, stolen, &allocatable)?;
            let second = capture(&window, &pin, port)?;
            if first != second {
                return Err(Error::Refused);
            }
            let mode = mode(&first)?;
            // Preserve the measured firmware mode, not an invented EDID target.
            // This milestone advertises ONLY this fixed mode to KMS/Weston.
            Ok((first, mode))
        })();
        match admitted {
            Ok((f, m)) => Ok((pin, port, f, m, info)),
            Err(e) => {
                pin.restore(&window)?;
                Err(e)
            }
        }
    };
    let (power, port, firmware, mode, info) = setup().map_err(message)?;
    let pci = axdriver_display::DisplayPciIdentity {
        bus: bdf.bus,
        device: bdf.device,
        function: bdf.function,
        vendor_id: info.vendor_id,
        device_id: info.device_id,
        revision: info.revision,
        subsystem_vendor: info.subsystem_vendor_id,
        subsystem_device: info.subsystem_id,
    };
    let adapter = Arc::try_new(Native {
        registers: window,
        timer: super::gmbus::MonotonicTimer,
        gtt,
        power,
        firmware,
        port,
        mode,
        pci,
        state: Mutex::new(State {
            current: None,
            quarantine: Vec::new(),
            unbound_quarantine: Vec::new(),
            lost: false,
            frame_progress: None,
        }),
    });
    let adapter = match adapter {
        Ok(a) => a,
        Err(_) => {
            power.restore(&window).map_err(message)?;
            return Err(String::from(
                "fastboot adapter allocation failed; power restored",
            ));
        }
    };
    let device = crate::drm::DrmDevice::new(adapter.clone(), 1, 2, 3, 4);
    if let Err(e) = crate::drm::register_primary_device(device) {
        power.restore(&window).map_err(message)?;
        return Err(format!("fastboot KMS registration failed: {e}"));
    }
    Ok(format!(
        "intel-fastboot: native fixed-mode KMS registered {mode:?} {port:?}; firmware \
         clocks/link/WM retained; pageflip completion requires SURFLIVE and fresh hardware \
         frames; 未在硬件上验证"
    ))
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, collections::BTreeMap, vec};
    use core::sync::atomic::AtomicU64;

    use super::{super::gtt::PageTable, *};
    #[derive(Clone)]
    struct Model {
        inner: Arc<Mutex<ModelState>>,
    }
    struct ModelState {
        words: BTreeMap<u32, u32>,
        dkl: BTreeMap<u32, u32>,
        log: Vec<(u32, u32)>,
        frames: bool,
        writes: usize,
        fail: Option<usize>,
        stall: bool,
        fail_surface: bool,
    }
    impl Model {
        fn new() -> Self {
            let mut words = BTreeMap::new();
            for (r, v) in [
                (0x45404, 0x405),
                (0x45454, 0x40),
                (0x45444, 0x40),
                (0x45504, 0),
                (0x70008, 3 << 30),
                (0x78000, 0),
                (0x78004, 0),
                (0x420c0, 0),
                (0x60420, 0),
                (0x6001c, (63 << 16) | 63),
                (0x70030, 0),
                (0x45270, 1),
                (0x6002c, 0),
                (0x6007c, 0),
                (0x70180, (1 << 31) | (4 << 24)),
                (0x701cc, 1 << 13),
                (0x7019c, 0x200000),
                (0x701ac, 0x200000),
                (0x701a4, 0),
                (0x70190, (63 << 16) | 63),
                (0x70188, 4),
                (0x7018c, 0),
                (0x70080, 0),
                (0x60400, (1 << 31) | (4 << 27) | (3 << 16)),
                (0x161500, 4),
                (0x64300, (1 << 31) | (1 << 6)),
                (0x163880, 0),
                (0x1638a0, 15),
                (0x4610c, 8 << 28),
                (0x164280, 0),
                (0x51004, 0),
                (0x46038, 0xcc000000),
                (0x1010a0, 0x44332211),
                (0x60200, 0),
                (0x4a480, 0),
                (0x49028, 0),
                (0x70034, 0),
                (0x68180, 0),
                (0x68280, 0),
                (0x45008, 3 << 30),
                (0x44fe8, 0),
                (0x44300, 0),
                (0x44304, 0),
                (0x4438c, 0),
                (0x70040, 0),
            ] {
                words.insert(r, v);
            }
            for i in 1..5 {
                words.insert(0x70180 + i * 0x100, 0);
            }
            let pair = |a: u32, b: u32| (a - 1) | ((b - 1) << 16);
            for (r, v) in [
                (0x60000, pair(64, 2200)),
                (0x60004, pair(64, 2200)),
                (0x60008, pair(80, 100)),
                (0x6000c, pair(64, 1125)),
                (0x60010, pair(64, 1125)),
                (0x60014, pair(66, 70)),
            ] {
                words.insert(r, v);
            }
            for (base, cursor) in [(0x70240, false), (0x70140, true)] {
                for plane in 0..if cursor { 1 } else { 5 } {
                    let p = base + plane * 0x100;
                    for level in 0..6 {
                        words.insert(
                            p + level * 4,
                            if !cursor && plane == 0 {
                                (1 << 31) | 10
                            } else {
                                0
                            },
                        );
                    }
                    for delta in [0x28, 0x18, 0x1c] {
                        words.insert(p + delta, 0);
                    }
                    words.insert(
                        p + 0x3c,
                        if !cursor && plane == 0 {
                            (127 << 16) | 1
                        } else {
                            0
                        },
                    );
                }
            }
            let pll = intel_display::dpll_mgr::icl_calc_mg_pll_state(148500, 24000, None).unwrap();
            let mut dkl = BTreeMap::new();
            for (r, v) in [
                (0x212c, pll.refclkin_ctl),
                (0x20d4, pll.hsclkctl),
                (0x20d8, pll.coreclkctl1),
                (0x2200, pll.div0),
                (0x2204, pll.div1),
                (0x2210, pll.ssc),
                (0x2214, pll.bias),
                (0x2218, pll.tdc_coldst_bias),
                (0x236c, 1 << 15),
            ] {
                dkl.insert(r, v);
            }
            for bank in [0, 0x1000] {
                for r in [0x14, 0xa0, 0xd00, 0x2c0, 0x2c4, 0x2c8, 0x2f8, 0xdc4, 0xdc8] {
                    dkl.insert(bank + r, 0);
                }
            }
            Self {
                inner: Arc::new(Mutex::new(ModelState {
                    words,
                    dkl,
                    log: Vec::new(),
                    frames: true,
                    writes: 0,
                    fail: None,
                    stall: false,
                    fail_surface: false,
                })),
            }
        }
        fn set(&self, r: u32, v: u32) {
            self.inner.lock().words.insert(r, v);
        }
    }
    impl Registers for Model {
        fn read64(&self, r: Register) -> Option<u64> {
            self.read(r).map(u64::from)
        }
        fn read(&self, r: Register) -> Option<u32> {
            let mut s = self.inner.lock();
            let r = r.offset();
            if r == 0x70040 && s.frames {
                let v = s.words.get_mut(&r)?;
                *v = v.wrapping_add(1);
                return Some(*v);
            }
            if (0x168000..0x169000).contains(&r) {
                let bank = s.words.get(&0x1010a0)? & 15;
                return s.dkl.get(&(bank * 0x1000 + r - 0x168000)).copied();
            }
            s.words.get(&r).copied()
        }
        fn write(&self, r: Register, v: u32) -> bool {
            let mut s = self.inner.lock();
            let r = r.offset();
            s.writes += 1;
            let fail = s.fail == Some(s.writes) || (r == 0x7019c && s.fail_surface);
            if r == 0x7019c {
                s.fail_surface = false;
            }
            s.log.push((r, v));
            if [0x45404, 0x45454, 0x45444].contains(&r) {
                let state = s.words[&r] & 0x55555555;
                s.words.insert(r, (v & 0xaaaaaaaa) | state);
            } else {
                s.words.insert(r, v);
            }
            if r == 0x7019c && !s.stall {
                s.words.insert(0x701ac, v);
            }
            !fail // error may have landed, just like MMIO.
        }
    }
    #[derive(Clone)]
    struct Timer(Arc<AtomicU64>);
    impl PollTimer for Timer {
        fn pause(&self) {}
        fn now_micros(&self) -> u64 {
            self.0.fetch_add(1000, Ordering::Relaxed)
        }
    }
    fn native() -> (
        Arc<Native<Model, Timer>>,
        Model,
        super::super::gtt::mock::MockPageTable,
    ) {
        let r = Model::new();
        let power = PowerPin::acquire(&r, TcPort::Tc1).unwrap();
        let firmware = capture(&r, &power, TcPort::Tc1).unwrap();
        assert_eq!(firmware, capture(&r, &power, TcPort::Tc1).unwrap());
        let mode = mode(&firmware).unwrap();
        let array = super::super::gtt::mock::MockPageTable::new(65536);
        for i in 0..4 {
            array.write(0x200000 / 4096 + i, 0x80000001 + (i as u64) * 4096);
        }
        let gtt = Arc::new(Gtt::over(Box::new(array.clone())).unwrap());
        let adapter = Arc::new(Native {
            registers: r.clone(),
            timer: Timer(Arc::new(AtomicU64::new(0))),
            gtt,
            power,
            firmware,
            port: TcPort::Tc1,
            mode,
            pci: axdriver_display::DisplayPciIdentity {
                bus: 0,
                device: 2,
                function: 0,
                vendor_id: 0x8086,
                device_id: 0x46d0,
                subsystem_vendor: 0,
                subsystem_device: 0,
                revision: 0,
            },
            state: Mutex::new(State {
                current: None,
                quarantine: Vec::new(),
                unbound_quarantine: Vec::new(),
                lost: false,
                frame_progress: None,
            }),
        });
        (adapter, r, array)
    }
    fn scanout(a: &Native<Model, Timer>) -> Scanout {
        let backing = a
            .create_dumb(
                DumbRequest {
                    width: 64,
                    height: 64,
                    bpp: 32,
                },
                256,
                16384,
                Arc::new(()),
            )
            .unwrap();
        Scanout {
            backing,
            width: 64,
            height: 64,
            pitch: 256,
            bpp: 32,
            format: intel_display::universal_plane::XRGB8888,
            framebuffer_width: 64,
            framebuffer_height: 64,
            backing_size: 16384,
            framebuffer_offset: 0,
            offset: 0,
            source_x: 0,
            source_y: 0,
            mode: a.mode,
            damage: None,
        }
    }
    #[test]
    fn already_on_power_pin_refuses_dark_and_recovers_landed_failure() {
        for n in 1..=3 {
            let r = Model::new();
            let before = [
                read(&r, 0x45404).unwrap(),
                read(&r, 0x45454).unwrap(),
                read(&r, 0x45444).unwrap(),
            ];
            r.inner.lock().fail = Some(n);
            assert!(PowerPin::acquire(&r, TcPort::Tc1).is_err());
            for (&offset, v) in [0x45404, 0x45454, 0x45444].iter().zip(before) {
                assert_eq!(read(&r, offset).unwrap(), v);
            }
        }
        let r = Model::new();
        r.set(0x45444, 0);
        assert!(PowerPin::acquire(&r, TcPort::Tc1).is_err());
        assert!(r.inner.lock().log.is_empty());
    }
    #[test]
    fn ownership_checks_exact_boot_aperture_stolen_pte_and_allocator_exclusion() {
        let (a, _, array) = native();
        let boot = axhal::boot::BootFramebuffer {
            address: 0xc0200000,
            width: 64,
            height: 64,
            pitch: 256,
            bpp: 32,
            red: axgpu::ColorChannel {
                position: 16,
                size: 8,
            },
            green: axgpu::ColorChannel {
                position: 8,
                size: 8,
            },
            blue: axgpu::ColorChannel {
                position: 0,
                size: 8,
            },
        };
        assert!(
            ownership(
                &a.gtt,
                &a.firmware,
                &boot,
                0xc0000000,
                0x80000000..0x90000000,
                &[]
            )
            .is_ok()
        );
        assert!(
            ownership(
                &a.gtt,
                &a.firmware,
                &boot,
                0xc0000000,
                0x80000000..0x90000000,
                &[(0x80001000, 4096)]
            )
            .is_err()
        );
        assert!(
            ownership(
                &a.gtt,
                &a.firmware,
                &boot,
                0xd0000000,
                0x80000000..0x90000000,
                &[]
            )
            .is_err()
        );
        array.write(513, 0x80001003);
        assert!(
            ownership(
                &a.gtt,
                &a.firmware,
                &boot,
                0xc0000000,
                0x80000000..0x90000000,
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn native_present_binds_scattered_ram_latches_and_retires_old_after_frames() {
        let _context = crate::test_support::scheduler_test_context();
        let (a, r, array) = native();
        let first = scanout(&a);
        let pages = first.backing.shared_pages().unwrap();
        let done = a.present(first).unwrap();
        assert!(done.is_signaled() && !done.is_failed());
        let old = a.state.lock().current.as_ref().unwrap().binding.address;
        for i in 0..4 {
            assert_eq!(
                array.raw(old as usize / 4096 + i),
                pages.paddr_at(i).unwrap().as_usize() as u64 | 1
            );
        }
        assert_eq!(array.raw(512), 0x80000001);
        a.present(scanout(&a)).unwrap();
        assert_eq!(array.raw(old as usize / 4096), 0);
        let writes = r.inner.lock().log.clone();
        assert!(
            writes
                .iter()
                .all(|(r, _)| [0x45404, 0x45454, 0x45444, 0x1010a0, 0x7019c].contains(r))
        );
    }
    #[test]
    fn stopped_frame_never_signals_success_and_keeps_dma_quarantined() {
        let _context = crate::test_support::scheduler_test_context();
        let (a, r, _) = native();
        let s = scanout(&a);
        r.inner.lock().frames = false;
        assert_eq!(a.present(s).err(), Some(DrmError::DeviceLost));
        assert!(a.state.lock().lost);
        assert_eq!(a.state.lock().quarantine.len(), 1);
    }
    #[test]
    fn changed_state_and_invalid_geometry_refuse_before_plane_write() {
        let _context = crate::test_support::scheduler_test_context();
        let (a, r, _) = native();
        let mut s = scanout(&a);
        s.pitch = 512;
        assert_eq!(a.present(s).err(), Some(DrmError::Unsupported));
        r.set(0x70030, 1 << 11);
        assert_eq!(a.present(scanout(&a)).err(), Some(DrmError::DeviceLost));
        assert!(!r.inner.lock().log.iter().any(|(r, _)| *r == 0x7019c));
    }
    #[test]
    fn drm_fixed_mode_dumb_framebuffer_commit_reaches_native_present() {
        let _context = crate::test_support::scheduler_test_context();
        let (a, r, _) = native();
        let dev = crate::drm::DrmDevice::new(a.clone(), 1, 2, 3, 4);
        let file = dev.open_primary();
        let dumb = file
            .create_dumb(DumbRequest {
                width: 64,
                height: 64,
                bpp: 32,
            })
            .unwrap();
        let fb = file
            .add_framebuffer(dumb.handle, 64, 64, dumb.pitch, 32)
            .unwrap();
        let source = 64 << 16;
        use crate::drm::{atomic::Change, property};
        file.submit_legacy_atomic(
            &[
                Change {
                    object: 1,
                    property: property::CONNECTOR_CRTC_ID,
                    value: 3,
                },
                Change {
                    object: 3,
                    property: property::CRTC_ACTIVE,
                    value: 1,
                },
                Change {
                    object: 3,
                    property: property::CRTC_MODE_ID,
                    value: 0,
                },
                Change {
                    object: 4,
                    property: property::PLANE_FB_ID,
                    value: fb as u64,
                },
                Change {
                    object: 4,
                    property: property::PLANE_CRTC_ID,
                    value: 3,
                },
                Change {
                    object: 4,
                    property: property::PLANE_SRC_W,
                    value: source,
                },
                Change {
                    object: 4,
                    property: property::PLANE_SRC_H,
                    value: source,
                },
                Change {
                    object: 4,
                    property: property::PLANE_CRTC_W,
                    value: 64,
                },
                Change {
                    object: 4,
                    property: property::PLANE_CRTC_H,
                    value: 64,
                },
            ],
            Some(a.mode),
            Some(17),
            true,
        )
        .unwrap();
        for _ in 0..6 {
            dev.advance_vblank().unwrap();
        }
        assert!(r.inner.lock().log.iter().any(|(r, _)| *r == 0x7019c));
        assert_eq!(file.resources().crtc.framebuffer, Some(fb));
        assert!(file.dequeue_event().is_some());
    }
    #[test]
    fn landed_plane_write_error_restores_firmware_and_releases_only_proven_inactive_mapping() {
        let _context = crate::test_support::scheduler_test_context();
        let (a, r, array) = native();
        r.inner.lock().fail_surface = true;
        assert_eq!(a.present(scanout(&a)).err(), Some(DrmError::Busy));
        assert_eq!(read(&r, 0x7019c), Ok(0x200000));
        assert_eq!(read(&r, 0x701ac), Ok(0x200000));
        assert!(!a.state.lock().lost);
        assert!(a.state.lock().quarantine.is_empty());
        assert!(a.state.lock().current.is_none());
        assert_eq!(array.raw(512), 0x80000001);
        for i in 0..array.entries() {
            if !(512..516).contains(&i) {
                assert_eq!(array.raw(i), 0);
            }
        }
    }
    #[test]
    fn native_hardware_frame_gate_never_manufactures_vblank_events_or_color_dpms_success() {
        let (a, r, _) = native();
        r.inner.lock().frames = false;
        let dev = crate::drm::DrmDevice::new(a.clone(), 1, 2, 3, 4);
        for _ in 0..5 {
            dev.advance_vblank().unwrap();
        }
        assert_eq!(dev.metrics().vblanks, 0);
        let file = dev.open_primary();
        file.wait_vblank(77).unwrap();
        assert_eq!(dev.metrics().pending_vblank_events, 1);
        a.timer.0.store(1_000_000, Ordering::Relaxed);
        assert_eq!(dev.advance_vblank(), Err(DrmError::DeviceLost));
        assert_eq!(dev.metrics().pending_vblank_events, 0);
        assert!(file.dequeue_event().is_none());
        assert_eq!(
            a.validate_atomic_state(false, true, false),
            Err(DrmError::Unsupported)
        );
        assert_eq!(
            a.validate_atomic_state(true, false, false),
            Err(DrmError::Unsupported)
        );
        assert_eq!(
            a.validate_atomic_state(true, true, true),
            Err(DrmError::Unsupported)
        );
    }
    #[test]
    fn failed_pll_readout_restores_hip_and_terminally_closes_native_submission() {
        let _context = crate::test_support::scheduler_test_context();
        let (a, r, _) = native();
        r.inner.lock().dkl.remove(&0x2214);
        assert_eq!(a.present(scanout(&a)).err(), Some(DrmError::DeviceLost));
        assert!(a.state.lock().lost);
        assert_eq!(read(&r, 0x1010a0), Ok(0x44332211));
        let writes = r.inner.lock().log.len();
        assert_eq!(a.present(scanout(&a)).err(), Some(DrmError::DeviceLost));
        assert_eq!(r.inner.lock().log.len(), writes);
    }
}
