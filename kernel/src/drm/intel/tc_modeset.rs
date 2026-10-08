// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_ddi.c:
// intel_ddi_enable_transcoder_clock, intel_ddi_transcoder_func_reg_val_get,
// intel_ddi_buf_enable/disable; intel_dpll_mgr.c: dkl_pll_write, mg_pll_enable/
// disable; intel_display.c: intel_set_transcoder_timings and pipe enable;
// skl_universal_plane.c: primary-plane arm. Original source copyrights:
// Copyright © 2006-2007 Intel Corporation (intel_display.c).
// Copyright © 2006-2016 Intel Corporation (intel_dpll_mgr.c).
// Copyright © 2012 Intel Corporation (intel_ddi.c).
// Copyright © 2020 Intel Corporation (skl_universal_plane.c).
// MIT permission text: crates/ax/tk-intel-display/LICENSE-MIT.
// ADL-P/N display13 legacy TC1/TC2 HDMI only; no Type-C cold exit/ownership
// acquisition, DP, DSI, combo PHY, runtime CDCLK/PCode transition or other
// unsupported workaround path. Initial CDCLK/PCode setup is in power.rs.

//! Restricted live TC HDMI modeset sequence. Admission and complete before-
//! image recovery are owned by `fastboot`; this file only performs a forward
//! programming pass after the active TC link, required power wells, firmware
//! framebuffer ownership, target timing, PLL and watermark policy were proved.

use alloc::{format, string::String, vec::Vec};

use intel_display::{
    RegisterIo,
    device::Port,
    dkl_phy::{DklIo, TcPort},
    dpll_mgr::{DklPllState, dkl_pll_write},
    hdmi::RawInfoframe,
    hdmi_packet::{Infoframe, hdmi_infoframe_checksum},
};

use super::{
    gmbus::PollTimer,
    pipe,
    regs::{Meaning, Register, Registers, ddi, pipe as p},
};
use crate::drm::modes::Mode;

const PIPE_ENABLE: u32 = 1 << 31;
const PIPE_RUNNING: u32 = 1 << 30;
const PLL_ENABLE: u32 = 1 << 31;
const PLL_LOCK: u32 = 1 << 30;
const PLL_POWER_ENABLE: u32 = 1 << 27;
const PLL_POWER_STATE: u32 = 1 << 26;
const DDI_ENABLE: u32 = 1 << 31;
const DDI_IDLE: u32 = 1 << 7;
const DDI_TC_PHY_OWNERSHIP: u32 = 1 << 6;
const DDI_LANE_REVERSAL: u32 = 1 << 16;
const AVI_ENABLE: u32 = 1 << 12;
const UNDERRUN: u32 = 1 << 31;
const MODE_SHADOW: [u32; 14] = [
    0x70030, 0x70028, 0x6007c, 0x60000, 0x60004, 0x60008, 0x6000c, 0x60010, 0x60014, 0x6001c,
    0x70188, 0x7018c, 0x70190, 0x701cc,
];

fn mmio_register(offset: u32, write: bool) -> Register {
    if write {
        Register::read_write("N305_TC_MODESET", offset, Meaning::BringUp, None)
    } else {
        Register::read_only("N305_TC_MODESET", offset, Meaning::BringUp, None)
    }
}
fn read(r: &impl Registers, offset: u32) -> Result<u32, String> {
    r.read(mmio_register(offset, false))
        .filter(|value| *value != u32::MAX)
        .ok_or_else(|| format!("TC modeset register {offset:#x} unavailable"))
}
fn write(r: &impl Registers, offset: u32, value: u32) -> Result<(), String> {
    if r.write(mmio_register(offset, true), value) {
        Ok(())
    } else {
        Err(format!("TC modeset write {offset:#x} refused"))
    }
}
fn poll(
    r: &impl Registers,
    timer: &impl PollTimer,
    offset: u32,
    mask: u32,
    expected: u32,
    timeout_us: u64,
) -> Result<(), String> {
    let start = timer.now_micros();
    for _ in 0..500_000 {
        if read(r, offset)? & mask == expected {
            return Ok(());
        }
        if timer.now_micros().saturating_sub(start) >= timeout_us {
            break;
        }
        timer.pause();
    }
    Err(format!(
        "TC modeset status timeout register={offset:#x} mask={mask:#x} expected={expected:#x}"
    ))
}

