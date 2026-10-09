// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_ddi.c:
// intel_ddi_enable_transcoder_clock, intel_ddi_transcoder_func_reg_val_get,
// intel_ddi_disable_transcoder_func,
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
//! framebuffer ownership, target timing, PLL reservation and watermark policy
//! were proved. The forward pass uses translated shared-DPLL callbacks for
//! DKL disable/enable; its direct DKL writes remain only for rollback/tests.

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
    let value = super::de_io::DeIo::new(r)
        .read(mmio_register(offset, false))
        .map_err(|error| format!("TC modeset register {offset:#x} unavailable: {error:?}"))?;
    if value == u32::MAX {
        Err(format!("TC modeset register {offset:#x} unavailable"))
    } else {
        Ok(value)
    }
}
fn write(r: &impl Registers, offset: u32, value: u32) -> Result<(), String> {
    super::de_io::DeIo::new(r)
        .write(mmio_register(offset, true), value)
        .map_err(|error| format!("TC modeset write {offset:#x} refused: {error:?}"))
}
fn poll<T: PollTimer + ?Sized>(
    r: &impl Registers,
    timer: &T,
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
pub(super) fn dkl_io<R: Registers>(registers: &R) -> DklRegisterIo<'_, R> {
    DklRegisterIo { registers }
}
pub(super) struct DklRegisterIo<'a, R> {
    registers: &'a R,
}
impl<R: Registers> RegisterIo for DklRegisterIo<'_, R> {
    fn read32(&self, offset: u32) -> Result<u32, intel_display::Error> {
        let value = super::de_io::DeIo::new(self.registers)
            .read(mmio_register(offset, false))
            .map_err(|_| intel_display::Error::Unavailable(offset))?;
        if value == u32::MAX {
            Err(intel_display::Error::Unavailable(offset))
        } else {
            Ok(value)
        }
    }
    fn write32(&self, offset: u32, value: u32) -> Result<(), intel_display::Error> {
        super::de_io::DeIo::new(self.registers)
            .write(mmio_register(offset, true), value)
            .map_err(|_| intel_display::Error::Unavailable(offset))
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

/// Optional source shared-DPLL lifecycle operations owned by the enclosing
/// Native transaction. Rollback uses direct before-image restoration because
/// source callbacks are void and cannot serve as their own inverse.
pub(super) trait DpllLifecycle {
    fn disable(&mut self) -> Result<(), String>;
    fn enable(&mut self) -> Result<(), String>;
    fn rollback_new(&mut self) -> Result<(), String>;
    fn release_new(&mut self) -> Result<(), String>;
}

/// CDCLK is shared by every display pipe. The Native caller may change it
/// only while a complete all-pipes/link-disabled predicate holds, and must
/// pair any forward transition with the captured CDCLK before-image.
pub(super) trait ClockLifecycle {
    fn adjust(&mut self, target_clock_khz: u32, restore_before_image: bool) -> Result<(), String>;
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
    let sink_limit_khz = crate::drm::modes::Edid::parse_lossy(edid_bytes)
        .ok()
        .and_then(|edid| edid.max_tmds_clock_khz());
    source_hdmi_tmds_clock_with_limit(
        mode,
        sink_limit_khz,
        intel_display::intel_hdmi_full::HdmiPortClass::TypeC,
    )
}

pub(super) fn source_hdmi_tmds_clock_with_limit(
    mode: &Mode,
    sink_limit_khz: Option<u32>,
    port_class: intel_display::intel_hdmi_full::HdmiPortClass,
) -> Option<u32> {
    use intel_display::intel_hdmi_full::{
        ClockLimits, HdmiMode, OutputFormat, PortPlatform, SinkCapabilities,
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
        port: port_class,
        source_limit_khz: 300_000,
        dp_dual_mode_limit_khz: None,
        sink_limit_khz: sink_limit_khz.and_then(|clock| i32::try_from(clock).ok()),
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

/// Bound the translated transcoder-disable helper to the single Pipe-A pair
/// that the N305 TC transaction already owns. A failed read suppresses the
/// follow-up FUNC_CTL write; the caller verifies both writes by readback.
struct TcTranscoderFuncIo<'a, R> {
    registers: &'a R,
    func_ctl: Option<Register>,
    func_ctl2: Option<Register>,
    buffer_ctl: Option<Register>,
    clock_sel: Option<Register>,
    timer: Option<&'a dyn PollTimer>,
    read_failed: bool,
    write_failed: bool,
    wait_timed_out: bool,
}

impl<'a, R: Registers> TcTranscoderFuncIo<'a, R> {
    fn new(registers: &'a R) -> Self {
        Self {
            registers,
            func_ctl: Some(ddi::TRANS_DDI_FUNC_CTL_A),
            func_ctl2: Some(ddi::TRANS_DDI_FUNC_CTL2_A),
            buffer_ctl: None,
            clock_sel: None,
            timer: None,
            read_failed: false,
            write_failed: false,
            wait_timed_out: false,
        }
    }

    fn buffer(registers: &'a R, buffer_ctl: Register, timer: &'a dyn PollTimer) -> Self {
        Self {
            registers,
            func_ctl: None,
            func_ctl2: None,
            buffer_ctl: Some(buffer_ctl),
            clock_sel: None,
            timer: Some(timer),
            read_failed: false,
            write_failed: false,
            wait_timed_out: false,
        }
    }

    fn clock(registers: &'a R, clock_sel: Register) -> Self {
        Self {
            registers,
            func_ctl: None,
            func_ctl2: None,
            buffer_ctl: None,
            clock_sel: Some(clock_sel),
            timer: None,
            read_failed: false,
            write_failed: false,
            wait_timed_out: false,
        }
    }

    fn register(&self, offset: u32) -> Option<Register> {
        [
            self.func_ctl,
            self.func_ctl2,
            self.buffer_ctl,
            self.clock_sel,
        ]
        .into_iter()
        .flatten()
        .find(|register| register.offset() == offset)
    }
}

impl<R: Registers> intel_display::intel_ddi_full::DdiIo for TcTranscoderFuncIo<'_, R> {
    fn read(&mut self, reg: u32) -> u32 {
        let Some(typed) = self.register(reg) else {
            self.read_failed = true;
            return u32::MAX;
        };
        match self.registers.read(typed) {
            Some(value) => value,
            None => {
                self.read_failed = true;
                u32::MAX
            }
        }
    }

    fn write(&mut self, reg: u32, value: u32) {
        let Some(typed) = self.register(reg) else {
            self.write_failed = true;
            return;
        };
        if self.read_failed || !self.registers.write(typed, value) {
            self.write_failed = true;
        }
    }

    fn combo_phy_read(
        &mut self,
        _phy: u8,
        _reg: intel_display::intel_ddi_full::ComboPhyRegister,
    ) -> u32 {
        0
    }
    fn combo_phy_write(
        &mut self,
        _phy: u8,
        _reg: intel_display::intel_ddi_full::ComboPhyRegister,
        _value: u32,
    ) {
    }
    fn combo_phy_rmw(
        &mut self,
        _phy: u8,
        _reg: intel_display::intel_ddi_full::ComboPhyRegister,
        _clear: u32,
        _set: u32,
    ) {
    }
    fn mg_phy_rmw(
        &mut self,
        _port: intel_display::intel_ddi_full::Port,
        _reg: intel_display::intel_ddi_full::MgPhyRegister,
        _clear: u32,
        _set: u32,
    ) {
    }
    fn dkl_phy_read(
        &mut self,
        _port: intel_display::intel_ddi_full::Port,
        _reg: intel_display::intel_ddi_full::DklPhyRegister,
    ) -> u32 {
        0
    }
    fn dkl_phy_write(
        &mut self,
        _port: intel_display::intel_ddi_full::Port,
        _reg: intel_display::intel_ddi_full::DklPhyRegister,
        _value: u32,
    ) {
    }
    fn dkl_phy_rmw(
        &mut self,
        _port: intel_display::intel_ddi_full::Port,
        _reg: intel_display::intel_ddi_full::DklPhyRegister,
        _clear: u32,
        _set: u32,
    ) {
    }
    fn mg_dp_mode_read(&mut self, _port: intel_display::intel_ddi_full::Port, _lane: u8) -> u32 {
        0
    }
    fn mg_dp_mode_write(
        &mut self,
        _port: intel_display::intel_ddi_full::Port,
        _lane: u8,
        _value: u32,
    ) {
    }

    fn wait_set(&mut self, reg: u32, mask: u32, timeout_ms: u32) -> bool {
        self.wait_for_buffer_state(reg, mask, mask, u64::from(timeout_ms) * 1_000)
    }

    fn wait_clear(&mut self, reg: u32, mask: u32, timeout_ms: u32) -> bool {
        self.wait_for_buffer_state(reg, mask, 0, u64::from(timeout_ms) * 1_000)
    }
}

impl<R: Registers> TcTranscoderFuncIo<'_, R> {
    fn wait_for_buffer_state(
        &mut self,
        reg: u32,
        mask: u32,
        expected: u32,
        timeout_us: u64,
    ) -> bool {
        if self.buffer_ctl.is_none_or(|buffer| buffer.offset() != reg) || mask != DDI_IDLE {
            self.wait_timed_out = true;
            return true;
        }
        let Some(timer) = self.timer else {
            self.wait_timed_out = true;
            return true;
        };
        let result = poll(self.registers, timer, reg, mask, expected, timeout_us);
        if result.is_err() {
            self.wait_timed_out = true;
            true
        } else {
            false
        }
    }
}

