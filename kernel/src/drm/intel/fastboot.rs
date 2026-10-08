//! N305 firmware-preserving KMS with a restricted TC1/TC2 legacy-HDMI timing
//! transition. The first scanout preserves the firmware display image; later
//! admitted modes may change the DKL PLL, transcoder timing, and plane while
//! retaining the captured WM/DDB policy. Uses MIT i915 readouts/programming in
//! `tk-intel-display`; adapter/ownership policy is original TheKernel code.
//! Only pipe-A, opaque linear XR24, no scaling/color/DSC/VRR is admitted.
//! Hardware writes remain opt-in.
use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering, fence};

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
    regs::{self, Meaning, Register, Registers, pipe as p},
};
use crate::{
    drm::{
        DisplayAdapter, DrmError, DrmResult, DumbRequest, GemBacking, Mode as DrmMode, Scanout,
        fence::Fence,
    },
    mm::{SharedFixedView, SharedPages},
};

// Compile the adjacent DPLL adapter in the host test build without adding a
// production callsite or requiring the owner module to wire it yet.
#[cfg(test)]
#[path = "shared_dpll.rs"]
mod shared_dpll_adapter_compile_check;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeMode {
    timing: crate::drm::modes::Mode,
    kms: DrmMode,
    vic: u8,
}

fn drm_mode(timing: crate::drm::modes::Mode) -> DrmMode {
    DrmMode {
        width: u32::from(timing.hdisplay),
        height: u32::from(timing.vdisplay),
        refresh_millihz: timing.refresh_millihz(),
    }
}

fn firmware_timing(f: &Firmware) -> Result<crate::drm::modes::Mode, Error> {
    use crate::drm::modes::{Mode, ModeFlags, TimingSource};
    let t = f.pipe.timings;
    if t.interlaced
        || t.set_context_latency != 0
        || t.hblank_start != t.hdisplay
        || t.hblank_end != t.htotal
        || t.vblank_start != t.vdisplay
        || t.vblank_end != t.vtotal
        || [
            t.hdisplay,
            t.htotal,
            t.hsync_start,
            t.hsync_end,
            t.vdisplay,
            t.vtotal,
            t.vsync_start,
            t.vsync_end,
        ]
        .into_iter()
        .any(|count| count > u32::from(u16::MAX))
    {
        return Err(Error::Refused);
    }
    let hblank = t.htotal - t.hdisplay;
    let hfront = t.hsync_start - t.hdisplay;
    let hsync = t.hsync_end - t.hsync_start;
    let vblank = t.vtotal - t.vdisplay;
    let vfront = t.vsync_start - t.vdisplay;
    let vsync = t.vsync_end - t.vsync_start;
    let mode = Mode::from_blanking(
        f.pixel_clock,
        t.hdisplay as u16,
        hblank as u16,
        hfront as u16,
        hsync as u16,
        t.vdisplay as u16,
        vblank as u16,
        vfront as u16,
        vsync as u16,
        ModeFlags::NONE,
        TimingSource::Firmware,
    )
    .with_polarity(f.ddi.positive_hsync, f.ddi.positive_vsync);
    if !mode.is_well_formed() {
        return Err(Error::Refused);
    }
    Ok(mode)
}

fn cta_vic(mode: &crate::drm::modes::Mode) -> Option<u8> {
    use crate::drm::modes::ModeFlags;
    if mode.flags != ModeFlags::NONE {
        return None;
    }
    // The only cold timing transition admitted here is the measured N305
    // HDMI route's 1080p60 request; the captured 4K30 timing remains restorable.
    let timing = crate::drm::modes::CTA_VIC_TIMINGS
        .iter()
        .find(|entry| entry.vic == 16)
        .map(|entry| entry.mode)?;
    if mode.same_timing(&timing) {
        return Some(16);
    }
    if mode.same_timing(&firmware_4k30()) {
        return Some(95);
    }
    None
}

fn firmware_4k30() -> crate::drm::modes::Mode {
    crate::drm::modes::Mode::from_blanking(
        297_000,
        3840,
        560,
        176,
        88,
        2160,
        90,
        8,
        10,
        crate::drm::modes::ModeFlags::NONE,
        crate::drm::modes::TimingSource::Firmware,
    )
    .with_polarity(true, true)
}

fn native_modes(
    edid_bytes: &[u8],
    f: &Firmware,
) -> Result<(Vec<NativeMode>, NativeMode, NativeMode), Error> {
    use crate::drm::modes::{Edid, ModeList, collect_modes};
    let edid = Edid::parse_lossy(edid_bytes).map_err(|_| Error::Refused)?;
    let sink_tmds_limit = edid.max_tmds_clock_khz();
    let mut candidates = ModeList::new();
    collect_modes(&edid, &mut candidates);
    let current_timing = firmware_timing(f)?;
    let current_vic = cta_vic(&current_timing).ok_or(Error::Refused)?;
    let current = NativeMode {
        timing: current_timing,
        kms: drm_mode(current_timing),
        vic: current_vic,
    };
    let mut modes = Vec::new();
    modes
        .try_reserve_exact(2)
        .map_err(|_| Error::Unavailable(0))?;
    // Preserve the equivalent firmware mode for boot fbdev/console. Exposing
    // a second mode must not force a destructive PLL transition at handoff.
    modes.push(current);
    if let Some(target) = candidates.modes().copied().find(|mode| {
        crate::drm::intel::modeset::is_reference_timing(mode)
            && mode.clock_khz == 148_500
            && cta_vic(mode) == Some(16)
            && super::tc_modeset::source_hdmi_tmds_clock_with_limit(
                mode,
                sink_tmds_limit,
                intel_display::intel_hdmi_full::HdmiPortClass::TypeC,
            )
            .is_some()
    }) {
        let target = NativeMode {
            timing: target,
            kms: drm_mode(target),
            vic: 16,
        };
        if !target.timing.same_timing(&current.timing)
            && pitch_for(target.kms.width)
                .is_some_and(|pitch| retained_watermark_budget(f, target, pitch))
            && intel_display::dpll_mgr::icl_calc_mg_pll_state(
                target.timing.clock_khz,
                f.refclk,
                f.afc_startup,
            )
            .is_ok()
        {
            modes.push(target);
        }
    }
    Ok((modes, current, current))
}

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
        let _lock = super::DKL_ACCESS_LOCK.lock();
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
pub(super) struct PowerPin {
    port: TcPort,
    offsets: [u32; 3],
    before: [u32; 3],
    masks: [u32; 3],
}