fn tc_phy(port: TcPort) -> Result<Port, String> {
    match port {
        TcPort::Tc1 => Ok(Port::Tc1),
        TcPort::Tc2 => Ok(Port::Tc2),
        TcPort::Tc3 | TcPort::Tc4 => Err(String::from("only TC1/TC2 legacy HDMI admitted")),
    }
}
fn ddi_buf_ctl(port: TcPort) -> Result<u32, String> {
    tc_phy(port)?;
    // ADL-P maps PORT_D/PORT_E to TC1/TC2, at the DDI_CTL register stride.
    Ok(0x64000 + (3 + port.index()) * 0x100)
}
fn trans_select_port(port: TcPort) -> Result<u32, String> {
    tc_phy(port)?;
    // `TGL_TRANS_DDI_SELECT_PORT(port) = (port + 1) << 27`, with TC1=D(3).
    Ok((4 + port.index()) << 27)
}
fn trans_clock_select(port: TcPort) -> Result<u32, String> {
    tc_phy(port)?;
    // Display13 maps D/E (TC1/2) to PHY_F/PHY_G, not DDI D/E.
    Ok((6 + port.index()) << 28)
}

fn dkl_io<R: Registers>(registers: &R) -> DklRegisterIo<'_, R> {
    DklRegisterIo { registers }
}
struct DklRegisterIo<'a, R> {
    registers: &'a R,
}
impl<R: Registers> RegisterIo for DklRegisterIo<'_, R> {
    fn read32(&self, offset: u32) -> Result<u32, intel_display::Error> {
        self.registers
            .read(mmio_register(offset, false))
            .filter(|value| *value != u32::MAX)
            .ok_or(intel_display::Error::Unavailable(offset))
    }
    fn write32(&self, offset: u32, value: u32) -> Result<(), intel_display::Error> {
        if self.registers.write(mmio_register(offset, true), value) {
            Ok(())
        } else {
            Err(intel_display::Error::Unavailable(offset))
        }
    }
}
impl<R: Registers> DklIo for DklRegisterIo<'_, R> {
    fn with_dkl_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, intel_display::Error>,
    ) -> Result<T, intel_display::Error> {
        let _lock = super::DKL_ACCESS_LOCK.lock();
        operation()
    }
}

impl<R: Registers> intel_display::tc::TcIo for DklRegisterIo<'_, R> {
    fn display_core_powered(&self) -> bool {
        let mask = 2 | (2 << 2) | (2 << 10);
        self.read32(0x45404)
            .is_ok_and(|v| v & (mask | (mask >> 1)) == mask | (mask >> 1))
            && self
                .read32(0x45504)
                .is_ok_and(|v| v & super::power::DC_STATE_MASK == 0)
    }
    fn tc_port_powered(&self, port: TcPort) -> bool {
        if port.index() > 1 {
            return false;
        }
        let mask = 2 << ((3 + port.index()) * 2);
        self.read32(0x45454)
            .is_ok_and(|v| v & (mask | (mask >> 1)) == mask | (mask >> 1))
    }
    fn tc_cold_blocked(&self, port: TcPort) -> bool {
        if port.index() > 1 {
            return false;
        }
        let mask = 2 << ((3 + port.index()) * 2);
        self.read32(0x45444)
            .is_ok_and(|v| v & (mask | (mask >> 1)) == mask | (mask >> 1))
    }
}