fn source_tc_encoder(port: TcPort) -> Result<intel_display::intel_ddi_full::DdiEncoder, String> {
    use intel_display::intel_ddi_full as i915;

    let source_port = match port {
        TcPort::Tc1 => i915::Port::D,
        TcPort::Tc2 => i915::Port::E,
        TcPort::Tc3 | TcPort::Tc4 => {
            return Err(String::from(
                "translated DDI buffer path only admits TC1/TC2",
            ));
        }
    };
    let phy = match port {
        TcPort::Tc1 => 5, // PHY_F
        TcPort::Tc2 => 6, // PHY_G
        TcPort::Tc3 | TcPort::Tc4 => {
            return Err(String::from("translated DDI path only admits TC1/TC2"));
        }
    };
    Ok(i915::DdiEncoder {
        port: source_port,
        phy,
        output: i915::OutputType::Hdmi,
        display: i915::Platform {
            display_ver: 13,
            alderlake_p: true,
            ..i915::Platform::default()
        },
        is_tc: true,
        ..i915::DdiEncoder::default()
    })
}

/// HDMI encoder disable callback used by the TC modeset transaction. The
/// source DDI helper owns the buffer handshake; this wrapper adds checked DE
/// readback so timeout or a lost register cannot look like success.
fn disable_tc_hdmi_encoder<R: Registers, T: PollTimer>(
    registers: &R,
    timer: &T,
    port: TcPort,
) -> Result<(), String> {
    use intel_display::intel_ddi_full as i915;

    let encoder = source_tc_encoder(port)?;
    let state = i915::CrtcState {
        output: i915::OutputType::Hdmi,
        ..i915::CrtcState::default()
    };
    let control = ddi_buf_ctl(port)?;
    let source_buffer = mmio_register(control, true);
    let mut io = TcTranscoderFuncIo::buffer(registers, source_buffer, timer);
    i915::intel_ddi_buf_disable(&mut io, &encoder, &state);
    if io.read_failed || io.write_failed || io.wait_timed_out {
        return Err(String::from("translated DDI buffer disable failed"));
    }
    let value = read(registers, control)?;
    if value & DDI_ENABLE != 0 || value & DDI_IDLE == 0 {
        return Err(format!(
            "translated DDI buffer disable readback mismatch {value:#x}"
        ));
    }
    Ok(())
}

/// HDMI encoder enable callback, paired with `disable_tc_hdmi_encoder` and
/// called only while the enclosing transaction owns the old/new PHY image.
fn enable_tc_hdmi_encoder<R: Registers, T: PollTimer>(
    registers: &R,
    timer: &T,
    port: TcPort,
    output: u32,
) -> Result<(), String> {
    use intel_display::intel_ddi_full as i915;

    let encoder = source_tc_encoder(port)?;
    let state = i915::CrtcState {
        output: i915::OutputType::Hdmi,
        ..i915::CrtcState::default()
    };
    let control = ddi_buf_ctl(port)?;
    let mut io = TcTranscoderFuncIo::buffer(registers, mmio_register(control, true), timer);
    i915::intel_ddi_buf_enable(&mut io, &encoder, output);
    if io.read_failed || io.write_failed || io.wait_timed_out {
        return Err(String::from("translated DDI buffer enable failed"));
    }
    let value = read(registers, control)?;
    if value & DDI_ENABLE == 0 || value & DDI_IDLE != 0 {
        return Err(format!(
            "translated DDI buffer enable readback mismatch {value:#x}"
        ));
    }
    Ok(())
}