impl PowerPin {
    pub(super) fn acquire(r: &impl Registers, port: TcPort) -> Result<Self, Error> {
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
            port,
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

    /// Revalidate the long-lived fastboot lease before a shared-DPLL adapter
    /// uses its read-only logical power references. This never requests or
    /// releases a well: it proves the source-map-backed pin is still held,
    /// DC states remain disabled, and the oscillator still matches the
    /// firmware-captured DPLL reference clock.
    pub(super) fn verify_dpll_context(
        &self,
        r: &impl Registers,
        port: TcPort,
        reference_khz: u32,
    ) -> Result<(), Error> {
        if self.port != port || port.index() > 1 {
            return Err(Error::Refused);
        }
        self.held(r)?;
        if read(r, 0x45504)? & super::power::DC_STATE_MASK != 0 {
            return Err(Error::Refused);
        }
        let reference = match r
            .read(regs::SKL_DSSM)
            .ok_or(Error::Unavailable(regs::SKL_DSSM.offset()))?
            >> 29
        {
            0 => 24_000,
            1 => 19_200,
            2 => 38_400,
            _ => return Err(Error::Refused),
        };
        if reference != reference_khz {
            return Err(Error::Refused);
        }
        Ok(())
    }

    pub(super) const fn port(&self) -> TcPort {
        self.port
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
    trans_clock: u32,
    hdmi: intel_display::hdmi::HdmiReadout,
    wm: [intel_display::watermark::PlaneWatermarks; 6],
    ddb: [intel_display::watermark::DdbEntry; 6],
    dbuf: intel_display::watermark::DbufState,
    color: intel_display::color::ColorConfig,
    scalers: [intel_display::scaler::Scaler; 2],
    refclk: u32,
    pixel_clock: u32,
    afc_startup: Option<u8>,
}
fn capture(
    r: &impl Registers,
    pin: &PowerPin,
    port: TcPort,
    afc_startup: Option<u8>,
) -> Result<Firmware, Error> {
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
    let trans_clock = read(r, 0x46140)?;
    if !clock.enabled || clock.pll != ddi::TcPllKind::Dkl {
        return Err(Error::Refused);
    }
    let refclk = match read(r, 0x51004)? >> 29 {
        0 => 24000,
        1 => 19200,
        2 => 38400,
        _ => return Err(Error::Refused),
    };
    let pll = dpll_mgr::dkl_pll_get_hw_state(&io, port, refclk, afc_startup.is_some())?
        .ok_or(Error::Refused)?;
    if afc_startup.is_some_and(|value| ((pll.state.div0 >> 25) & 7) != u32::from(value)) {
        return Err(Error::Refused);
    }
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
        trans_clock,
        hdmi,
        wm,
        ddb,
        dbuf,
        color,
        scalers,
        refclk,
        pixel_clock,
        afc_startup,
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
fn mode(f: &Firmware) -> Result<DrmMode, Error> {
    let t = f.pipe.timings;
    let hz = u64::from(f.pixel_clock) * 1_000_000 / (u64::from(t.htotal) * u64::from(t.vtotal));
    if !(25_000..=240_000).contains(&hz) {
        return Err(Error::Refused);
    }
    Ok(DrmMode {
        width: t.hdisplay,
        height: t.vdisplay,
        refresh_millihz: hz.try_into().map_err(|_| Error::Refused)?,
    })
}
fn read_route_edid<R: Registers, T: PollTimer>(
    registers: &R,
    timer: &T,
    pin: super::gmbus::Pin,
) -> Result<Vec<u8>, Error> {
    let mut notes = super::gmbus::BusNotes::default();
    let base = super::gmbus::read_edid_with(registers, timer, pin, &mut notes)
        .map_err(|_| Error::Refused)?;
    if base.extension_count() > 1 {
        return Err(Error::Refused);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::from(base.extension_count() + 1) * 128)
        .map_err(|_| Error::Unavailable(0))?;
    bytes.extend_from_slice(base.as_slice());
    if base.extension_count() == 1 {
        let extension = super::gmbus::read_edid_extension_with(registers, timer, pin)
            .map_err(|_| Error::Refused)?
            .ok_or(Error::Refused)?;
        bytes.extend_from_slice(extension.as_slice());
    }
    let mut verify_notes = super::gmbus::BusNotes::default();
    let verify = super::gmbus::read_edid_with(registers, timer, pin, &mut verify_notes)
        .map_err(|_| Error::Refused)?;
    if verify != base {
        return Err(Error::Refused);
    }
    Ok(bytes)
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
    firmware: Firmware,
    current_mode: NativeMode,
    connected: bool,
    last_hpd_poll: u64,
    last_hpd_irq: Option<u64>,
    hpd_good_samples: u8,
    hpd_bad_samples: u8,
    irq_fault_reported: bool,
    unsupported_sink_reported: bool,
    current: Option<Bound>,
    quarantine: Vec<Bound>,
    unbound_quarantine: Vec<(Arc<SharedPages>, SharedFixedView)>,
    lost: bool,
    frame_progress: Option<(u32, u64)>,
    counter_epoch: u64,
}
struct Native<R, T> {
    registers: R,
    timer: T,
    gtt: Arc<Gtt>,
    power: PowerPin,
    shared_dpll: Mutex<super::shared_dpll::SharedDpllState>,
    baseline: Firmware,
    modes: Vec<NativeMode>,
    preferred: DrmMode,
    sink_edid: Vec<u8>,
    afc_startup: Option<u8>,
    watermark: super::pipe::WatermarkConfig,
    port: TcPort,
    pci: axdriver_display::DisplayPciIdentity,
    irq_event_sequence: AtomicU32,
    state: Mutex<State>,
}

/// Re-read the selected TC PLL through the translated generic manager while
/// the Native-owned pin keeps the source-mapped display/PHY domains alive.
fn translated_tc_dpll_readout<R: Registers, T: PollTimer>(
    native: &Native<R, T>,
) -> Result<(bool, intel_display::intel_dpll_mgr_full::IntelDpllHwState), String> {
    let identity = super::shared_dpll::AdlNIdentity::verify(
        native.pci.vendor_id,
        native.pci.device_id,
        native.pci.revision,
    )
    .map_err(|error| format!("shared DPLL identity changed: {error:?}"))?;
    let mut power =
        super::shared_dpll::PinnedDpllPower::new(&native.power, identity, native.baseline.refclk)
            .map_err(|error| format!("shared DPLL power context unavailable: {error:?}"))?;
    native
        .shared_dpll
        .lock()
        .get_hw_state(
            &native.registers,
            &native.timer,
            &mut power,
            (3 + native.port.index()) as usize,
        )
        .map_err(|error| format!("translated shared DPLL readout refused: {error:?}"))
}

/// Debounce task-context DDC samples and report the physical connector state
/// independently of whether the audio owner could retire or publish its HDA
/// route. Audio errors are retained for diagnostics, not used to hide a
/// confirmed cable transition; the hook must keep TC link/power ownership when
/// HDA state is uncertain.
fn hpd_config_transition(
    state: &mut State,
    sample_matches: bool,
    preferred: DrmMode,
    audio_transition: impl FnOnce(bool) -> Result<(), String>,
) -> Option<(crate::drm::device::DisplayConfig, Option<String>)> {
    if sample_matches {
        state.hpd_good_samples = state.hpd_good_samples.saturating_add(1);
        state.hpd_bad_samples = 0;
    } else {
        state.hpd_bad_samples = state.hpd_bad_samples.saturating_add(1);
        state.hpd_good_samples = 0;
    }
    let connected = if state.connected && state.hpd_bad_samples >= 2 {
        false
    } else if !state.connected && state.hpd_good_samples >= 2 {
        true
    } else {
        return None;
    };
    let audio_error = audio_transition(connected).err();
    state.connected = connected;
    state.hpd_good_samples = 0;
    state.hpd_bad_samples = 0;
    Some((
        crate::drm::device::DisplayConfig {
            connected,
            mode: connected.then_some(preferred),
        },
        audio_error,
    ))
}

fn hpd_probe_due(now: u64, last_poll: u64, last_irq: Option<u64>) -> bool {
    now.saturating_sub(last_poll) >= 250_000
        && last_irq.is_none_or(|last| now.saturating_sub(last) >= 250_000)
}

fn frame_count(r: &impl Registers) -> Result<u32, Error> {
    // All-ones is a valid frame just before wrap, unlike configuration words.
    r.read(reg(0x70040, false))
        .ok_or(Error::Unavailable(0x70040))
}

/// The display-12/13 PIPEFRAME and PIPEFRAMEPIXEL pair is not synchronized.
/// This adapter samples the high counter on both sides of the pixel register,
/// matching intel_vblank.c's stable 64-bit read before its vblank-boundary
/// adjustment.  A failed MMIO read is reported to the caller rather than
/// becoming a fabricated zero counter.
struct VblankCounterIo<'a, R> {
    registers: &'a R,
    valid: bool,
}

impl<R: Registers> intel_display::intel_vblank_full::VblankIo for VblankCounterIo<'_, R> {
    fn read64_frame_pixel(&mut self, _pipe: u8) -> u64 {
        for _ in 0..4 {
            let high_before = self.registers.read(reg(0x70040, false));
            let pixel = self.registers.read(reg(0x70044, false));
            let high_after = self.registers.read(reg(0x70040, false));
            match (high_before, pixel, high_after) {
                (Some(before), Some(pixel), Some(after)) if before == after => {
                    return (u64::from(before & 0xffff) << 32) | u64::from(pixel);
                }
                (Some(_), Some(_), Some(_)) => {}
                _ => {
                    self.valid = false;
                    return 0;
                }
            }
        }
        self.valid = false;
        0
    }
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

fn pitch_for(width: u32) -> Option<u32> {
    width
        .checked_mul(4)?
        .checked_add(63)
        .map(|pitch| pitch & !63)
}

/// Preserve firmware WM/DDB only for the source-checked linear-XRGB profile.
/// Clock and pitch alone are not a watermark proof: method selection, line
/// demand and DDB minima also depend on htotal/width. The exact 4K30->1080p60
/// reduction has a matching source line time and no worse demand at every
/// source-valid latency; unknown profiles refuse before display writes.
fn retained_watermark_budget(baseline: &Firmware, target: NativeMode, pitch: u32) -> bool {
    intel_display::watermark::adlp_linear_xrgb_4k30_watermark_profile_no_worse(
        baseline.pixel_clock,
        baseline.plane.width,
        baseline.pipe.timings.htotal,
        target.timing.clock_khz,
        u32::from(target.timing.hdisplay),
        u32::from(target.timing.htotal),
    ) && pitch <= baseline.plane.pitch
        && baseline.dbuf.enabled_slices != 0
        && baseline.ddb[0].blocks() != 0
        && baseline.wm[0].levels[0].enable
}

fn avi_frame(words: [u32; 8]) -> intel_display::hdmi::RawInfoframe {
    let mut raw = [0; 32];
    for (word, bytes) in words.into_iter().zip(raw.chunks_exact_mut(4)) {
        bytes.copy_from_slice(&word.to_le_bytes());
    }
    intel_display::hdmi::RawInfoframe {
        raw,
        kind: intel_display::hdmi::FrameType::Avi,
    }
}

fn same_mode_state(
    observed: &Firmware,
    baseline: &Firmware,
    mode: NativeMode,
    surface: u32,
    pitch: u32,
    pll: &intel_display::dpll_mgr::DklPllState,
    port: TcPort,
    expected_avi: intel_display::hdmi::RawInfoframe,
    expected_phy: &intel_display::tc::DklPhyState,
) -> bool {
    let t = mode.timing;
    let pipe = observed.pipe;
    let same_timings = pipe.timings.hdisplay == u32::from(t.hdisplay)
        && pipe.timings.htotal == u32::from(t.htotal)
        && pipe.timings.hblank_start == u32::from(t.hdisplay)
        && pipe.timings.hblank_end == u32::from(t.htotal)
        && pipe.timings.hsync_start == u32::from(t.hsync_start)
        && pipe.timings.hsync_end == u32::from(t.hsync_end)
        && pipe.timings.vdisplay == u32::from(t.vdisplay)
        && pipe.timings.vtotal == u32::from(t.vtotal)
        && pipe.timings.vblank_start == u32::from(t.vdisplay)
        && pipe.timings.vblank_end == u32::from(t.vtotal)
        && pipe.timings.vsync_start == u32::from(t.vsync_start)
        && pipe.timings.vsync_end == u32::from(t.vsync_end)
        && pipe.timings.set_context_latency == 0
        && !pipe.timings.interlaced;
    let route = match port {
        TcPort::Tc1 => Port::Tc1,
        TcPort::Tc2 => Port::Tc2,
        _ => return false,
    };
    observed.pixel_clock == t.clock_khz
        && same_timings
        && pipe.source == (u32::from(t.hdisplay), u32::from(t.vdisplay))
        && observed.ddi.enabled
        && observed.ddi.port == Some(route)
        && observed.ddi.mode == intel_display::ddi::DdiMode::Hdmi
        && observed.ddi.bpp == Some(24)
        && observed.plane.native_linear_xrgb()
        && observed.plane.pitch == pitch
        && observed.plane.width == u32::from(t.hdisplay)
        && observed.plane.height == u32::from(t.vdisplay)
        && observed.plane.surface() == surface & 0xffff_f000
        && observed.pll.state == *pll
        && observed.pll.enable == baseline.pll.enable
        && observed.refclk == baseline.refclk
        && observed.afc_startup == baseline.afc_startup
        && observed.ddi == baseline.ddi
        && observed.clock == baseline.clock
        && observed.trans_clock == baseline.trans_clock
        && observed.pipe.transconf == baseline.pipe.transconf
        && observed.pipe.dss == baseline.pipe.dss
        && observed.pipe.chicken == baseline.pipe.chicken
        && observed.pipe.frame_start_delay == baseline.pipe.frame_start_delay
        && observed.pipe.vrr == baseline.pipe.vrr
        && observed.pipe.misc.output == baseline.pipe.misc.output
        && observed.pipe.misc.bpc == baseline.pipe.misc.bpc
        && observed.pipe.misc.raw & !super::pipe::PIPE_MISC_OWNED_MASK
            == baseline.pipe.misc.raw & !super::pipe::PIPE_MISC_OWNED_MASK
        && observed.pipe.multiplier_raw == baseline.pipe.multiplier_raw
        && observed.pipe.pixel_multiplier == baseline.pipe.pixel_multiplier
        && observed.hdmi.control == baseline.hdmi.control
        && observed.hdmi.enable == baseline.hdmi.enable
        && observed.hdmi.enabled_packets == baseline.hdmi.enabled_packets
        && observed.hdmi.gcp == baseline.hdmi.gcp
        && observed.hdmi.frames[0] == Some(expected_avi)
        && observed.hdmi.frames[1..] == baseline.hdmi.frames[1..]
        && observed.wm == baseline.wm
        && observed.ddb == baseline.ddb
        && observed.dbuf == baseline.dbuf
        && observed.color == baseline.color
        && observed.scalers == baseline.scalers
        && observed.fia == baseline.fia
        && observed.phy == *expected_phy
}

impl<R: Registers + Send + Sync, T: PollTimer + Send + Sync> DisplayAdapter for Native<R, T> {
    fn driver_name(&self) -> &'static str {
        "thekernel_intel"
    }
    fn platform_name(&self) -> Option<&'static str> {
        Some("n305-native-fastboot")
    }
    fn preferred_mode(&self) -> DrmMode {
        self.preferred
    }
    fn supported_modes(&self) -> Vec<DrmMode> {
        self.modes.iter().map(|mode| mode.kms).collect()
    }
    fn mode_is_supported(&self, requested: DrmMode) -> bool {
        self.modes.iter().any(|mode| mode.kms == requested)
    }
    fn connector_edid(&self) -> Option<Vec<u8>> {
        Some(self.sink_edid.clone())
    }
    fn supports_cursor(&self) -> bool {
        false
    }
    fn primary_formats(&self) -> &'static [u32] {
        // Native fastboot currently validates and programs only the exact
        // opaque linear XR24 primary-plane path.
        &[intel_display::universal_plane::XRGB8888]
    }
    fn gamma_lut_size(&self) -> u32 {
        256
    }
    fn degamma_lut_size(&self) -> u32 {
        intel_display::intel_color_full::glk_degamma_lut_size(13) as u32
    }
    fn pci_identity(&self) -> Option<axdriver_display::DisplayPciIdentity> {
        Some(self.pci)
    }
    fn display_config_changed(&self) -> DrmResult<Option<crate::drm::device::DisplayConfig>> {
        let now = self.timer.now_micros();
        let mut state = self.state.lock();
        if state.lost {
            return Err(DrmError::DeviceLost);
        }
        if super::irq::faulted() && !state.irq_fault_reported {
            state.irq_fault_reported = true;
            axlog::warn!(
                "intel-irq: display MSI owner faulted; shared master disabled, retaining TC +                 link/power and falling back to PIPEFRAME plus debounced task-context HPD polling"
            );
        }
        if super::irq::take_hpd_pending() {
            // HPD is only a wake hint. Restart the quiet period on every
            // notification so a bouncing cable cannot tear down scanout from
            // one interrupt or a rapid IRQ storm.
            state.last_hpd_irq = Some(now);
        }
        // The south HPD status is an interrupt latch, not a cable-level GPIO.
        // The owned IRQ is only a wake hint: a bounded task-context GMBUS EDID
        // probe remains the source-backed connection test for this legacy
        // HDMI sink.
        if !hpd_probe_due(now, state.last_hpd_poll, state.last_hpd_irq) {
            return Ok(None);
        }
        state.last_hpd_irq = None;
        state.last_hpd_poll = now;
        if self.power.held(&self.registers).is_err() {
            state.lost = true;
            return Err(DrmError::DeviceLost);
        }
        let current_pixel_clock_khz = state.current_mode.timing.clock_khz;
        let pin = if self.port == TcPort::Tc1 {
            super::gmbus::Pin::Tc1
        } else {
            super::gmbus::Pin::Tc2
        };
        let observed = read_route_edid(&self.registers, &self.timer, pin);
        let mut unsupported_sink = false;
        let sample_matches = match observed {
            Ok(bytes) if bytes == self.sink_edid => true,
            Ok(_) => {
                unsupported_sink = !state.unsupported_sink_reported;
                state.unsupported_sink_reported = true;
                false
            }
            Err(_) => false,
        };
        if sample_matches {
            state.unsupported_sink_reported = false;
        }
        let transition =
            hpd_config_transition(&mut state, sample_matches, self.preferred, |connected| {
                // Retire or republish HDA only after the two-sample physical
                // decision and while the TC link/power pin are still held.
                // Audio errors do not falsify cable state or stop the
                // hardware-vblank worker: the audio owner quarantines unknown
                // HDA state, and the TC link remains powered.
                if connected {
                    match super::audio::after_link_enabled(
                        &self.registers,
                        &self.timer,
                        self.port,
                        current_pixel_clock_khz,
                        &self.sink_edid,
                    ) {
                        Ok(super::audio::LinkAudioStatus::Enabled) => Ok(()),
                        Ok(super::audio::LinkAudioStatus::Unavailable(reason)) => {
                            axlog::warn!("intel-hdmi-audio: HPD reconnect video-only: {reason}");
                            Ok(())
                        }
                        Err(error) => Err(format!(
                            "HPD reconnect audio handoff unverified (TC link/power retained): \
                             {error}"
                        )),
                    }
                } else {
                    super::audio::before_link_disable(&self.registers, &self.timer, self.port)
                        .map_err(|error| {
                            format!(
                                "HPD unplug reported, but HDA DMA retirement is unverified; TC \
                                 link/power remain held: {error}"
                            )
                        })
                }
            });
        let (update, audio_error) = transition
            .map(|(config, audio_error)| (Some(config), audio_error))
            .unwrap_or((None, None));
        drop(state);
        if let Some(error) = audio_error {
            axlog::error!("intel-hdmi-audio: {error}");
        }
        if unsupported_sink {
            axlog::warn!(
                "intel-hpd: TC connector EDID changed; native mode list is pinned to the \
                 boot-validated sink, reporting disconnected"
            );
        }
        Ok(update)
    }
    fn hardware_vblank_counter(&self) -> DrmResult<Option<(u64, u32)>> {
        let mut state = self.state.lock();
        if state.lost {
            return Err(DrmError::DeviceLost);
        }
        if self.power.held(&self.registers).is_err() {
            state.lost = true;
            return Err(DrmError::DeviceLost);
        }
        let timing = state.current_mode.timing;
        let mut vblank_io = VblankCounterIo {
            registers: &self.registers,
            valid: true,
        };
        let counter = intel_display::intel_vblank_full::i915_get_vblank_counter(
            &mut vblank_io,
            intel_display::intel_vblank_full::Display {
                display_ver: 13,
                ddi: true,
                ..Default::default()
            },
            0,
            intel_display::intel_vblank_full::VblankCrtc {
                hwmode: intel_display::intel_vblank_full::Mode {
                    clock: timing.clock_khz,
                    crtc_clock: timing.clock_khz,
                    htotal: i32::from(timing.htotal),
                    hsync_start: i32::from(timing.hsync_start),
                    vdisplay: i32::from(timing.vdisplay),
                    vblank_start: i32::from(timing.vdisplay),
                    vblank_end: i32::from(timing.vtotal),
                    vtotal: i32::from(timing.vtotal),
                    ..Default::default()
                },
                max_vblank_count: 0x00ff_ffff,
            },
        );
        if !vblank_io.valid {
            state.lost = true;
            return Err(DrmError::DeviceLost);
        }
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
        Ok(Some((state.counter_epoch, counter)))
    }
    fn wait_vblank_hint(
        &self,
        delay: core::time::Duration,
    ) -> Option<crate::drm::device::VblankWake> {
        let online = super::irq::online();
        let faulted = super::irq::faulted();
        if !online && !faulted {
            return crate::drm::device::wait_vblank_timer(delay);
        }
        let observed = self.irq_event_sequence.load(Ordering::Acquire);
        let current = super::irq::event_sequence();
        if current != observed {
            self.irq_event_sequence.store(current, Ordering::Release);
            return Some(crate::drm::device::VblankWake::Interrupt);
        }
        if !online {
            return crate::drm::device::wait_vblank_timer(delay);
        }
        match super::irq::wait_for_event(current, delay) {
            Ok(interrupted) => {
                self.irq_event_sequence
                    .store(super::irq::event_sequence(), Ordering::Release);
                Some(if interrupted {
                    crate::drm::device::VblankWake::Interrupt
                } else {
                    crate::drm::device::VblankWake::Timeout
                })
            }
            Err(_) => crate::drm::device::wait_vblank_timer(delay),
        }
    }
    fn validate_atomic_state(
        &self,
        active: bool,
        dpms_on: bool,
        gamma_lut: bool,
        color_pipeline_changed: bool,
    ) -> DrmResult<()> {
        if active && dpms_on && !gamma_lut && !color_pipeline_changed {
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
        let mode = self.modes.iter().find(|mode| {
            mode.kms.width == request.width
                && request.height >= mode.kms.height
                && request.height <= mode.kms.height.saturating_mul(2)
        });
        if mode.is_none() {
            return Err(DrmError::Unsupported);
        }
        let expected_pitch = request
            .width
            .checked_mul(4)
            .and_then(|width| width.checked_add(63))
            .map(|width| width & !63)
            .ok_or(DrmError::Overflow)?;
        if request.bpp != 32
            || pitch != expected_pitch
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
        let Some(target) = self.modes.iter().find(|mode| mode.kms == s.mode).copied() else {
            return Err(DrmError::Unsupported);
        };
        let Some(expected_pitch) = pitch_for(target.kms.width) else {
            return Err(DrmError::Overflow);
        };
        if s.width != target.kms.width
            || s.height != target.kms.height
            || s.framebuffer_width != s.width
            || s.pitch != expected_pitch
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
        if target != state.current_mode && state.counter_epoch == u64::MAX {
            return Err(DrmError::Overflow);
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
        let mut observed = match capture(&self.registers, &self.power, self.port, self.afc_startup)
        {
            Ok(v) => v,
            Err(_) => {
                state.lost = true;
                complete.signal_error();
                return Err(DrmError::DeviceLost);
            }
        };
        let live_surface = match read(&self.registers, p::PLANE_SURF_A.offset()) {
            Ok(v) => v,
            Err(_) => {
                state.lost = true;
                complete.signal_error();
                return Err(DrmError::DeviceLost);
            }
        };
        if observed.plane.surface_raw != live_surface
            || read(&self.registers, p::PLANE_SURFLIVE_A.offset()).ok()
                != Some(live_surface & 0xffff_f000)
        {
            state.lost = true;
            complete.signal_error();
            return Err(DrmError::DeviceLost);
        }
        observed.plane.surface_raw = state.firmware.plane.surface_raw;
        if observed != state.firmware {
            state.lost = true;
            complete.signal_error();
            return Err(DrmError::DeviceLost);
        }
        // Linux pin-to-display preparation leaves the CPU domain before
        // scanout. KMS already waited explicit/implicit producer fences; flush
        // the owned WB alias before either GGTT/SURF publication or a retained
        // binding update, including same-buffer fbdev damage.
        if super::gt::copy::sync_cpu_pages(pages.clone()).is_err() {
            complete.signal_error();
            return Err(DrmError::Unsupported);
        }
        let before = live_surface;
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
        let Some(surface) = address
            .checked_add(s.offset)
            .and_then(|v| u32::try_from(v).ok())
        else {
            if let Some(new) = next {
                // SAFETY: overflow is detected before this candidate's
                // PLANE_CTL/SURF write; the previously verified before-image
                // remains active, so the new GGTT binding has no DMA user.
                if unsafe { self.gtt.release_binding(&new.binding) }.is_err() {
                    state.quarantine.push(new);
                    state.lost = true;
                }
            }
            complete.signal_error();
            return Err(if state.lost {
                DrmError::DeviceLost
            } else {
                DrmError::Overflow
            });
        };
        let changing_mode = target.timing != state.current_mode.timing;
        if changing_mode
            && super::atomic_modeset_wiring::preflight_native_mode_change(
                state.current_mode.timing,
                target.timing,
                self.port,
            )
            .is_err()
        {
            // The translated-state projection is pure and runs before the
            // existing TC transaction. It only admits the same Pipe-A, TC1/2,
            // linear-XRGB8888, RGB 8-bpc, VIC 16/95 subset; it does not execute
            // any atomic hook or replace `tc_modeset::program`.
            if let Some(new) = next {
                // SAFETY: the preflight only inspects CPU-side state, before
                // any plane or link write; the verified old scanout remains
                // the sole DMA owner of a display surface.
                if unsafe { self.gtt.release_binding(&new.binding) }.is_err() {
                    state.quarantine.push(new);
                    state.lost = true;
                }
            }
            complete.signal_error();
            return Err(if state.lost {
                DrmError::DeviceLost
            } else {
                DrmError::Unsupported
            });
        }
        if !changing_mode {
            if latch(&self.registers, &self.timer, surface).is_err() {
                let recovered = latch(&self.registers, &self.timer, before).is_ok();
                if let Some(new) = next {
                    if recovered {
                        // SAFETY: `latch(before)` proved SURFLIVE matched the
                        // saved, previously active surface and then observed
                        // two fresh hardware frame counts; scanout no longer
                        // references this candidate mapping.
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
            state.firmware.plane.surface_raw = surface;
        } else {
            if !retained_watermark_budget(&self.baseline, target, s.pitch)
                || state.firmware.wm != self.baseline.wm
                || state.firmware.ddb != self.baseline.ddb
                || state.firmware.dbuf != self.baseline.dbuf
            {
                if let Some(new) = next {
                    // SAFETY: this rejection precedes `tc_modeset::program`
                    // and every PLANE_CTL/SURF write; the verified old image
                    // remains active and cannot address this binding.
                    if unsafe { self.gtt.release_binding(&new.binding) }.is_err() {
                        state.quarantine.push(new);
                        state.lost = true;
                    }
                }
                complete.signal_error();
                return Err(if state.lost {
                    DrmError::DeviceLost
                } else {
                    DrmError::Unsupported
                });
            }
            let Some(avi) = observed.hdmi.frames[0] else {
                if let Some(new) = next {
                    // SAFETY: the absent AVI packet is detected before the TC
                    // transaction and before any plane address write; the
                    // candidate binding has never been visible to scanout.
                    if unsafe { self.gtt.release_binding(&new.binding) }.is_err() {
                        state.quarantine.push(new);
                        state.lost = true;
                    }
                }
                complete.signal_error();
                return Err(if state.lost {
                    DrmError::DeviceLost
                } else {
                    DrmError::Unsupported
                });
            };
            let avi_words = match super::tc_modeset::avi_words(avi, target.vic) {
                Ok(words) => words,
                Err(_) => {
                    if let Some(new) = next {
                        // SAFETY: AVI validation/building happens before the
                        // TC transaction; the plane still references its
                        // verified before-image, not this new binding.
                        if unsafe { self.gtt.release_binding(&new.binding) }.is_err() {
                            state.quarantine.push(new);
                            state.lost = true;
                        }
                    }
                    complete.signal_error();
                    return Err(if state.lost {
                        DrmError::DeviceLost
                    } else {
                        DrmError::Unsupported
                    });
                }
            };
            let expected_target_avi = avi_frame(avi_words);
            let (target_pll, expected_target_phy) =
                match intel_display::dpll_mgr::icl_calc_mg_pll_state(
                    target.timing.clock_khz,
                    self.baseline.refclk,
                    self.afc_startup,
                )
                .and_then(|pll| {
                    intel_display::tc::adlp_tc_dkl_hdmi_expected_phy_state(
                        observed.phy,
                        self.port,
                        target.timing.clock_khz,
                        5,
                        intel_display::tc::Wa16011342517::Active,
                    )
                    .map(|phy| (pll, phy))
                }) {
                    Ok(plan) => plan,
                    Err(_) => {
                        if let Some(new) = next {
                            // SAFETY: target PLL/PHY arithmetic is preflighted before
                            // any TC or plane write; the verified before-image
                            // still owns scanout and cannot reference this binding.
                            if unsafe { self.gtt.release_binding(&new.binding) }.is_err() {
                                state.quarantine.push(new);
                                state.lost = true;
                            }
                        }
                        complete.signal_error();
                        return Err(if state.lost {
                            DrmError::DeviceLost
                        } else {
                            DrmError::Unsupported
                        });
                    }
                };
            let old_firmware = state.firmware.clone();
            let old_mode = state.current_mode;
            let old_surface = before;
            let old_pitch = old_firmware.plane.pitch;
            let watermark = self.watermark;
            let mut display_writes_started = false;
            let transition = translated_tc_dpll_readout(self)
                .and_then(|(enabled, manager_state)| {
                    if enabled != (old_firmware.pll.enable != 0)
                        || !super::shared_dpll::dkl_state_matches_source_readout(
                            &old_firmware.pll.state,
                            &manager_state,
                            self.afc_startup.is_some(),
                        )
                    {
                        return Err(String::from(
                            "translated shared DPLL no longer matches the active before-image",
                        ));
                    }
                    super::tc_modeset::program(
                        &self.registers,
                        &self.timer,
                        self.port,
                        &target.timing,
                        s.pitch,
                        surface,
                        Some(watermark),
                        &target_pll,
                        self.afc_startup,
                        &avi_words,
                        &self.sink_edid,
                        None,
                        None,
                        true,
                        None,
                        &mut display_writes_started,
                    )
                })
                .and_then(|()| {
                    let next_state =
                        capture(&self.registers, &self.power, self.port, self.afc_startup)
                            .map_err(|e| format!("TC modeset readback failed: {e:?}"))?;
                    let (manager_pll_on, manager_state) = translated_tc_dpll_readout(self)?;
                    if manager_pll_on != (next_state.pll.enable != 0)
                        || !super::shared_dpll::dkl_state_matches_source_readout(
                            &next_state.pll.state,
                            &manager_state,
                            self.afc_startup.is_some(),
                        )
                    {
                        return Err(String::from(
                            "firmware and translated TC DPLL enable readouts disagree after \
                             modeset",
                        ));
                    }
                    if !same_mode_state(
                        &next_state,
                        &self.baseline,
                        target,
                        surface,
                        s.pitch,
                        &target_pll,
                        self.port,
                        expected_target_avi,
                        &expected_target_phy,
                    ) {
                        return Err(String::from(
                            "TC modeset state did not match the full target image",
                        ));
                    }
                    Ok(next_state)
                });
            if display_writes_started {
                // Equivalent to source vblank off/on around this serialized
                // transaction. Hardware counter resets in forward OR rollback
                // share one new epoch, observed only after releasing this lock.
                state.counter_epoch += 1;
                state.frame_progress = None;
            }
            match transition {
                Ok(next_state) => {
                    state.firmware = next_state;
                    state.current_mode = target;
                }
                Err(original) if !display_writes_started => {
                    // Preflight and the HDA retirement gate precede every
                    // display write. The old scanout remains authoritative,
                    // so an audio-only refusal must not make the display
                    // worker lost or run a destructive rollback against a
                    // still-active link.
                    if let Some(new) = next {
                        // SAFETY: `tc_modeset::program` reports this stage
                        // only before its first PLANE_CTL/SURF or link write;
                        // the existing scanout remains the sole DMA owner.
                        if unsafe { self.gtt.release_binding(&new.binding) }.is_err() {
                            state.quarantine.push(new);
                            state.lost = true;
                        }
                    }
                    complete.signal_error();
                    axlog::warn!("intel-tc-modeset: refused before display writes: {original}");
                    return Err(if state.lost {
                        DrmError::DeviceLost
                    } else {
                        DrmError::Busy
                    });
                }
                Err(original) => {
                    let old_avi_frame = old_firmware.hdmi.frames[0];
                    let old_avi = old_avi_frame
                        .and_then(|raw| super::tc_modeset::avi_words_preserve(raw).ok());
                    let mut rollback_display_writes_started = false;
                    let recovered = old_avi.is_some_and(|old_avi| {
                        super::tc_modeset::program(
                            &self.registers,
                            &self.timer,
                            self.port,
                            &old_mode.timing,
                            old_pitch,
                            old_surface,
                            Some(watermark),
                            &old_firmware.pll.state,
                            self.afc_startup,
                            &old_avi,
                            &self.sink_edid,
                            Some(old_firmware.pipe.misc.raw),
                            Some(old_firmware.plane.ctl),
                            false,
                            Some(&old_firmware.phy),
                            &mut rollback_display_writes_started,
                        )
                        .and_then(|()| {
                            let restored =
                                capture(&self.registers, &self.power, self.port, self.afc_startup)
                                    .map_err(|e| format!("TC rollback readback failed: {e:?}"))?;
                            let (manager_pll_on, manager_state) = translated_tc_dpll_readout(self)?;
                            if manager_pll_on != (restored.pll.enable != 0)
                                || !super::shared_dpll::dkl_state_matches_source_readout(
                                    &restored.pll.state,
                                    &manager_state,
                                    self.afc_startup.is_some(),
                                )
                            {
                                return Err(String::from(
                                    "firmware and translated TC DPLL enable readouts disagree \
                                     after rollback",
                                ));
                            }
                            if !same_mode_state(
                                &restored,
                                &self.baseline,
                                old_mode,
                                old_surface,
                                old_pitch,
                                &old_firmware.pll.state,
                                self.port,
                                old_avi_frame.unwrap(),
                                &old_firmware.phy,
                            ) {
                                return Err(String::from(
                                    "TC rollback did not restore the old image",
                                ));
                            }
                            Ok(())
                        })
                        .is_ok()
                    });
                    if let Some(new) = next {
                        if recovered {
                            // SAFETY: rollback readback matched the old full
                            // image, and `tc_modeset::program` proved its saved
                            // SURFLIVE address across two fresh frames. The
                            // display engine no longer references this new
                            // mapping.
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
                    axlog::warn!(
                        "intel-tc-modeset: {}; {}",
                        original,
                        if recovered {
                            "ROLLBACK_MMIO_VERIFIED"
                        } else {
                            "ROLLBACK_FAILED; DMA owners quarantined"
                        }
                    );
                    return Err(if recovered && !state.lost {
                        DrmError::Busy
                    } else {
                        DrmError::DeviceLost
                    });
                }
            }
        }
        if let Some(new) = next
            && let Some(old) = state.current.replace(new)
        {
            // SAFETY: the preceding latch/modeset path proved the new
            // SURFLIVE address followed by a second fresh hardware frame, so
            // this replaced scanout binding is no longer referenced.
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
        let bios = intel_display::intel_bios::intel_bios_init(
            Some(vbt.data()),
            13,
            intel_display::dmc::DmcPlatform::AlderLakeN,
            false,
            true,
            false,
            &[
                Port::A,
                Port::B,
                Port::C,
                Port::D,
                Port::E,
                Port::F,
                Port::Tc1,
                Port::Tc2,
            ],
        );
        let afc_startup = bios
            .vbt
            .as_ref()
            .map(|vbt| vbt.afc_startup_override())
            .transpose()?
            .flatten();
        let route = bios
            .definitions
            .as_ref()
            .and_then(|definitions| {
                definitions.encoder(if port == TcPort::Tc1 {
                    Port::Tc1
                } else {
                    Port::Tc2
                })
            })
            .ok_or(Error::Refused)?;
        if !route.supports_hdmi()
            || route.usb_type_c
            || route.thunderbolt
            || route.lspcon
            || route.dynamic_port_over_tc
            || route.hdmi_level_shift != 5
            || route.gmbus_pin() != Some(if port == TcPort::Tc1 { 9 } else { 10 })
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
        super::dmc::display_power_ready(window, intel_display::dmc::DmcPlatform::AlderLakeN);
        let admitted = (|| {
            let watermark =
                super::power::read_source_watermark_config(&window, &super::gmbus::MonotonicTimer)
                    .map_err(|error| {
                        axlog::warn!(
                            "intel-fastboot: source watermark profile unavailable: {error}"
                        );
                        Error::Refused
                    })?;
            let first = capture(&window, &pin, port, afc_startup)?;
            // Reuse the translated shared-DPLL manager's generic TC1/TC2
            // hardware-state dispatcher as an independent read-only check of
            // the firmware DKL PLL readout.  This is not yet the allocator or
            // enable/disable owner: the existing TC transaction still owns
            // its indivisible modeset/rollback sequence below.
            let dpll_identity = super::shared_dpll::AdlNIdentity::verify(
                info.vendor_id,
                info.device_id,
                info.revision,
            )
            .map_err(|error| {
                axlog::warn!("intel-fastboot: shared DPLL identity refused: {error:?}");
                Error::Refused
            })?;
            let mut shared_dpll = super::shared_dpll::SharedDpllState::new(dpll_identity, 0);
            let mut dpll_power =
                super::shared_dpll::PinnedDpllPower::new(&pin, dpll_identity, first.refclk)
                    .map_err(|error| {
                        axlog::warn!(
                            "intel-fastboot: shared DPLL power context refused: {error:?}"
                        );
                        Error::Refused
                    })?;
            shared_dpll
                .init(&window, &super::gmbus::MonotonicTimer, &mut dpll_power)
                .map_err(|error| {
                    axlog::warn!("intel-fastboot: shared DPLL manager init refused: {error:?}");
                    Error::Refused
                })?;
            let dpll_index = (3 + port.index()) as usize;
            let (manager_pll_on, manager_state) = shared_dpll
                .get_hw_state(
                    &window,
                    &super::gmbus::MonotonicTimer,
                    &mut dpll_power,
                    dpll_index,
                )
                .map_err(|error| {
                    axlog::warn!(
                        "intel-fastboot: translated shared DPLL readout refused: {error:?}"
                    );
                    Error::Refused
                })?;
            if manager_pll_on != (first.pll.enable != 0)
                || !super::shared_dpll::dkl_state_matches_source_readout(
                    &first.pll.state,
                    &manager_state,
                    afc_startup.is_some(),
                )
            {
                axlog::warn!(
                    "intel-fastboot: firmware and translated TC DPLL enable readouts disagree: \
                     firmware={} manager={}",
                    first.pll.enable,
                    manager_pll_on
                );
                return Err(Error::Refused);
            }
            if first.plane.pitch != (first.plane.width * 4).div_ceil(64) * 64 {
                return Err(Error::Refused);
            }
            ownership(&gtt, &first, &boot, aperture, stolen, &allocatable)?;
            let ddc = if port == TcPort::Tc1 {
                super::gmbus::Pin::Tc1
            } else {
                super::gmbus::Pin::Tc2
            };
            let sink_edid = read_route_edid(&window, &super::gmbus::MonotonicTimer, ddc)?;
            let second = capture(&window, &pin, port, afc_startup)?;
            if first != second {
                return Err(Error::Refused);
            }
            let (modes, preferred, current) = native_modes(&sink_edid, &first)?;
            Ok((
                first,
                modes,
                preferred,
                current,
                sink_edid,
                watermark,
                shared_dpll,
            ))
        })();
        match admitted {
            Ok((f, modes, preferred, current, edid, watermark, shared_dpll)) => Ok((
                pin,
                port,
                f,
                modes,
                preferred,
                current,
                edid,
                afc_startup,
                watermark,
                shared_dpll,
                info,
            )),
            Err(e) => {
                pin.restore(&window)?;
                Err(e)
            }
        }
    };
    let (
        power,
        port,
        firmware,
        modes,
        preferred,
        current_mode,
        sink_edid,
        afc_startup,
        watermark,
        shared_dpll,
        info,
    ) = setup().map_err(message)?;
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
        shared_dpll: Mutex::new(shared_dpll),
        baseline: firmware.clone(),
        modes,
        preferred: preferred.kms,
        sink_edid,
        afc_startup,
        watermark,
        port,
        pci,
        irq_event_sequence: AtomicU32::new(0),
        state: Mutex::new(State {
            firmware,
            current_mode,
            connected: true,
            last_hpd_poll: 0,
            last_hpd_irq: None,
            hpd_good_samples: 0,
            hpd_bad_samples: 0,
            irq_fault_reported: false,
            unsupported_sink_reported: false,
            current: None,
            quarantine: Vec::new(),
            unbound_quarantine: Vec::new(),
            lost: false,
            frame_progress: None,
            counter_epoch: 0,
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
    if let Err(e) = crate::drm::register_primary_device(device.clone()) {
        power.restore(&window).map_err(message)?;
        return Err(format!("fastboot KMS registration failed: {e}"));
    }
    match super::pci::Ecam::platform() {
        Some(mut ecam) => match super::irq::install_n305(&mut ecam, bdf, port, window) {
            Ok(()) => axlog::info!(
                "intel-irq: owned single MSI for Pipe-A vblank and selected {port:?} HPD; \
                 PIPEFRAME remains the KMS sequence source"
            ),
            Err(error) => axlog::warn!(
                "intel-irq: dedicated MSI setup refused/unverified: {error}; display remains \
                 active with PIPEFRAME and bounded HPD polling"
            ),
        },
        None => axlog::warn!(
            "intel-irq: PCI ECAM unavailable; display remains active with PIPEFRAME and bounded \
             HPD polling"
        ),
    }
    match super::audio::after_link_enabled(
        &adapter.registers,
        &adapter.timer,
        port,
        current_mode.timing.clock_khz,
        &adapter.sink_edid,
    ) {
        Ok(super::audio::LinkAudioStatus::Enabled) => {}
        Ok(super::audio::LinkAudioStatus::Unavailable(reason)) => {
            axlog::warn!(
                "intel-hdmi-audio: initial display remains active, audio is unavailable: {reason}"
            );
        }
        Err(error) => {
            // Display is already stable. An unknown HDA/ELD state keeps its
            // own quarantine and does not tear down a working TC scanout.
            axlog::error!(
                "intel-hdmi-audio: initial handoff is unverified; display link/power retained: \
                 {error}"
            );
        }
    }
    if let Err(error) = device.start_runtime_workers() {
        axlog::warn!(
            "intel-hpd: TC connector reconciliation worker unavailable; a later KMS request may \
             retry: {error:?}"
        );
    }
    Ok(format!(
        "intel-fastboot: TC1/TC2 legacy-HDMI KMS registered preferred={preferred:?} \
         firmware_mode={:?}; supported_modes={:?}; firmware WM/DDB retained; TC modeset rollback \
         and pageflip completion require fresh SURFLIVE/hardware frames; 未在硬件上验证",
        current_mode.kms,
        adapter
            .modes
            .iter()
            .map(|mode| mode.kms)
            .collect::<Vec<_>>()
    ))
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, collections::BTreeMap, vec};
    use core::sync::atomic::AtomicU64;

    use super::{super::gtt::PageTable, *};

    fn source_watermark_fixture() -> super::super::pipe::WatermarkConfig {
        super::super::pipe::WatermarkConfig {
            display_ver: 13,
            latencies: [2, 4, 6, 8, 14, 16, 0, 0],
            num_levels: 6,
            sagv_block_time_us: 0,
        }
    }

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
        fail_offset: Option<u32>,
        stall: bool,
        fail_surface: bool,
        frame_pixel_pending: bool,
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
                (0x70000, 0),
                (0x70028, 0),
                (0x70024, 0),
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
                (0x46140, 6 << 28),
                (0x164280, 0),
                (0x51004, 0),
                (0x46038, 0xcc000000),
                (0x1010a0, 0x44332211),
                (0x60200, 1 << 12),
                (0x650c0, 0),
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
                (0x70044, 0),
            ] {
                words.insert(r, v);
            }
            let mut avi_packet = [0u8; 17];
            avi_packet[0] = 0x82;
            avi_packet[1] = 2;
            avi_packet[2] = 13;
            avi_packet[3] = intel_display::hdmi_packet::hdmi_infoframe_checksum(&avi_packet);
            let mut avi_raw = [0u8; 32];
            avi_raw[..3].copy_from_slice(&avi_packet[..3]);
            avi_raw[4..18].copy_from_slice(&avi_packet[3..]);
            for (n, bytes) in avi_raw.chunks_exact(4).enumerate() {
                words.insert(
                    0x60220 + n as u32 * 4,
                    u32::from_le_bytes(bytes.try_into().unwrap()),
                );
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
                    fail_offset: None,
                    stall: false,
                    fail_surface: false,
                    frame_pixel_pending: false,
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
            if r == 0x70000 && s.frames {
                let line = s.words.get_mut(&r)?;
                *line = (*line + 100) % 1125;
                return Some(*line);
            }
            if r == 0x70040 && s.frames {
                let pending = core::mem::replace(&mut s.frame_pixel_pending, false);
                let v = s.words.get_mut(&r)?;
                if !pending {
                    *v = v.wrapping_add(1);
                }
                return Some(*v);
            }
            if r == 0x70044 && s.frames {
                s.frame_pixel_pending = true;
                return s.words.get(&r).copied();
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
            let fail = s.fail == Some(s.writes)
                || s.fail_offset == Some(r)
                || (r == 0x7019c && s.fail_surface);
            if s.fail_offset == Some(r) {
                s.fail_offset = None;
            }
            if r == 0x7019c {
                s.fail_surface = false;
            }
            s.log.push((r, v));
            if (0x168000..0x169000).contains(&r) {
                let bank = s.words.get(&0x1010a0).copied().unwrap_or(0) & 15;
                s.dkl.insert(bank * 0x1000 + r - 0x168000, v);
            } else if r == 0x46038 {
                let mut value = v & !((1 << 30) | (1 << 26));
                if v & (1 << 31) != 0 {
                    value |= 1 << 30;
                }
                if v & (1 << 27) != 0 {
                    value |= 1 << 26;
                }
                s.words.insert(r, value);
            } else if r == 0x70008 {
                s.words.insert(
                    r,
                    if v & (1 << 31) != 0 {
                        v | (1 << 30)
                    } else {
                        v & !(1 << 30)
                    },
                );
            } else if r == 0x64300 {
                s.words.insert(
                    r,
                    if v & (1 << 31) != 0 {
                        v & !(1 << 7)
                    } else {
                        v | (1 << 7)
                    },
                );
            } else if [0x45404, 0x45454, 0x45444].contains(&r) {
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
    fn test_shared_dpll(
        r: &Model,
        pin: &PowerPin,
        refclk: u32,
    ) -> super::super::shared_dpll::SharedDpllState {
        let identity = super::super::shared_dpll::AdlNIdentity::verify(0x8086, 0x46d0, 0).unwrap();
        let mut manager = super::super::shared_dpll::SharedDpllState::new(identity, 0);
        let mut power =
            super::super::shared_dpll::PinnedDpllPower::new(pin, identity, refclk).unwrap();
        manager
            .init(r, &Timer(Arc::new(AtomicU64::new(0))), &mut power)
            .unwrap();
        manager
    }
    fn native() -> (
        Arc<Native<Model, Timer>>,
        Model,
        super::super::gtt::mock::MockPageTable,
    ) {
        let r = Model::new();
        let power = PowerPin::acquire(&r, TcPort::Tc1).unwrap();
        let firmware = capture(&r, &power, TcPort::Tc1, None).unwrap();
        assert_eq!(firmware, capture(&r, &power, TcPort::Tc1, None).unwrap());
        let timing = firmware_timing(&firmware).unwrap();
        let current_mode = NativeMode {
            timing,
            kms: mode(&firmware).unwrap(),
            vic: 16,
        };
        let target_timing = crate::drm::modes::Mode::from_blanking(
            148_500,
            64,
            2135,
            16,
            24,
            64,
            1061,
            2,
            4,
            crate::drm::modes::ModeFlags::NONE,
            crate::drm::modes::TimingSource::Firmware,
        )
        .with_polarity(timing.hsync_positive, timing.vsync_positive);
        let target_mode = NativeMode {
            timing: target_timing,
            kms: drm_mode(target_timing),
            vic: 16,
        };
        let array = super::super::gtt::mock::MockPageTable::new(65536);
        for i in 0..4 {
            array.write(0x200000 / 4096 + i, 0x80000001 + (i as u64) * 4096);
        }
        let gtt = Arc::new(Gtt::over(Box::new(array.clone())).unwrap());
        let shared_dpll = test_shared_dpll(&r, &power, firmware.refclk);
        let adapter = Arc::new(Native {
            registers: r.clone(),
            timer: Timer(Arc::new(AtomicU64::new(0))),
            gtt,
            power,
            shared_dpll: Mutex::new(shared_dpll),
            baseline: firmware.clone(),
            modes: vec![current_mode, target_mode],
            preferred: current_mode.kms,
            sink_edid: Vec::new(),
            afc_startup: None,
            watermark: source_watermark_fixture(),
            port: TcPort::Tc1,
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
            irq_event_sequence: AtomicU32::new(0),
            state: Mutex::new(State {
                firmware,
                current_mode,
                connected: true,
                last_hpd_poll: 0,
                last_hpd_irq: None,
                hpd_good_samples: 0,
                hpd_bad_samples: 0,
                irq_fault_reported: false,
                unsupported_sink_reported: false,
                current: None,
                quarantine: Vec::new(),
                unbound_quarantine: Vec::new(),
                lost: false,
                frame_progress: None,
                counter_epoch: 0,
            }),
        });
        (adapter, r, array)
    }
    fn expected_phy(
        before: &intel_display::tc::DklPhyState,
        port: TcPort,
        clock: u32,
    ) -> intel_display::tc::DklPhyState {
        intel_display::tc::adlp_tc_dkl_hdmi_expected_phy_state(
            *before,
            port,
            clock,
            5,
            intel_display::tc::Wa16011342517::Active,
        )
        .unwrap()
    }

    fn native_4k30_to_1080p60() -> (
        Arc<Native<Model, Timer>>,
        Model,
        super::super::gtt::mock::MockPageTable,
    ) {
        let r = Model::new();
        let pair = |a: u32, b: u32| (a - 1) | ((b - 1) << 16);
        for (offset, value) in [
            (0x60000, pair(3840, 4400)),
            (0x60004, pair(3840, 4400)),
            (0x60008, pair(4016, 4104)),
            (0x6000c, pair(2160, 2250)),
            (0x60010, pair(2160, 2250)),
            (0x60014, pair(2168, 2178)),
            (0x6001c, (3839 << 16) | 2159),
            (0x70190, pair(3840, 2160)),
            (0x70188, 15_360 / 64),
        ] {
            r.set(offset, value);
        }
        let baseline_pll =
            intel_display::dpll_mgr::icl_calc_mg_pll_state(297_000, 24_000, None).unwrap();
        {
            let mut model = r.inner.lock();
            for (offset, value) in [
                (0x212c, baseline_pll.refclkin_ctl),
                (0x20d4, baseline_pll.hsclkctl),
                (0x20d8, baseline_pll.coreclkctl1),
                (0x2200, baseline_pll.div0),
                (0x2204, baseline_pll.div1),
                (0x2210, baseline_pll.ssc),
                (0x2214, baseline_pll.bias),
                (0x2218, baseline_pll.tdc_coldst_bias),
            ] {
                model.dkl.insert(offset, value);
            }
        }

        let power = PowerPin::acquire(&r, TcPort::Tc1).unwrap();
        let firmware = capture(&r, &power, TcPort::Tc1, None).unwrap();
        assert_eq!(firmware, capture(&r, &power, TcPort::Tc1, None).unwrap());
        let current_timing = firmware_timing(&firmware).unwrap();
        assert_eq!(
            (
                current_timing.clock_khz,
                current_timing.hdisplay,
                current_timing.htotal
            ),
            (297_000, 3840, 4400)
        );
        assert_eq!(
            (
                firmware.plane.width,
                firmware.plane.height,
                firmware.plane.pitch
            ),
            (3840, 2160, 15_360)
        );
        let current_mode = NativeMode {
            timing: current_timing,
            kms: mode(&firmware).unwrap(),
            vic: 95,
        };
        let target_timing = crate::drm::modes::CTA_VIC_TIMINGS
            .iter()
            .find(|entry| entry.vic == 16)
            .unwrap()
            .mode;
        let target_mode = NativeMode {
            timing: target_timing,
            kms: drm_mode(target_timing),
            vic: 16,
        };
        assert_eq!(
            (
                target_timing.clock_khz,
                target_timing.hdisplay,
                target_timing.htotal
            ),
            (148_500, 1920, 2200)
        );

        // The old firmware surface has a complete simulated linear-GGTT
        // backing, not the four-page dummy used by the unrelated tiny fixture.
        let array = super::super::gtt::mock::MockPageTable::new(65536);
        for i in 0..firmware.plane.main_size.div_ceil(4096) as usize {
            array.write(0x200000 / 4096 + i, 0x80000001 + (i as u64) * 4096);
        }
        let gtt = Arc::new(Gtt::over(Box::new(array.clone())).unwrap());
        let shared_dpll = test_shared_dpll(&r, &power, firmware.refclk);
        let adapter = Arc::new(Native {
            registers: r.clone(),
            timer: Timer(Arc::new(AtomicU64::new(0))),
            gtt,
            power,
            shared_dpll: Mutex::new(shared_dpll),
            baseline: firmware.clone(),
            modes: vec![current_mode, target_mode],
            preferred: current_mode.kms,
            sink_edid: Vec::new(),
            afc_startup: None,
            watermark: source_watermark_fixture(),
            port: TcPort::Tc1,
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
            irq_event_sequence: AtomicU32::new(0),
            state: Mutex::new(State {
                firmware,
                current_mode,
                connected: true,
                last_hpd_poll: 0,
                last_hpd_irq: None,
                hpd_good_samples: 0,
                hpd_bad_samples: 0,
                irq_fault_reported: false,
                unsupported_sink_reported: false,
                current: None,
                quarantine: Vec::new(),
                unbound_quarantine: Vec::new(),
                lost: false,
                frame_progress: None,
                counter_epoch: 0,
            }),
        });
        (adapter, r, array)
    }
    fn edid_with_1080p60_vic16() -> Vec<u8> {
        edid_with_1080p60_vic16_and_max_tmds(None)
    }
    fn edid_with_1080p60_vic16_and_max_tmds(max_tmds_5mhz: Option<u8>) -> Vec<u8> {
        let mut edid = crate::drm::intel::gmbus::tests::valid_edid(1).to_vec();
        let base = edid.get_mut(..128).unwrap();
        let checksum = base[..127]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        base[127] = 0u8.wrapping_sub(checksum);

        let mut cta = [0u8; 128];
        cta[0] = 0x02;
        cta[1] = 0x03;
        if let Some(max_tmds_5mhz) = max_tmds_5mhz {
            cta[2] = 14; // HDMI VSDB and one-entry video block
            cta[4] = 0x67; // 7-byte vendor-specific payload
            cta[5..12].copy_from_slice(&[0x03, 0x0c, 0x00, 0x00, 0x00, 0x00, max_tmds_5mhz]);
            cta[12] = 0x41; // one-entry video data block
            cta[13] = 16; // 1080p60 VIC 16
        } else {
            cta[2] = 6; // data block collection ends before the checksum
            cta[4] = 0x41; // one-entry video data block
            cta[5] = 16; // 1080p60 VIC 16
        }
        let checksum = cta[..127]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        cta[127] = 0u8.wrapping_sub(checksum);
        edid.extend_from_slice(&cta);
        edid
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
            mode: a.preferred,
            damage: None,
        }
    }
    fn scanout_for_full_mode(a: &Native<Model, Timer>, mode: NativeMode) -> Scanout {
        let width = mode.kms.width;
        let height = mode.kms.height;
        let pitch = pitch_for(width).unwrap();
        let backing_size = u64::from(pitch) * u64::from(height);
        let backing = a
            .create_dumb(
                DumbRequest {
                    width,
                    height,
                    bpp: 32,
                },
                pitch,
                backing_size,
                Arc::new(()),
            )
            .unwrap();
        Scanout {
            backing,
            width,
            height,
            pitch,
            bpp: 32,
            format: intel_display::universal_plane::XRGB8888,
            framebuffer_width: width,
            framebuffer_height: height,
            backing_size,
            framebuffer_offset: 0,
            offset: 0,
            source_x: 0,
            source_y: 0,
            mode: mode.kms,
            damage: None,
        }
    }
    #[test]
    fn native_modes_keeps_firmware_preferred_and_filters_unproven_wm_profile() {
        let (a, ..) = native_4k30_to_1080p60();
        let edid = edid_with_1080p60_vic16();
        let (modes, preferred, current) = native_modes(&edid, &a.baseline).unwrap();
        assert_eq!(current.vic, 95);
        assert_eq!(
            preferred, current,
            "native exposure must not force a cold mode transition"
        );
        assert_eq!(modes.len(), 2);
        assert_eq!(modes[0], current);
        assert_eq!(modes[1].vic, 16);
        assert_eq!(modes[1].timing.clock_khz, 148_500);

        let mut unproven_baseline = a.baseline.clone();
        unproven_baseline.plane.width -= 1;
        let (modes, preferred, current) = native_modes(&edid, &unproven_baseline).unwrap();
        assert_eq!(preferred, current);
        assert_eq!(modes, vec![current], "inadmissible target stays hidden");

        let capped_edid = edid_with_1080p60_vic16_and_max_tmds(Some(20)); // 100 MHz
        let (modes, preferred, current) = native_modes(&capped_edid, &a.baseline).unwrap();
        assert_eq!(preferred, current);
        assert_eq!(modes, vec![current], "sink-limited target stays hidden");
    }
    #[test]
    fn translated_tc_dpll_readout_matches_firmware_capture() {
        let (adapter, ..) = native();
        let firmware_enabled = adapter.state.lock().firmware.pll.enable != 0;
        let (enabled, manager_state) = translated_tc_dpll_readout(&adapter).unwrap();
        assert_eq!(enabled, firmware_enabled);
        assert!(super::super::shared_dpll::dkl_state_matches_source_readout(
            &adapter.baseline.pll.state,
            &manager_state,
            adapter.afc_startup.is_some(),
        ));
    }
    #[test]
    fn translated_tc_dpll_readout_rejects_enable_bit_drift() {
        let (adapter, registers, ..) = native();
        let firmware_enabled = adapter.state.lock().firmware.pll.enable;
        assert_ne!(firmware_enabled & (1 << 31), 0);
        let mut model = registers.inner.lock();
        let enable = model.words.get(&0x46038).copied().unwrap();
        model.words.insert(0x46038, enable & !(1 << 31));
        drop(model);
        assert!(!translated_tc_dpll_readout(&adapter).unwrap().0);
    }
    #[test]
    fn translated_tc_dpll_readout_refuses_lost_phy_power_pin() {
        let (adapter, registers, ..) = native();
        let mut model = registers.inner.lock();
        let request = model.words.get(&0x45444).copied().unwrap();
        model.words.insert(0x45444, request & !(1 << 7));
        drop(model);
        assert!(translated_tc_dpll_readout(&adapter).is_err());
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
    fn dpll_pin_context_is_read_only_and_rechecks_clock_dc_state_and_route() {
        let r = Model::new();
        let pin = PowerPin::acquire(&r, TcPort::Tc1).unwrap();
        let writes = r.inner.lock().writes;
        assert!(pin.verify_dpll_context(&r, TcPort::Tc1, 24_000).is_ok());
        assert_eq!(r.inner.lock().writes, writes, "verification is read-only");
        assert!(pin.verify_dpll_context(&r, TcPort::Tc2, 24_000).is_err());
        assert!(pin.verify_dpll_context(&r, TcPort::Tc1, 19_200).is_err());

        r.set(0x45504, 1 << 30);
        assert!(pin.verify_dpll_context(&r, TcPort::Tc1, 24_000).is_err());
        r.set(0x45504, 0);

        let pin_req = 2 << 2;
        let current = read(&r, 0x45404).unwrap();
        r.set(0x45404, current & !pin_req);
        assert!(pin.verify_dpll_context(&r, TcPort::Tc1, 24_000).is_err());
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
                &a.baseline,
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
                &a.baseline,
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
                &a.baseline,
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
                &a.baseline,
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
    fn tc_modeset_publishes_only_after_full_image_and_fresh_frame_readback() {
        let _context = crate::test_support::scheduler_test_context();
        let (probe, probe_registers, _) = native_4k30_to_1080p60();
        let probe_baseline = probe.state.lock().firmware.clone();
        let probe_target = probe.modes[1];
        let initial_audio =
            super::super::audio::before_link_disable(&probe_registers, &probe.timer, probe.port);
        assert!(
            initial_audio.is_ok(),
            "initial unowned HDA proof: {initial_audio:?}"
        );
        let probe_avi = super::super::tc_modeset::avi_words(
            probe_baseline.hdmi.frames[0].unwrap(),
            probe_target.vic,
        )
        .unwrap();
        let probe_pll = intel_display::dpll_mgr::icl_calc_mg_pll_state(
            probe_target.timing.clock_khz,
            probe_baseline.refclk,
            None,
        )
        .unwrap();
        let probe_pitch = pitch_for(probe_target.kms.width).unwrap();
        let probe_surface = probe_baseline.plane.surface();
        let mut probe_display_writes_started = false;
        let probe_result = super::super::tc_modeset::program(
            &probe_registers,
            &probe.timer,
            probe.port,
            &probe_target.timing,
            probe_pitch,
            probe_surface,
            None,
            &probe_pll,
            None,
            &probe_avi,
            &[],
            None,
            None,
            true,
            None,
            &mut probe_display_writes_started,
        );
        assert!(probe_result.is_ok(), "direct TC program: {probe_result:?}");
        let probe_observed = capture(
            &probe_registers,
            &probe.power,
            probe.port,
            probe.afc_startup,
        )
        .unwrap();
        assert!(
            same_mode_state(
                &probe_observed,
                &probe.baseline,
                probe_target,
                probe_surface,
                probe_pitch,
                &probe_pll,
                probe.port,
                avi_frame(probe_avi),
                &expected_phy(
                    &probe_baseline.phy,
                    probe.port,
                    probe_target.timing.clock_khz
                ),
            ),
            "TC readback mismatch: {probe_observed:#?}"
        );
        let (a, r, array) = native_4k30_to_1080p60();
        let target = a.modes[1];
        assert_eq!(
            capture(&r, &a.power, a.port, a.afc_startup).unwrap(),
            a.state.lock().firmware
        );
        let result = a.present(scanout_for_full_mode(&a, target));
        let writes = r.inner.lock().log.clone();
        let mmio_writes = writes
            .iter()
            .filter(|(offset, _)| *offset != 0x1010a0)
            .copied()
            .collect::<Vec<_>>();
        assert!(
            result.is_ok(),
            "TC modeset error={:?}, lost={}, MMIO={:?}",
            result.as_ref().err(),
            a.state.lock().lost,
            mmio_writes
        );
        let done = result.unwrap();
        assert!(done.is_signaled() && !done.is_failed());
        let state = a.state.lock();
        assert_eq!(state.current_mode, target);
        let surface = u32::try_from(state.current.as_ref().unwrap().binding.address).unwrap();
        let pitch = target.kms.width * 4;
        assert!(retained_watermark_budget(&a.baseline, target, pitch));
        let observed = capture(&r, &a.power, a.port, a.afc_startup).unwrap();
        assert!(
            same_mode_state(
                &observed,
                &a.baseline,
                target,
                surface,
                pitch,
                &observed.pll.state,
                a.port,
                avi_frame(
                    super::super::tc_modeset::avi_words(
                        a.baseline.hdmi.frames[0].unwrap(),
                        target.vic,
                    )
                    .unwrap()
                ),
                &expected_phy(&a.baseline.phy, a.port, target.timing.clock_khz),
            )
        );
        assert!(
            r.inner
                .lock()
                .log
                .iter()
                .any(|(offset, _)| *offset == 0x46140)
        );
        let baseline_pages = a.baseline.plane.main_size.div_ceil(4096) as usize;
        assert_eq!(array.raw(512), 0x80000001, "first retained firmware PTE");
        assert_eq!(
            array.raw(512 + baseline_pages - 1),
            0x80000001 + ((baseline_pages - 1) as u64) * 4096,
            "last retained firmware PTE"
        );
    }

    #[test]
    fn landed_tc_clock_select_failure_restores_complete_firmware_modeset_image() {
        let _context = crate::test_support::scheduler_test_context();
        for failed_offset in [0x46140, 0x1682c4, 0x1682c8, 0x168d00] {
            let (a, r, array) = native_4k30_to_1080p60();
            let initial_audio = super::super::audio::before_link_disable(&r, &a.timer, a.port);
            assert!(
                initial_audio.is_ok(),
                "initial unowned HDA proof: {initial_audio:?}"
            );
            let original = a.state.lock().firmware.clone();
            r.inner.lock().fail_offset = Some(failed_offset);
            assert_eq!(
                a.present(scanout_for_full_mode(&a, a.modes[1])).err(),
                Some(DrmError::Busy)
            );
            assert!(!a.state.lock().lost);
            let state = a.state.lock();
            assert_eq!(state.current_mode, a.modes[0]);
            assert!(state.current.is_none());
            assert!(state.quarantine.is_empty());
            let old_surface = original.plane.surface();
            let observed = capture(&r, &a.power, a.port, a.afc_startup).unwrap();
            assert_eq!(observed.pipe, original.pipe, "pipe");
            assert_eq!(observed.plane, original.plane, "plane");
            assert_eq!(observed.ddi, original.ddi, "DDI");
            assert_eq!(observed.pll, original.pll, "PLL");
            assert_eq!(observed.phy, original.phy, "PHY");
            assert_eq!(observed.fia, original.fia, "FIA");
            assert_eq!(observed.clock, original.clock, "TC clock");
            assert_eq!(
                observed.trans_clock, original.trans_clock,
                "transcoder clock"
            );
            assert_eq!(observed.hdmi, original.hdmi, "HDMI packets");
            assert_eq!(observed.wm, original.wm, "watermarks");
            assert_eq!(observed.ddb, original.ddb, "DDB");
            assert_eq!(observed.dbuf, original.dbuf, "DBUF");
            assert_eq!(observed.color, original.color, "color");
            assert_eq!(observed.scalers, original.scalers, "scalers");
            assert_eq!(observed.refclk, original.refclk, "refclk");
            assert_eq!(observed.pixel_clock, original.pixel_clock, "pixel clock");
            assert_eq!(observed.afc_startup, original.afc_startup, "AFC");
            assert!(same_mode_state(
                &observed,
                &a.baseline,
                a.modes[0],
                old_surface,
                original.plane.pitch,
                &original.pll.state,
                a.port,
                original.hdmi.frames[0].unwrap(),
                &original.phy,
            ));
            assert_eq!(read(&r, 0x46140), Ok(original.trans_clock));
            let baseline_pages = original.plane.main_size.div_ceil(4096) as usize;
            assert_eq!(array.raw(512), 0x80000001);
            assert_eq!(
                array.raw(512 + baseline_pages - 1),
                0x80000001 + ((baseline_pages - 1) as u64) * 4096,
                "last retained firmware PTE"
            );
        }
    }

    #[test]
    fn hpd_reports_stable_physical_state_and_keeps_power_when_audio_is_unverified() {
        let (a, r, _) = native();
        let mut state = a.state.lock();
        let mut audio_calls = Vec::new();
        assert!(
            hpd_config_transition(&mut state, false, a.preferred, |connected| {
                audio_calls.push(connected);
                Ok(())
            },)
            .is_none()
        );
        assert!(audio_calls.is_empty(), "one bad EDID sample is not stable");
        assert!(state.connected);

        // The two-sample unplug still reaches KMS even when HDA retirement
        // cannot be proven. Audio remains quarantined by its hook, while the
        // display state stays live enough to finish accepted frame retirement.
        let (disconnected, disconnect_audio_error) =
            hpd_config_transition(&mut state, false, a.preferred, |connected| {
                audio_calls.push(connected);
                Err(String::from("HDA stop not proven"))
            })
            .unwrap();
        assert_eq!(audio_calls, vec![false]);
        assert!(!disconnected.connected);
        assert_eq!(
            disconnect_audio_error.as_deref(),
            Some("HDA stop not proven")
        );
        assert!(!state.connected && !state.lost);

        // A stable reconnect is likewise reported even when HDA cannot be
        // re-enabled; video remains connected and the audio error is distinct.
        assert!(hpd_config_transition(&mut state, true, a.preferred, |_| Ok(()),).is_none());
        let (reconnected, reconnect_audio_error) =
            hpd_config_transition(&mut state, true, a.preferred, |connected| {
                audio_calls.push(connected);
                Err(String::from("HDA route quarantined"))
            })
            .unwrap();
        assert_eq!(audio_calls, vec![false, true]);
        assert!(reconnected.connected);
        assert_eq!(
            reconnect_audio_error.as_deref(),
            Some("HDA route quarantined")
        );
        assert!(state.connected && !state.lost);
        drop(state);
        assert!(a.power.held(&r).is_ok());
    }

    #[test]
    fn modeset_counter_epoch_rebases_without_manufacturing_vblank_events() {
        let _context = crate::test_support::scheduler_test_context();
        let (a, r, _) = native_4k30_to_1080p60();
        let dev = crate::drm::DrmDevice::new(a.clone(), 1, 2, 3, 4);
        let target_scanout = scanout_for_full_mode(&a, a.modes[1]);
        // Check preflight exhaustion on the outgoing 4K->1080 transition;
        // reusing this full-size target avoids an unrelated second 32MiB
        // allocation in the bounded host physical-memory test pool.
        a.state.lock().counter_epoch = u64::MAX;
        let writes = r.inner.lock().writes;
        assert_eq!(
            a.present(target_scanout.clone()).err(),
            Some(DrmError::Overflow)
        );
        assert_eq!(r.inner.lock().writes, writes);
        a.state.lock().counter_epoch = 0;
        r.set(0x70040, 100);
        dev.advance_vblank().unwrap();
        dev.advance_vblank().unwrap();
        let sequence = dev.vblank_sequence();
        assert_eq!(sequence, 1);
        a.present(target_scanout).unwrap();
        assert_eq!(a.state.lock().counter_epoch, 1);
        // Model the pipe's reset counter, not an IRQ-count substitution.
        r.set(0x70040, 0);
        dev.advance_vblank().unwrap();
        assert_eq!(
            dev.vblank_sequence(),
            sequence,
            "new epoch is only a baseline"
        );
        dev.advance_vblank().unwrap();
        assert_eq!(
            dev.vblank_sequence(),
            sequence + 1,
            "one fresh hardware frame"
        );
    }

    #[test]
    fn hpd_irq_is_only_a_hint_and_each_edid_sample_keeps_250ms_debounce() {
        assert!(!hpd_probe_due(249_999, 0, None));
        assert!(hpd_probe_due(250_000, 0, None));
        // A recent IRQ delays a probe even if the periodic timer just elapsed;
        // repeated notifications restart the quiet interval.
        assert!(!hpd_probe_due(250_000, 0, Some(249_999)));
        assert!(!hpd_probe_due(500_000, 0, Some(250_001)));
        assert!(hpd_probe_due(500_001, 0, Some(250_001)));
        // After the first sample, a second confirmation cannot happen early.
        assert!(!hpd_probe_due(499_999, 250_000, None));
        assert!(hpd_probe_due(500_000, 250_000, None));
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
            Some(a.preferred),
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
            a.validate_atomic_state(false, true, false, false),
            Err(DrmError::Unsupported)
        );
        assert_eq!(
            a.validate_atomic_state(true, false, false, false),
            Err(DrmError::Unsupported)
        );
        assert_eq!(
            a.validate_atomic_state(true, true, true, false),
            Err(DrmError::Unsupported)
        );
        assert_eq!(
            a.validate_atomic_state(true, true, false, true),
            Err(DrmError::Unsupported),
            "degamma/CTM changes are not programmed by the native backend"
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