/// Recover only the eight words changed by the signal-level sequence while
/// the DDI buffer is disabled. Restore the whole shared selector even when a
/// store landed and then reported failure; the outer transaction owns power,
/// the complete PHY before-image and the subsequent full scanout proof.
fn restore_signal_levels(
    io: &impl DklIo,
    port: TcPort,
    before: &intel_display::tc::DklPhyState,
) -> Result<(), String> {
    io.with_dkl_lock(|| {
        let selector = io.read32(0x1010a0)?;
        let result = (|| {
            for (lane, state) in before.lanes.iter().enumerate() {
                for (offset, value) in [
                    (0x0d00, state.lane_suspend),
                    (0x2c0, state.tx_control[0]),
                    (0x2c4, state.tx_control[1]),
                    (0x2c8, state.tx_control[2]),
                ] {
                    let reg = intel_display::dkl_phy::DklRegister::new(
                        port,
                        lane as u32 * 0x1000 + offset,
                    )?;
                    io.write32(reg.selector(), reg.index_value())?;
                    io.write32(reg.aperture(), value)?;
                    if io.read32(reg.aperture())? != value {
                        return Err(intel_display::Error::RestoreFailed(reg.aperture()));
                    }
                }
            }
            Ok(())
        })();
        if io.write32(0x1010a0, selector).is_err() || io.read32(0x1010a0).ok() != Some(selector) {
            return Err(intel_display::Error::RestoreFailed(0x1010a0));
        }
        result
    })
    .map_err(|e| format!("TC signal-level restoration failed: {e:?}"))
}

fn dkl_pll_off<R: Registers, T: PollTimer>(r: &R, timer: &T, port: TcPort) -> Result<(), String> {
    let offset = port.pll_enable();
    let before = read(r, offset)?;
    if before & PLL_ENABLE != 0 {
        write(r, offset, before & !PLL_ENABLE)?;
        // i915 `icl_pll_disable`: the lock clear timeout is 1 us.
        poll(r, timer, offset, PLL_LOCK, 0, 1)?;
    }
    let current = read(r, offset)?;
    if current & PLL_POWER_ENABLE != 0 {
        write(r, offset, current & !PLL_POWER_ENABLE)?;
        poll(r, timer, offset, PLL_POWER_STATE, 0, 1_000)?;
    }
    Ok(())
}
fn dkl_pll_on<R: Registers, T: PollTimer>(
    r: &R,
    timer: &T,
    port: TcPort,
    state: &DklPllState,
    afc_startup: Option<u8>,
) -> Result<(), String> {
    let offset = port.pll_enable();
    let before = read(r, offset)?;
    if before & (PLL_ENABLE | PLL_POWER_ENABLE) != 0 {
        return Err(String::from(
            "TC DKL PLL must be disabled before programming",
        ));
    }
    write(r, offset, before | PLL_POWER_ENABLE)?;
    poll(r, timer, offset, PLL_POWER_STATE, PLL_POWER_STATE, 1_000)?;
    dkl_pll_write(&dkl_io(r), port, state, afc_startup)
        .map_err(|e| format!("DKL PLL field programming failed: {e:?}"))?;
    let powered = read(r, offset)?;
    write(r, offset, powered | PLL_ENABLE)?;
    poll(r, timer, offset, PLL_LOCK, PLL_LOCK, 600)
}

fn wait_two_frames<R: Registers, T: PollTimer>(r: &R, timer: &T) -> Result<(), String> {
    if read(r, p::PIPECONF_A.offset())? & PIPE_RUNNING == 0 {
        return Ok(());
    }
    let started = timer.now_micros();
    let mut last = read(r, p::PIPEDSL_A.offset())? & 0x000f_ffff;
    let mut wraps = 0;
    let mut sample_after = started;
    for _ in 0..500_000 {
        let now = timer.now_micros();
        if now.saturating_sub(started) >= 200_000 {
            break;
        }
        if now >= sample_after {
            let line = read(r, p::PIPEDSL_A.offset())? & 0x000f_ffff;
            if line < last {
                wraps += 1;
                if wraps == 2 {
                    return Ok(());
                }
            }
            last = line;
            sample_after = now.saturating_add(200);
        }
        timer.pause();
    }
    Err(String::from(
        "TC plane quiesce missed two live frame boundaries",
    ))
}