/// HDMI pre-enable signal-level callback from `intel_ddi_pre_enable_hdmi()`.
/// The caller must already have proved the exact ADL-P D0 waiver and VBT level.
fn pre_enable_tc_hdmi_encoder<I: DklIo + intel_display::tc::TcIo>(
    io: &I,
    port: TcPort,
    port_clock_khz: u32,
) -> Result<(), String> {
    intel_display::tc::adlp_tc_dkl_hdmi_set_signal_levels(
        io,
        port,
        port_clock_khz,
        5,
        intel_display::tc::Wa16011342517::Active,
    )
    .map_err(|error| format!("TC HDMI signal-level programming failed: {error:?}"))
}

/// Concrete encoder callback adapter for the currently admitted HDMI route.
/// These methods are transaction-bound: callers must retain the firmware
/// before-image and not expose their partial success as a completed atomic
/// commit. DP and cold Type-C are intentionally not represented by this type.
struct TcHdmiEncoderOps<'a, R, T> {
    registers: &'a R,
    timer: &'a T,
    port: TcPort,
    phase: TcHdmiEncoderPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TcHdmiEncoderPhase {
    Active,
    Disabled,
    PostDisabled,
    Prepared,
    Enabled,
}

fn advance_hdmi_encoder_phase(
    phase: &mut TcHdmiEncoderPhase,
    expected: TcHdmiEncoderPhase,
    next: TcHdmiEncoderPhase,
) -> Result<(), String> {
    if *phase != expected {
        return Err(format!(
            "TC HDMI callback out of order: phase={phase:?}, expected={expected:?}"
        ));
    }
    *phase = next;
    Ok(())
}

impl<R: Registers, T: PollTimer> TcHdmiEncoderOps<'_, R, T> {
    fn disable(&mut self) -> Result<(), String> {
        if self.phase != TcHdmiEncoderPhase::Active {
            return Err(String::from("TC HDMI disable requires active encoder"));
        }
        disable_tc_hdmi_encoder(self.registers, self.timer, self.port)?;
        advance_hdmi_encoder_phase(
            &mut self.phase,
            TcHdmiEncoderPhase::Active,
            TcHdmiEncoderPhase::Disabled,
        )
    }

    fn post_disable(&mut self) -> Result<(), String> {
        if self.phase != TcHdmiEncoderPhase::Disabled {
            return Err(String::from(
                "TC HDMI post-disable requires disabled encoder",
            ));
        }
        post_disable_tc_hdmi_encoder(self.registers, self.port)?;
        advance_hdmi_encoder_phase(
            &mut self.phase,
            TcHdmiEncoderPhase::Disabled,
            TcHdmiEncoderPhase::PostDisabled,
        )
    }

    fn pre_enable(&mut self, port_clock_khz: u32) -> Result<(), String> {
        if self.phase != TcHdmiEncoderPhase::PostDisabled {
            return Err(String::from(
                "TC HDMI pre-enable requires post-disabled encoder",
            ));
        }
        pre_enable_tc_hdmi_encoder(&dkl_io(self.registers), self.port, port_clock_khz)?;
        advance_hdmi_encoder_phase(
            &mut self.phase,
            TcHdmiEncoderPhase::PostDisabled,
            TcHdmiEncoderPhase::Prepared,
        )
    }

    fn restore_pre_enable(
        &mut self,
        before: &intel_display::tc::DklPhyState,
    ) -> Result<(), String> {
        if self.phase != TcHdmiEncoderPhase::PostDisabled {
            return Err(String::from(
                "TC HDMI restore requires post-disabled encoder",
            ));
        }
        restore_signal_levels(&dkl_io(self.registers), self.port, before)?;
        advance_hdmi_encoder_phase(
            &mut self.phase,
            TcHdmiEncoderPhase::PostDisabled,
            TcHdmiEncoderPhase::Prepared,
        )
    }

    fn enable(&mut self, output: u32) -> Result<(), String> {
        if self.phase != TcHdmiEncoderPhase::Prepared {
            return Err(String::from("TC HDMI enable requires prepared encoder"));
        }
        enable_tc_hdmi_encoder(self.registers, self.timer, self.port, output)?;
        advance_hdmi_encoder_phase(
            &mut self.phase,
            TcHdmiEncoderPhase::Prepared,
            TcHdmiEncoderPhase::Enabled,
        )
    }

    /// Enable the HDMI AVI infoframe through the source HSW writer. Keep this
    /// inside the route-bound encoder adapter so an HDMI packet cannot be
    /// dispatched for a DP or unselected TC port by a future caller.
    fn enable_avi_infoframe(&self, words: &[u32; 8]) -> Result<(), String> {
        if self.phase != TcHdmiEncoderPhase::PostDisabled
            && self.phase != TcHdmiEncoderPhase::Prepared
        {
            return Err(String::from(
                "TC HDMI AVI programming is out of encoder phase",
            ));
        }
        write_pipe_a_avi(self.registers, words)
    }
}

/// Kernel adapter for the source Haswell/Gen12+ AVI-DIP writer. Only the
/// transcoder-A AVI control and its eight data dwords are addressable here.
struct TcAviIo<'a, R> {
    registers: &'a R,
    read_failed: bool,
    write_failed: bool,
}

impl<'a, R: Registers> TcAviIo<'a, R> {
    fn new(registers: &'a R) -> Self {
        Self {
            registers,
            read_failed: false,
            write_failed: false,
        }
    }

    fn allowed_register(register: u32) -> bool {
        register == 0x60200
            || ((0x60220..=0x6023c).contains(&register) && (register - 0x60220) % 4 == 0)
    }
}