fn frame_counter<R: Registers>(r: &R) -> Result<u32, String> {
    // This monotonically increments and is allowed to wrap through all-ones.
    r.read(mmio_register(0x70040, false))
        .ok_or_else(|| String::from("TC frame counter unavailable"))
}
fn prove_new_scanout<R: Registers, T: PollTimer>(
    r: &R,
    timer: &T,
    address: u32,
) -> Result<(), String> {
    let started = timer.now_micros();
    let first = frame_counter(r)?;
    let mut matching_frame = None;
    for _ in 0..1_000_000 {
        let live = read(r, p::PLANE_SURFLIVE_A.offset())? & 0xffff_f000;
        let frame = frame_counter(r)?;
        if live == address & 0xffff_f000 && frame != first {
            if let Some(previous) = matching_frame {
                if frame != previous {
                    if read(r, ddi_buf_ctl_from_surface(r)?)? & DDI_IDLE != 0 {
                        return Err(String::from("TC DDI became idle after enable"));
                    }
                    if read(r, p::PIPESTAT_A.offset())? & UNDERRUN != 0 {
                        return Err(String::from("pipe A FIFO underrun after TC commit"));
                    }
                    return Ok(());
                }
            } else {
                matching_frame = Some(frame);
            }
        }
        if timer.now_micros().saturating_sub(started) >= 150_000 {
            break;
        }
        timer.pause();
    }
    Err(format!(
        "TC plane did not latch {address:#010x} through fresh frames"
    ))
}

// The caller sets the port before every proof; keeping this lookup explicit
// avoids treating a firmware DDI identity as a DKL selector.
fn ddi_buf_ctl_from_surface<R: Registers>(r: &R) -> Result<u32, String> {
    let function = read(r, ddi::TRANS_DDI_FUNC_CTL_A.offset())?;
    match intel_display::ddi::decode_function_control(function).port {
        Some(Port::Tc1) => ddi_buf_ctl(TcPort::Tc1),
        Some(Port::Tc2) => ddi_buf_ctl(TcPort::Tc2),
        _ => Err(String::from("active transcoder no longer names TC1/TC2")),
    }
}

/// Rebuild a validated AVI packet for one supported CTA mode. The hardware
/// stores an ECC byte-hole at raw byte3; the checksum stays at raw byte4.
pub(super) fn avi_words(frame: RawInfoframe, vic: u8) -> Result<[u32; 8], String> {
    if !matches!(frame.unpack(), Ok(Infoframe::Avi(_))) || vic == 0 || vic > 127 {
        return Err(String::from("TC AVI source packet or VIC is not admitted"));
    }
    let mut packet = frame.packet();
    // HDMI AVI packet byte7 is VIC: header3 + checksum + payload byte3.
    packet[7] = vic;
    packet[3] = 0;
    packet[3] = hdmi_infoframe_checksum(&packet[..17]);
    let mut raw = [0u8; 32];
    raw[..3].copy_from_slice(&packet[..3]);
    raw[4..18].copy_from_slice(&packet[3..17]);
    Ok(core::array::from_fn(|n| {
        let offset = n * 4;
        u32::from_le_bytes([
            raw[offset],
            raw[offset + 1],
            raw[offset + 2],
            raw[offset + 3],
        ])
    }))
}

/// Restore the firmware's original AVI image exactly during rollback. The
/// old transcoder timing may have had a pre-existing AVI VIC that differs from
/// the CTA code later used to advertise that captured firmware mode.
pub(super) fn avi_words_preserve(frame: RawInfoframe) -> Result<[u32; 8], String> {
    if !matches!(frame.unpack(), Ok(Infoframe::Avi(_))) {
        return Err(String::from("TC rollback AVI source packet is invalid"));
    }
    Ok(core::array::from_fn(|n| {
        let offset = n * 4;
        u32::from_le_bytes([
            frame.raw[offset],
            frame.raw[offset + 1],
            frame.raw[offset + 2],
            frame.raw[offset + 3],
        ])
    }))
}