impl<R: Registers> intel_display::intel_hdmi_full::HdmiIo for TcAviIo<'_, R> {
    fn register(&self, register: intel_display::intel_hdmi_full::InfoframeRegister) -> u32 {
        use intel_display::intel_hdmi_full::{InfoframeFamily, InfoframeRegister};

        match register {
            InfoframeRegister::Control {
                family: InfoframeFamily::Haswell,
                pipe: 0,
                transcoder: 0,
            } => 0x60200,
            InfoframeRegister::Data {
                family: InfoframeFamily::Haswell,
                pipe: 0,
                transcoder: 0,
                packet_type: intel_display::intel_hdmi_full::HDMI_INFOFRAME_TYPE_AVI,
                dword,
            } if dword < 8 => 0x60220 + u32::from(dword) * 4,
            _ => u32::MAX,
        }
    }

    fn read32(&mut self, register: u32) -> u32 {
        if !Self::allowed_register(register) {
            self.read_failed = true;
            return u32::MAX;
        }
        self.registers
            .read(mmio_register(register, false))
            .unwrap_or_else(|| {
                self.read_failed = true;
                u32::MAX
            })
    }

    fn write32(&mut self, register: u32, value: u32) {
        if !Self::allowed_register(register) {
            self.write_failed = true;
            return;
        }
        if !self.registers.write(mmio_register(register, true), value) {
            self.write_failed = true;
        }
    }

    fn hdmi_port_enabled(&mut self) -> bool {
        false
    }
    fn transcoder_function_enabled(&mut self, _transcoder: u8) -> bool {
        false
    }
    fn pack_infoframe(
        &mut self,
        _packet_type: u8,
        _frame: &[u8],
        _out: &mut [u8],
    ) -> Result<usize, ()> {
        Err(())
    }
    fn write_infoframe(&mut self, _packet_type: u8, _bytes: &[u8], _len: usize) {}
    fn read_infoframe(&mut self, _packet_type: u8, _bytes: &mut [u8]) {}
    fn unpack_infoframe(&mut self, _packet_type: u8, _packed: &[u8], _out: &mut [u8]) -> bool {
        false
    }
}

fn write_pipe_a_avi<R: Registers>(registers: &R, avi_words: &[u32; 8]) -> Result<(), String> {
    let mut packet = [0u8; 32];
    for (word, bytes) in avi_words.iter().zip(packet.chunks_exact_mut(4)) {
        bytes.copy_from_slice(&word.to_le_bytes());
    }
    let mut io = TcAviIo::new(registers);
    intel_display::intel_hdmi_full::hsw_write_infoframe(
        &mut io,
        0,
        13,
        false,
        false,
        intel_display::intel_hdmi_full::HDMI_INFOFRAME_TYPE_AVI,
        &packet,
        packet.len(),
    );
    if io.read_failed || io.write_failed {
        return Err(String::from("translated Pipe-A AVI infoframe write failed"));
    }
    let control = registers
        .read(mmio_register(0x60200, false))
        .ok_or_else(|| String::from("Pipe-A AVI control readback unavailable"))?;
    if control
        & intel_display::intel_hdmi_full::hsw_infoframe_enable(
            intel_display::intel_hdmi_full::HDMI_INFOFRAME_TYPE_AVI,
        )
        == 0
    {
        return Err(String::from(
            "Pipe-A AVI enable bit missing after source write",
        ));
    }
    for (index, word) in avi_words.iter().copied().enumerate() {
        let register = mmio_register(0x60220 + index as u32 * 4, false);
        if registers.read(register) != Some(word) {
            return Err(format!(
                "Pipe-A AVI data readback mismatch at dword {index}"
            ));
        }
    }
    Ok(())
}

fn enable_pipe_a_transcoder_clock<R: Registers>(registers: &R, port: TcPort) -> Result<(), String> {
    use intel_display::intel_ddi_full as i915;

    let encoder = source_tc_encoder(port)?;
    let state = i915::CrtcState {
        cpu_transcoder: i915::Transcoder::A,
        output: i915::OutputType::Hdmi,
        ..i915::CrtcState::default()
    };
    let mut io = TcTranscoderFuncIo::clock(registers, ddi::TRANS_CLK_SEL_A);
    i915::intel_ddi_enable_transcoder_clock(&mut io, &encoder, &state);
    if io.write_failed {
        return Err(String::from(
            "translated Pipe-A transcoder clock write failed",
        ));
    }
    let readback = registers
        .read(ddi::TRANS_CLK_SEL_A)
        .ok_or_else(|| String::from("Pipe-A transcoder clock readback unavailable"))?;
    let expected = (6 + u32::from(port.index())) << 28;
    if readback != expected {
        return Err(format!(
            "translated transcoder clock readback mismatch {readback:#x}, expected {expected:#x}"
        ));
    }
    Ok(())
}

fn disable_pipe_a_transcoder_clock<R: Registers>(
    registers: &R,
    port: TcPort,
) -> Result<(), String> {
    use intel_display::intel_ddi_full as i915;

    let encoder = source_tc_encoder(port)?;
    let state = i915::CrtcState {
        cpu_transcoder: i915::Transcoder::A,
        output: i915::OutputType::Hdmi,
        ..i915::CrtcState::default()
    };
    let mut io = TcTranscoderFuncIo::clock(registers, ddi::TRANS_CLK_SEL_A);
    i915::intel_ddi_disable_transcoder_clock(&mut io, &encoder, &state);
    if io.write_failed {
        return Err(String::from(
            "translated Pipe-A transcoder clock disable failed",
        ));
    }
    let readback = registers
        .read(ddi::TRANS_CLK_SEL_A)
        .ok_or_else(|| String::from("Pipe-A transcoder clock readback unavailable"))?;
    if readback != 0 {
        return Err(format!(
            "translated transcoder clock disable readback mismatch {readback:#x}"
        ));
    }
    Ok(())
}

/// Encoder post-disable callback (`intel_ddi_post_disable()`): drop the
/// selected transcoder clock only after the source DDI disable handshake.
fn post_disable_tc_hdmi_encoder<R: Registers>(registers: &R, port: TcPort) -> Result<(), String> {
    disable_pipe_a_transcoder_clock(registers, port)
}

fn disable_pipe_a_transcoder<R: Registers>(registers: &R) -> Result<(), String> {
    use intel_display::intel_ddi_full as i915;

    let encoder = i915::DdiEncoder {
        output: i915::OutputType::Hdmi,
        display: i915::Platform {
            display_ver: 13,
            alderlake_p: true,
            ..i915::Platform::default()
        },
        ..i915::DdiEncoder::default()
    };
    let state = i915::CrtcState {
        cpu_transcoder: i915::Transcoder::A,
        output: i915::OutputType::Hdmi,
        master_transcoder: i915::Transcoder::Invalid,
        mst_master_transcoder: i915::Transcoder::Invalid,
        mst_master: false,
        ..i915::CrtcState::default()
    };
    let mut io = TcTranscoderFuncIo::new(registers);
    i915::intel_ddi_disable_transcoder_func(&mut io, &encoder, &state);
    if io.read_failed || io.write_failed {
        return Err(String::from("translated Pipe-A transcoder disable failed"));
    }
    let ctl2 = registers
        .read(ddi::TRANS_DDI_FUNC_CTL2_A)
        .ok_or_else(|| String::from("Pipe-A FUNC_CTL2 readback unavailable"))?;
    let ctl = registers
        .read(ddi::TRANS_DDI_FUNC_CTL_A)
        .ok_or_else(|| String::from("Pipe-A FUNC_CTL readback unavailable"))?;
    if ctl2 != 0 || ctl & DDI_ENABLE != 0 {
        return Err(format!(
            "translated transcoder disable readback mismatch ctl2={ctl2:#x} ctl={ctl:#x}"
        ));
    }
    Ok(())
}

fn enable_pipe_a_transcoder<R: Registers>(
    registers: &R,
    port: TcPort,
    drm_mode_flags: u32,
) -> Result<(), String> {
    use intel_display::intel_ddi_full as i915;

    // The source encoder's `port` enum encodes DDI selectors 1..N. On this
    // display-13 DKL route, TC1/TC2 occupy selector values 4/5, corresponding
    // to source enum entries D/E (see `tgl_transcoder_port_select`).
    let encoder = source_tc_encoder(port)?;
    let source_port = encoder.port;
    let state = i915::CrtcState {
        cpu_transcoder: i915::Transcoder::A,
        output: i915::OutputType::Hdmi,
        pipe_bpp: 24,
        mode_flags: drm_mode_flags,
        has_hdmi_sink: true,
        master_transcoder: i915::Transcoder::Invalid,
        mst_master_transcoder: i915::Transcoder::Invalid,
        ..i915::CrtcState::default()
    };
    let mut io = TcTranscoderFuncIo::new(registers);
    i915::intel_ddi_enable_transcoder_func(&mut io, &encoder, &state);
    if io.read_failed || io.write_failed {
        return Err(String::from("translated Pipe-A transcoder enable failed"));
    }
    let ctl2 = registers
        .read(ddi::TRANS_DDI_FUNC_CTL2_A)
        .ok_or_else(|| String::from("Pipe-A FUNC_CTL2 readback unavailable"))?;
    let ctl = registers
        .read(ddi::TRANS_DDI_FUNC_CTL_A)
        .ok_or_else(|| String::from("Pipe-A FUNC_CTL readback unavailable"))?;
    let expected = (1 << 31)
        | ((u32::from(source_port.index()) + 1) << 27)
        | (drm_mode_flags & (1 << 0)) << 16
        | (drm_mode_flags & (1 << 2)) << 15;
    if ctl2 != 0 || ctl != expected {
        return Err(format!(
            "translated transcoder enable readback mismatch ctl2={ctl2:#x} ctl={ctl:#x} \
             expected={expected:#x}"
        ));
    }
    Ok(())
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
    pixel_format: u32,
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
    mut dpll_lifecycle: Option<&mut dyn DpllLifecycle>,
    mut clock_lifecycle: Option<&mut dyn ClockLifecycle>,
    restore_clock: bool,
    display_writes_started: &mut bool,
) -> Result<(), String> {
    *display_writes_started = false;
    let cpp = match pixel_format {
        intel_display::universal_plane::XRGB8888 => 4,
        intel_display::universal_plane::RGB565 => 2,
        _ => {
            return Err(String::from(
                "TC primary plane pixel format is not admitted",
            ));
        }
    };
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
        || pitch < u32::from(mode.hdisplay) * cpp
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
    let mut encoder_ops = TcHdmiEncoderOps {
        registers: r,
        timer,
        port,
        phase: TcHdmiEncoderPhase::Active,
    };
    let buffer = read(r, control_offset)?;
    let plane_surface = pipe::PlaneSurface {
        ggtt_address: u64::from(surface),
        stride_bytes: pitch,
    };
    let pipe_program = match watermark {
        Some(watermark) => pipe::compute_with_watermarks_format(
            pipe::Pipe::A,
            mode,
            plane_surface,
            watermark,
            pixel_format,
        ),
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
    disable_pipe_a_transcoder(r)?;
    encoder_ops.disable()?;
    encoder_ops.post_disable()?;
    if let Some(lifecycle) = clock_lifecycle.as_deref_mut() {
        lifecycle.adjust(mode.clock_khz, restore_clock)?;
    }
    if let Some(lifecycle) = dpll_lifecycle.as_deref_mut() {
        lifecycle.disable()?;
    } else {
        dkl_pll_off(r, timer, port)?;
    }

    for planned in shadow {
        write(r, planned.register.offset(), planned.value)?;
    }

    if let Some(lifecycle) = dpll_lifecycle.as_deref_mut() {
        lifecycle.enable()?;
    } else {
        dkl_pll_on(r, timer, port, pll, afc_startup)?;
    }
    enable_pipe_a_transcoder_clock(r, port)?;
    encoder_ops.enable_avi_infoframe(avi)?;
    let mode_flags =
        u32::from(function & (1 << 16) != 0) | (u32::from(function & (1 << 17) != 0) << 2);
    enable_pipe_a_transcoder(r, port, mode_flags)?;
    // Linux intel_ddi_enable_hdmi sets DKL levels before enabling DDI_BUF.
    // Exact N305 D0 admission is the source Wa_16011342517 applicability proof;
    // VBT level5 was checked at the outer native handoff. Unknown revisions
    // cannot reach this restricted path.
    if let Some(before) = restore_phy {
        encoder_ops.restore_pre_enable(before)?;
    } else {
        encoder_ops.pre_enable(mode.clock_khz)?;
    }
    // HDMI on ADL-P/DKL has zero DP lanes and retains board lane reversal. TC
    // PHY ownership remains asserted; no inferred swing or USB-C mux is set.
    let output = (buffer & DDI_LANE_REVERSAL) | DDI_TC_PHY_OWNERSHIP;
    encoder_ops.enable(output)?;
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

pub(super) fn program_native<R: Registers + Send + Sync, T: PollTimer>(
    r: &R,
    timer: &T,
    port: TcPort,
    mode: &Mode,
    pitch: u32,
    pixel_format: u32,
    surface: u32,
    watermark: Option<pipe::WatermarkConfig>,
    avi: &[u32; 8],
    edid: &[u8],
    mut dpll_lifecycle: Option<&mut dyn DpllLifecycle>,
    mut clock_lifecycle: Option<&mut dyn ClockLifecycle>,
    display_writes_started: &mut bool,
    power: &mut dyn super::power::NativePowerOps,
    cleanup: &mut dyn FnMut(bool) -> Result<(), String>,
    verify: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    *display_writes_started = false;
    let cpp = match pixel_format {
        intel_display::universal_plane::XRGB8888 => 4,
        intel_display::universal_plane::RGB565 => 2,
        _ => {
            return Err(String::from(
                "TC primary plane pixel format is not admitted",
            ));
        }
    };
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
        || pitch < u32::from(mode.hdisplay) * cpp
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
    let encoder_ops = TcHdmiEncoderOps {
        registers: r,
        timer,
        port,
        phase: TcHdmiEncoderPhase::Active,
    };
    let buffer = read(r, control_offset)?;
    let plane_surface = pipe::PlaneSurface {
        ggtt_address: u64::from(surface),
        stride_bytes: pitch,
    };
    let pipe_program = match watermark {
        Some(watermark) => pipe::compute_with_watermarks_format(
            pipe::Pipe::A,
            mode,
            plane_surface,
            watermark,
            pixel_format,
        ),
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
            planned.value
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
    let mut tail = TcNativeTail {
        r,
        timer,
        port,
        mode,
        surface,
        function,
        buffer,
        old_plane,
        old_surface,
        shadow,
        pipe_program,
        encoder: encoder_ops,
        avi,
        dpll: dpll_lifecycle
            .take()
            .ok_or_else(|| String::from("native DPLL lifecycle absent"))?,
        clock: clock_lifecycle
            .take()
            .ok_or_else(|| String::from("native CDCLK lifecycle absent"))?,
        power,
        cleanup,
        verify,
        writes: display_writes_started,
        stopped: false,
    };
    super::native_modeset_ops::run_native_commit_tail(
        &mut tail,
        &[intel_display::power_map::PowerDomain::PipeA],
    )
    .map_err(|e| format!("native commit-tail failed: {e:?}"))?;
    Ok(())
}

struct TcNativeTail<'a, R, T> {
    r: &'a R,
    timer: &'a T,
    port: TcPort,
    mode: &'a Mode,
    surface: u32,
    function: u32,
    buffer: u32,
    old_plane: u32,
    old_surface: u32,
    shadow: Vec<pipe::PlannedWrite>,
    pipe_program: pipe::PipeProgram,
    encoder: TcHdmiEncoderOps<'a, R, T>,
    avi: &'a [u32; 8],
    dpll: &'a mut dyn DpllLifecycle,
    clock: &'a mut dyn ClockLifecycle,
    power: &'a mut dyn super::power::NativePowerOps,
    cleanup: &'a mut dyn FnMut(bool) -> Result<(), String>,
    verify: &'a mut dyn FnMut() -> Result<(), String>,
    writes: &'a mut bool,
    stopped: bool,
}
impl<R: Registers, T: PollTimer> super::power::NativePowerOps for TcNativeTail<'_, R, T> {
    fn power_domain_get(
        &mut self,
        d: intel_display::power_map::PowerDomain,
    ) -> Result<(), super::power::PowerError> {
        self.power.power_domain_get(d)
    }
    fn power_domain_put(
        &mut self,
        d: intel_display::power_map::PowerDomain,
    ) -> Result<(), super::power::PowerError> {
        self.power.power_domain_put(d)
    }
    fn dc_state_exit(&mut self) -> Result<(), super::power::PowerError> {
        self.power.dc_state_exit()
    }
    fn dc_state_enter(&mut self) -> Result<bool, super::power::PowerError> {
        self.power.dc_state_enter()
    }
    fn set_dc_state_target(&mut self, d: u32) -> Result<bool, super::power::PowerError> {
        self.power.set_dc_state_target(d)
    }
}
impl<R: Registers + Send + Sync, T: PollTimer> super::native_modeset_ops::NativeCommitTailOps
    for TcNativeTail<'_, R, T>
{
    type Error = String;
    fn disable_phase(&mut self) -> Result<(), String> {
        super::audio::before_link_disable(self.r, self.timer, self.port)?;
        *self.writes = true;
        write(
            self.r,
            p::PLANE_CTL_A.offset(),
            self.old_plane & !PIPE_ENABLE,
        )?;
        write(self.r, p::PLANE_SURF_A.offset(), self.old_surface)?;
        wait_two_frames(self.r, self.timer)?;
        write(
            self.r,
            p::PIPECONF_A.offset(),
            read(self.r, p::PIPECONF_A.offset())? & !(PIPE_ENABLE | PIPE_RUNNING),
        )?;
        poll(
            self.r,
            self.timer,
            p::PIPECONF_A.offset(),
            PIPE_RUNNING,
            0,
            100_000,
        )?;
        disable_pipe_a_transcoder(self.r)?;
        self.encoder.disable()?;
        self.encoder.post_disable()?;
        self.dpll.disable()
    }
    fn cdclk_pre_plane(&mut self) -> Result<(), String> {
        self.clock.adjust(self.mode.clock_khz, false)
    }
    fn dpll_phase(&mut self) -> Result<(), String> {
        self.dpll.enable()
    }
    fn encoder_pre_enable_phase(&mut self) -> Result<(), String> {
        enable_pipe_a_transcoder_clock(self.r, self.port)?;
        self.encoder.enable_avi_infoframe(self.avi)?;
        self.encoder.pre_enable(self.mode.clock_khz)
    }
    fn crtc_enable_phase(&mut self) -> Result<(), String> {
        for w in &self.shadow {
            write(self.r, w.register.offset(), w.value)?;
        }
        let flags = u32::from(self.function & (1 << 16) != 0)
            | (u32::from(self.function & (1 << 17) != 0) << 2);
        enable_pipe_a_transcoder(self.r, self.port, flags)?;
        self.encoder
            .enable((self.buffer & DDI_LANE_REVERSAL) | DDI_TC_PHY_OWNERSHIP)?;
        write(
            self.r,
            p::PIPECONF_A.offset(),
            read(self.r, p::PIPECONF_A.offset())? | PIPE_ENABLE,
        )?;
        poll(
            self.r,
            self.timer,
            p::PIPECONF_A.offset(),
            PIPE_RUNNING,
            PIPE_RUNNING,
            100_000,
        )
    }
    fn plane_update_phase(&mut self) -> Result<(), String> {
        write(self.r, p::PLANE_CTL_A.offset(), self.pipe_program.plane.ctl)?;
        write(
            self.r,
            p::PLANE_SURF_A.offset(),
            self.pipe_program.plane.surf,
        )?;
        prove_new_scanout(self.r, self.timer, self.surface)
    }
    fn cdclk_post_plane(&mut self) -> Result<(), String> {
        let current = super::clk::observe(self.r).map_err(|e| format!("CDCLK readback: {e:?}"))?;
        if !current.usable() || current.cdclk_khz < self.mode.clock_khz {
            return Err(String::from("native CDCLK insufficient"));
        }
        (self.verify)() // No post-plane decrease was requested by this single-pipe plan.
    }
    fn rollback_phase(
        &mut self,
        phase: super::native_modeset_ops::NativeCommitPhase,
    ) -> Result<(), String> {
        use super::native_modeset_ops::NativeCommitPhase as P;
        match phase {
            P::PlaneUpdate => {
                write(
                    self.r,
                    p::PLANE_CTL_A.offset(),
                    read(self.r, p::PLANE_CTL_A.offset())? & !PIPE_ENABLE,
                )?;
                write(self.r, p::PLANE_SURF_A.offset(), self.old_surface)
            }
            P::CrtcEnable => {
                // Enable may have failed after a write: disable unconditionally.
                disable_tc_hdmi_encoder(self.r, self.timer, self.port)?;
                write(
                    self.r,
                    p::PIPECONF_A.offset(),
                    read(self.r, p::PIPECONF_A.offset())? & !(PIPE_ENABLE | PIPE_RUNNING),
                )?;
                poll(
                    self.r,
                    self.timer,
                    p::PIPECONF_A.offset(),
                    PIPE_RUNNING,
                    0,
                    100_000,
                )?;
                disable_pipe_a_transcoder(self.r)
            }
            P::EncoderPreEnable => post_disable_tc_hdmi_encoder(self.r, self.port),
            P::Dpll => self.dpll.rollback_new(),
            P::CdclkPrePlane => self.clock.adjust(self.mode.clock_khz, true),
            P::Disable => {
                // Prove DMA quiescence even when the initial disable failed.
                if !*self.writes {
                    self.stopped = true;
                    return self.dpll.release_new();
                }
                write(
                    self.r,
                    p::PLANE_CTL_A.offset(),
                    read(self.r, p::PLANE_CTL_A.offset())? & !PIPE_ENABLE,
                )?;
                write(self.r, p::PLANE_SURF_A.offset(), self.old_surface)?;
                write(
                    self.r,
                    p::PIPECONF_A.offset(),
                    read(self.r, p::PIPECONF_A.offset())? & !(PIPE_ENABLE | PIPE_RUNNING),
                )?;
                poll(
                    self.r,
                    self.timer,
                    p::PIPECONF_A.offset(),
                    PIPE_RUNNING,
                    0,
                    100_000,
                )?;
                self.stopped = true;
                let encoder = disable_tc_hdmi_encoder(self.r, self.timer, self.port)
                    .and_then(|()| disable_pipe_a_transcoder(self.r))
                    .and_then(|()| post_disable_tc_hdmi_encoder(self.r, self.port));
                let reservation = self.dpll.release_new();
                encoder.and(reservation)
            }
            P::CdclkPostPlane => Ok(()), // Readback only, no resource acquired.
        }
    }
    fn rollback_framebuffer(&mut self) -> Result<(), String> {
        (self.cleanup)(self.stopped)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeMap, vec::Vec};
    use core::cell::RefCell;

    use super::source_hdmi_tmds_clock_with_limit;
    use crate::drm::modes::{Mode, ModeFlags, TimingSource};

    #[test]
    fn hdmi_encoder_callbacks_enforce_source_phase_order() {
        let mut phase = super::TcHdmiEncoderPhase::Active;
        assert!(
            super::advance_hdmi_encoder_phase(
                &mut phase,
                super::TcHdmiEncoderPhase::Disabled,
                super::TcHdmiEncoderPhase::PostDisabled,
            )
            .is_err()
        );
        assert_eq!(phase, super::TcHdmiEncoderPhase::Active);
        for (expected, next) in [
            (
                super::TcHdmiEncoderPhase::Active,
                super::TcHdmiEncoderPhase::Disabled,
            ),
            (
                super::TcHdmiEncoderPhase::Disabled,
                super::TcHdmiEncoderPhase::PostDisabled,
            ),
            (
                super::TcHdmiEncoderPhase::PostDisabled,
                super::TcHdmiEncoderPhase::Prepared,
            ),
            (
                super::TcHdmiEncoderPhase::Prepared,
                super::TcHdmiEncoderPhase::Enabled,
            ),
        ] {
            super::advance_hdmi_encoder_phase(&mut phase, expected, next).unwrap();
        }
        assert_eq!(phase, super::TcHdmiEncoderPhase::Enabled);
    }

    #[derive(Default)]
    struct TranscoderModel {
        values: RefCell<BTreeMap<u32, u32>>,
        writes: RefCell<Vec<(u32, u32)>>,
    }

    impl super::Registers for TranscoderModel {
        fn read(&self, register: super::Register) -> Option<u32> {
            self.values.borrow().get(&register.offset()).copied()
        }

        fn read64(&self, _register: super::Register) -> Option<u64> {
            None
        }

        fn write(&self, register: super::Register, value: u32) -> bool {
            let value = if register.offset() == 0x64300 {
                if value & super::DDI_ENABLE != 0 {
                    value & !super::DDI_IDLE
                } else {
                    value | super::DDI_IDLE
                }
            } else {
                value
            };
            self.values.borrow_mut().insert(register.offset(), value);
            self.writes.borrow_mut().push((register.offset(), value));
            true
        }
    }

    struct Timer;
    impl super::PollTimer for Timer {
        fn now_micros(&self) -> u64 {
            0
        }
        fn pause(&self) {}
    }

    #[test]
    fn active_tc_path_uses_source_transcoder_control_order() {
        let registers = TranscoderModel::default();
        let tc1_select = 4 << 27;
        registers.values.borrow_mut().insert(
            super::ddi::TRANS_DDI_FUNC_CTL_A.offset(),
            (1 << 31) | tc1_select,
        );
        registers
            .values
            .borrow_mut()
            .insert(super::ddi::TRANS_DDI_FUNC_CTL2_A.offset(), u32::MAX);

        super::enable_pipe_a_transcoder(&registers, super::TcPort::Tc1, (1 << 0) | (1 << 2))
            .unwrap();
        let expected_enabled = (1 << 31) | tc1_select | (1 << 16) | (1 << 17);
        assert_eq!(
            registers
                .values
                .borrow()
                .get(&super::ddi::TRANS_DDI_FUNC_CTL_A.offset()),
            Some(&expected_enabled)
        );
        assert_eq!(
            *registers.writes.borrow(),
            [
                (super::ddi::TRANS_DDI_FUNC_CTL2_A.offset(), 0),
                (super::ddi::TRANS_DDI_FUNC_CTL_A.offset(), expected_enabled),
            ]
        );

        registers.writes.borrow_mut().clear();
        super::disable_pipe_a_transcoder(&registers).unwrap();
        assert_eq!(
            registers
                .values
                .borrow()
                .get(&super::ddi::TRANS_DDI_FUNC_CTL_A.offset()),
            Some(&((1 << 16) | (1 << 17)))
        );
        assert_eq!(
            *registers.writes.borrow(),
            [
                (super::ddi::TRANS_DDI_FUNC_CTL2_A.offset(), 0),
                (
                    super::ddi::TRANS_DDI_FUNC_CTL_A.offset(),
                    (1 << 16) | (1 << 17)
                ),
            ]
        );

        registers.writes.borrow_mut().clear();
        super::enable_pipe_a_transcoder_clock(&registers, super::TcPort::Tc1).unwrap();
        assert_eq!(
            registers
                .values
                .borrow()
                .get(&super::ddi::TRANS_CLK_SEL_A.offset()),
            Some(&(6 << 28))
        );
        assert_eq!(
            *registers.writes.borrow(),
            [(super::ddi::TRANS_CLK_SEL_A.offset(), 6 << 28)]
        );
        registers.writes.borrow_mut().clear();
        super::disable_pipe_a_transcoder_clock(&registers, super::TcPort::Tc1).unwrap();
        assert_eq!(
            registers
                .values
                .borrow()
                .get(&super::ddi::TRANS_CLK_SEL_A.offset()),
            Some(&0)
        );
        assert_eq!(
            *registers.writes.borrow(),
            [(super::ddi::TRANS_CLK_SEL_A.offset(), 0)]
        );
    }

    #[test]
    fn active_tc_path_uses_source_ddi_buffer_handshakes() {
        use intel_display::intel_ddi_full as i915;

        let registers = TranscoderModel::default();
        let buffer = super::mmio_register(0x64300, true);
        registers
            .values
            .borrow_mut()
            .insert(buffer.offset(), super::DDI_IDLE | (1 << 16));
        let encoder = super::source_tc_encoder(super::TcPort::Tc1).unwrap();
        let state = i915::CrtcState {
            output: i915::OutputType::Hdmi,
            ..i915::CrtcState::default()
        };
        let timer = Timer;
        let mut io = super::TcTranscoderFuncIo::buffer(&registers, buffer, &timer);

        i915::intel_ddi_buf_disable(&mut io, &encoder, &state);
        assert!(!io.read_failed && !io.write_failed && !io.wait_timed_out);
        assert_eq!(
            registers.values.borrow().get(&buffer.offset()),
            Some(&((1 << 16) | super::DDI_IDLE))
        );

        registers.writes.borrow_mut().clear();
        i915::intel_ddi_buf_enable(&mut io, &encoder, (1 << 16) | (1 << 6));
        assert!(!io.read_failed && !io.write_failed && !io.wait_timed_out);
        assert_eq!(
            registers.values.borrow().get(&buffer.offset()),
            Some(&((1 << 16) | (1 << 6) | super::DDI_ENABLE))
        );
        assert_eq!(
            *registers.writes.borrow(),
            [(buffer.offset(), (1 << 16) | (1 << 6) | super::DDI_ENABLE)]
        );
    }

    #[test]
    fn active_tc_path_uses_source_hsw_avi_writer() {
        let registers = TranscoderModel::default();
        let control = super::mmio_register(0x60200, true);
        registers
            .values
            .borrow_mut()
            .insert(control.offset(), (1 << 31) | (1 << 12) | 3);
        let avi = [
            0x0403_0201,
            0x0807_0605,
            0x0c0b_0a09,
            0x100f_0e0d,
            0x1413_1211,
            0x1817_1615,
            0x1c1b_1a19,
            0x201f_1e1d,
        ];

        super::write_pipe_a_avi(&registers, &avi).unwrap();

        assert_eq!(
            registers.values.borrow().get(&control.offset()),
            Some(&((1 << 31) | (1 << 12) | 3))
        );
        for (index, word) in avi.iter().copied().enumerate() {
            assert_eq!(
                registers.values.borrow().get(&(0x60220 + index as u32 * 4)),
                Some(&word)
            );
        }
        assert_eq!(
            registers.writes.borrow().first(),
            Some(&(0x60200, (1 << 31) | 3))
        );
        assert_eq!(
            registers.writes.borrow().last(),
            Some(&(0x60200, (1 << 31) | (1 << 12) | 3))
        );
    }

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
        assert_eq!(
            source_hdmi_tmds_clock_with_limit(
                &mode(148_500),
                None,
                intel_display::intel_hdmi_full::HdmiPortClass::TypeC,
            ),
            Some(148_500)
        );
        assert_eq!(
            source_hdmi_tmds_clock_with_limit(
                &mode(300_000),
                None,
                intel_display::intel_hdmi_full::HdmiPortClass::TypeC,
            ),
            Some(300_000)
        );
        assert_eq!(
            source_hdmi_tmds_clock_with_limit(
                &mode(300_001),
                None,
                intel_display::intel_hdmi_full::HdmiPortClass::TypeC,
            ),
            None
        );
        assert_eq!(
            source_hdmi_tmds_clock_with_limit(
                &mode(24_999),
                None,
                intel_display::intel_hdmi_full::HdmiPortClass::TypeC,
            ),
            None
        );
        assert_eq!(
            source_hdmi_tmds_clock_with_limit(
                &mode(148_500),
                Some(100_000),
                intel_display::intel_hdmi_full::HdmiPortClass::TypeC,
            ),
            None
        );
        assert_eq!(
            source_hdmi_tmds_clock_with_limit(
                &mode(148_500),
                Some(200_000),
                intel_display::intel_hdmi_full::HdmiPortClass::TypeC,
            ),
            Some(148_500)
        );
    }
}