/// Ask the translated i915 HDMI clock policy whether this TC request can be
/// driven by the deliberately limited native path.  The kernel currently
/// supports only HDMI RGB at 8 bpc and does not implement scrambling/SCDC, so
/// the 300 MHz source ceiling is intentional. EDID sink TMDS limits are not
/// parsed here, so this does not claim that the sink's downstream ceiling was
/// verified; it only applies the source/mode policy for the implemented path.
/// Returning the source-computed TMDS rate also makes the assumption explicit
/// to the caller (for RGB 8 bpc it equals the pixel clock).
fn source_hdmi_tmds_clock(mode: &Mode, edid_bytes: &[u8]) -> Option<u32> {
    use intel_display::intel_hdmi_full::{
        ClockLimits, HdmiMode, HdmiPortClass, OutputFormat, PortPlatform, SinkCapabilities,
        intel_hdmi_compute_clock,
    };

    let mut pipe_bpp = 24;
    let adjusted = HdmiMode {
        clock_khz: i32::try_from(mode.clock_khz).ok()?,
        hdisplay: i32::from(mode.hdisplay),
        htotal: i32::from(mode.htotal),
        hblank_start: i32::from(mode.hdisplay),
        hblank_end: i32::from(mode.htotal),
        hsync_start: i32::from(mode.hsync_start),
        hsync_end: i32::from(mode.hsync_end),
        // Interlace and double-clock modes are rejected by the caller before
        // this helper; the upstream helper only consumes DBLCLK here.
        flags: 0,
    };
    let sink = SinkCapabilities {
        has_hdmi_sink: true,
        ycbcr420_allowed: false,
        mode_is_420: false,
        rgb_10bpc: false,
        rgb_12bpc: false,
        y420_10bpc: false,
        y420_12bpc: false,
        gmch: false,
        display_version: 13,
    };
    let limits = ClockLimits {
        platform: PortPlatform::Display(13),
        port: HdmiPortClass::TypeC,
        source_limit_khz: 300_000,
        dp_dual_mode_limit_khz: None,
        sink_limit_khz: crate::drm::modes::Edid::parse_lossy(edid_bytes)
            .ok()
            .and_then(|edid| edid.max_tmds_clock_khz())
            .and_then(|clock| i32::try_from(clock).ok()),
        has_hdmi_sink: true,
        respect_downstream_limits: true,
    };
    u32::try_from(intel_hdmi_compute_clock(
        &mut pipe_bpp,
        adjusted,
        OutputFormat::Rgb,
        sink,
        limits,
        true,
    )?)
    .ok()
}

/// Program one validated progressive RGB/XRGB timing on an already-active,
/// already-owned legacy TC port. No cold transition or new PHY ownership is
/// attempted. The DDB allocation and DBUF slice programming remain the
/// source's whole-buffer first-light-up policy, but primary-plane watermarks
/// are calculated from the PCode-derived latency table for the target mode.
pub(super) fn program<R: Registers + Send + Sync, T: PollTimer>(
    r: &R,
    timer: &T,
    port: TcPort,
    mode: &Mode,
    pitch: u32,
    surface: u32,
    watermark: Option<pipe::WatermarkConfig>,
    pll: &DklPllState,
    afc_startup: Option<u8>,
    avi: &[u32; 8],
    edid: &[u8],
    restore_pipe_misc: Option<u32>,
    restore_plane_ctl: Option<u32>,
    retire_audio_before_link: bool,
    restore_phy: Option<&intel_display::tc::DklPhyState>,
    display_writes_started: &mut bool,
) -> Result<(), String> {
    *display_writes_started = false;
    let function = read(r, ddi::TRANS_DDI_FUNC_CTL_A.offset())?;
    if !matches!(
        intel_display::ddi::decode_function_control(function).port,
        Some(Port::Tc1 | Port::Tc2)
    ) || mode.flags.contains(crate::drm::modes::ModeFlags::INTERLACE)
        || mode
            .flags
            .contains(crate::drm::modes::ModeFlags::DOUBLE_CLOCK)
        || !matches!(source_hdmi_tmds_clock(mode, edid), Some(25_000..=300_000))
        || surface == 0
        || surface & 0xfff != 0
        || pitch < u32::from(mode.hdisplay) * 4
        || !pitch.is_multiple_of(64)
    {
        return Err(String::from(
            "TC target mode or current legacy link not admitted",
        ));
    }
    let selected = tc_phy(port)?;
    if intel_display::ddi::decode_function_control(function).port != Some(selected) {
        return Err(String::from(
            "TC request does not match active firmware route",
        ));
    }
    let control_offset = ddi_buf_ctl(port)?;
    let buffer = read(r, control_offset)?;
    let plane_surface = pipe::PlaneSurface {
        ggtt_address: u64::from(surface),
        stride_bytes: pitch,
    };
    let pipe_program = match watermark {
        Some(watermark) => {
            pipe::compute_with_watermarks(pipe::Pipe::A, mode, plane_surface, watermark)
        }
        None => {
            #[cfg(test)]
            {
                pipe::compute(pipe::Pipe::A, mode, plane_surface)
            }
            #[cfg(not(test))]
            {
                return Err(String::from("PCode-derived watermark state absent"));
            }
        }
    }
    .map_err(|e| format!("TC pipe plan refused: {}", e.describe()))?;
    // Preflight every direct RW input needed by this pass before the first
    // destructive write. Power, link, WM/DDB and ownership proof are performed
    // by the admission transaction that invokes this function.
    let pipe_misc = read(r, p::PIPE_MISC_A.offset())?;
    let arb = read(r, p::PIPE_ARB_CTL_A.offset())?;
    let old_plane = read(r, p::PLANE_CTL_A.offset())?;
    let old_surface = read(r, p::PLANE_SURF_A.offset())?;
    let _old_clock = read(r, 0x46140)?;
    let _old_pipeconf = read(r, p::PIPECONF_A.offset())?;
    // Pipe shadow values are all calculated and all heap space is reserved
    // before the first MMIO write. The DDB allocation remains the one-pipe
    // whole-buffer first-light-up policy; its source-derived WM values above
    // were calculated for this exact target before entering this function.
    let all_shadow = pipe_program.writes(pipe_misc, arb);
    let mut shadow = Vec::new();
    shadow
        .try_reserve_exact(MODE_SHADOW.len())
        .map_err(|_| String::from("TC shadow-write journal allocation failed"))?;
    for planned in all_shadow
        .into_iter()
        .filter(|planned| MODE_SHADOW.contains(&planned.register.offset()))
    {
        let value = if planned.register.offset() == p::PIPE_MISC_A.offset() {
            restore_pipe_misc.unwrap_or(planned.value)
        } else {
            planned.value
        };
        shadow.push(super::pipe::PlannedWrite { value, ..planned });
    }
    if shadow.len() != MODE_SHADOW.len() {
        let present = shadow
            .iter()
            .map(|planned| planned.register.offset())
            .collect::<Vec<_>>();
        let missing = MODE_SHADOW
            .iter()
            .copied()
            .filter(|offset| !present.contains(offset))
            .collect::<Vec<_>>();
        return Err(format!(
            "TC source pipe plan is missing shadow fields present={present:?} missing={missing:?}"
        ));
    }
    // HDA first retires DMA/ELD while the old TC link and its display power
    // wells are still active. Refusal leaves this forward pass write-free.
    if retire_audio_before_link {
        super::audio::before_link_disable(r, timer, port)?;
    }
    // From this point every error requires a hardware before-image rollback;
    // a failure above left the display image untouched.
    *display_writes_started = true;
    // Turn off the primary plane and wait for scanout to cross two boundaries
    // before changing the mode/link clock.
    write(r, p::PLANE_CTL_A.offset(), old_plane & !PIPE_ENABLE)?;
    write(r, p::PLANE_SURF_A.offset(), old_surface)?;
    wait_two_frames(r, timer)?;
    write(
        r,
        p::PIPECONF_A.offset(),
        read(r, p::PIPECONF_A.offset())? & !(PIPE_ENABLE | PIPE_RUNNING),
    )?;
    poll(r, timer, p::PIPECONF_A.offset(), PIPE_RUNNING, 0, 100_000)?;
    write(
        r,
        ddi::TRANS_DDI_FUNC_CTL_A.offset(),
        read(r, ddi::TRANS_DDI_FUNC_CTL_A.offset())? & !DDI_ENABLE,
    )?;
    let current_buf = read(r, control_offset)?;
    write(r, control_offset, current_buf & !(DDI_ENABLE | DDI_IDLE))?;
    poll(r, timer, control_offset, DDI_IDLE, DDI_IDLE, 100_000)?;
    write(r, 0x46140, 0)?;
    dkl_pll_off(r, timer, port)?;

    for planned in shadow {
        write(r, planned.register.offset(), planned.value)?;
    }

    dkl_pll_on(r, timer, port, pll, afc_startup)?;
    write(r, 0x46140, trans_clock_select(port)?)?;
    let avi_control = read(r, 0x60200)?;
    write(r, 0x60200, avi_control & !AVI_ENABLE)?;
    for (word, value) in avi.iter().copied().enumerate() {
        write(r, 0x60220 + word as u32 * 4, value)?;
    }
    write(r, 0x60200, avi_control | AVI_ENABLE)?;
    let polarity = function & ((1 << 16) | (1 << 17));
    write(
        r,
        ddi::TRANS_DDI_FUNC_CTL_A.offset(),
        DDI_ENABLE | trans_select_port(port)? | polarity,
    )?;
    // Linux intel_ddi_enable_hdmi sets DKL levels before enabling DDI_BUF.
    // Exact N305 D0 admission is the source Wa_16011342517 applicability proof;
    // VBT level5 was checked at the outer native handoff. Unknown revisions
    // cannot reach this restricted path.
    let phy_io = dkl_io(r);
    if let Some(before) = restore_phy {
        restore_signal_levels(&phy_io, port, before)?;
    } else {
        intel_display::tc::adlp_tc_dkl_hdmi_set_signal_levels(
            &phy_io,
            port,
            mode.clock_khz,
            5,
            intel_display::tc::Wa16011342517::Active,
        )
        .map_err(|e| format!("TC HDMI signal-level programming failed: {e:?}"))?;
    }
    // HDMI on ADL-P/DKL has zero DP lanes and retains board lane reversal. TC
    // PHY ownership remains asserted; no inferred swing or USB-C mux is set.
    let output = (buffer & DDI_LANE_REVERSAL) | DDI_TC_PHY_OWNERSHIP | DDI_ENABLE;
    write(r, control_offset, output)?;
    poll(r, timer, control_offset, DDI_IDLE, 0, 100_000)?;
    write(
        r,
        p::PIPECONF_A.offset(),
        read(r, p::PIPECONF_A.offset())? | PIPE_ENABLE,
    )?;
    poll(
        r,
        timer,
        p::PIPECONF_A.offset(),
        PIPE_RUNNING,
        PIPE_RUNNING,
        100_000,
    )?;
    // `PLANE_CTL` self-arms only immediately before `PLANE_SURF`; neither is
    // published as successful until SURFLIVE and fresh frames prove it.
    write(
        r,
        p::PLANE_CTL_A.offset(),
        restore_plane_ctl.unwrap_or(pipe_program.plane.ctl),
    )?;
    write(r, p::PLANE_SURF_A.offset(), pipe_program.plane.surf)?;
    prove_new_scanout(r, timer, surface)?;
    match super::audio::after_link_enabled(r, timer, port, mode.clock_khz, edid) {
        Ok(super::audio::LinkAudioStatus::Enabled) => {}
        Ok(super::audio::LinkAudioStatus::Unavailable(reason)) => {
            axlog::warn!("intel-hdmi-audio: TC modeset committed video-only: {reason}");
        }
        Err(error) => {
            // Display state is already proved and the HDA layer retains its
            // quarantine; do not tear down a working link or lose vblank
            // retirement because audio teardown/re-enable was uncertain.
            axlog::error!(
                "intel-hdmi-audio: TC modeset video is stable, HDA state is unverified; retaining \
                 display link and power: {error}"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::source_hdmi_tmds_clock;
    use crate::drm::modes::{Mode, ModeFlags, TimingSource};

    fn mode(clock_khz: u32) -> Mode {
        Mode::from_blanking(
            clock_khz,
            1920,
            280,
            88,
            44,
            1080,
            45,
            4,
            5,
            ModeFlags::NONE,
            TimingSource::CtaVic(16),
        )
    }

    #[test]
    fn active_tc_hdmi_uses_source_tmds_policy() {
        assert_eq!(source_hdmi_tmds_clock(&mode(148_500), &[]), Some(148_500));
        assert_eq!(source_hdmi_tmds_clock(&mode(300_000), &[]), Some(300_000));
        assert_eq!(source_hdmi_tmds_clock(&mode(300_001), &[]), None);
        assert_eq!(source_hdmi_tmds_clock(&mode(24_999), &[]), None);
    }
}
