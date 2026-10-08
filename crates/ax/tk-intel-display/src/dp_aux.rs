// SPDX-License-Identifier: MIT
// Copyright © 2020-2021 Intel Corporation
//! Source-ordered translation of Linux 7.2.3 `intel_dp_aux.c`.
//!
//! Register addressing and DRM/PPS/power framework services are supplied by
//! `DpAuxIo`; transfer retry, packing, channel selection, and cleanup ordering
//! remain here. The API intentionally does not invent a default MMIO backend.

use alloc::string::String;

const BARE_ADDRESS_SIZE: usize = 3;
const HEADER_SIZE: usize = 4;
const AUX_MAX_PAYLOAD: usize = 20;
const SEND_BUSY: u32 = 1 << 31;
const DONE: u32 = 1 << 30;
const INTERRUPT: u32 = 1 << 29;
const TIMEOUT_ERROR: u32 = 1 << 28;
const RECEIVE_ERROR: u32 = 1 << 25;
const MESSAGE_SIZE_MASK: u32 = 0x01f0_0000;
const PRECHARGE_MASK: u32 = 0x000f_0000;
const POWER_REQUEST: u32 = 1 << 19;
const AKSV_SELECT: u32 = 1 << 15;
const TBT_IO: u32 = 1 << 11;
const BIT_CLOCK_2X_MASK: u32 = 0x7ff;
const FW_SYNC_MASK: u32 = 0x3e0;
const SYNC_MASK: u32 = 0x1f;
const AUX_I2C_MOT: u8 = 0x04;
const AUX_NATIVE_WRITE: u8 = 0x08;
const AUX_NATIVE_READ: u8 = 0x09;
const AUX_I2C_WRITE: u8 = 0x00;
const AUX_I2C_READ: u8 = 0x01;
const AUX_I2C_WRITE_STATUS_UPDATE: u8 = 0x02;
const AUX_HDCP_AKSV: u32 = 0x6800a;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuxChannel {
    None,
    A,
    B,
    C,
    D,
    E,
    F,
    UsbC1,
    UsbC2,
    UsbC3,
    UsbC4,
    UsbC5,
    UsbC6,
    XelpdpD,
    XelpdpE,
    XelpdpF,
}

impl AuxChannel {
    const fn ordinal(self) -> i32 {
        match self {
            Self::None => -1,
            Self::A => 0,
            Self::B => 1,
            Self::C => 2,
            Self::D => 3,
            Self::E => 4,
            Self::F => 5,
            Self::UsbC1 => 6,
            Self::UsbC2 => 7,
            Self::UsbC3 => 8,
            Self::UsbC4 => 9,
            Self::UsbC5 | Self::XelpdpD => 10,
            Self::UsbC6 | Self::XelpdpE => 11,
            Self::XelpdpF => 12,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuxGeneration {
    G4x,
    Valleyview,
    PchSplit,
    Skylake,
    Tigerlake,
    Xelpdp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuxRegister {
    pub generation: AuxGeneration,
    pub channel: AuxChannel,
    /// `None` denotes the control register; `Some(n)` denotes AUX data DW n.
    pub data_index: Option<u8>,
    /// PCH-split AUX B/C/D lives in the PCH register block.
    pub pch: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuxPlatform {
    pub display_version: u8,
    pub has_pch_split: bool,
    pub valleyview: bool,
    pub cherryview: bool,
    pub broadwell: bool,
    pub haswell: bool,
    pub pch_lpt_h: bool,
    pub rawclk_khz: u32,
    pub cdclk_khz: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuxError {
    Invalid,
    TooBig,
    Busy,
    Io,
    Timeout,
    Disconnected,
    Power,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuxState {
    pub channel: AuxChannel,
    pub platform: AuxPlatform,
    pub name: String,
    pub last_busy_status: u32,
    pub pm_qos_active: bool,
    pub edp: bool,
    pub tbt_alt_mode: bool,
    pub firmware_sync_length_quirk: bool,
    pub aux_domain: u16,
    pub encoder_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuxMessage<'a> {
    pub request: u8,
    pub address: u32,
    pub size: usize,
    pub buffer: Option<&'a [u8]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuxReply {
    pub bytes: i32,
    pub reply: u8,
    pub short_write: Option<u8>,
}

/// Kernel integration hooks. The adapter maps register selectors to the
/// platform's exact AUX registers and preserves posting-read/MMIO behavior.
pub trait DpAuxIo {
    fn lock_port(&mut self) -> Result<(), AuxError>;
    fn unlock_port(&mut self);
    fn connected(&mut self) -> bool;
    fn power_get(&mut self, domain: u16) -> Result<(), AuxError>;
    fn power_put_async(&mut self, domain: u16);
    fn pps_lock(&mut self) -> Result<bool, AuxError>;
    fn pps_vdd_on(&mut self) -> bool;
    fn pps_check_power(&mut self);
    fn pps_vdd_off(&mut self);
    fn pps_unlock(&mut self);
    fn cpu_latency_qos(&mut self, latency_us: Option<u32>);
    fn read(&mut self, register: AuxRegister) -> Result<u32, AuxError>;
    fn write(&mut self, register: AuxRegister, value: u32) -> Result<(), AuxError>;
    fn wait_send_done(&mut self, register: AuxRegister, timeout_ms: u32) -> Result<u32, AuxError>;
    fn sleep_ms(&mut self, milliseconds: u32);
    fn sleep_us_range(&mut self, minimum: u32, maximum: u32);
    fn trace_register(&mut self, register: AuxRegister, value: u32);
    fn diagnostic(&mut self, diagnostic: AuxDiagnostic);
    fn probe_dpcd(&mut self, enabled: bool);
    fn wake_aux_waiters(&mut self);
    fn register_aux(&mut self) -> Result<(), AuxError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuxDiagnostic {
    WaitTimeout(u32),
    NotStarted(u32),
    NotDone(u32),
    ReceiveError(u32),
    Timeout(u32),
    ForbiddenReceiveSize(usize),
    InvalidBufferSize,
    DuplicateChannel { owner: u32 },
    UsingChannel { source_is_vbt: bool },
    InvalidChannel(AuxChannel),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuxEncoder {
    pub id: u32,
    pub port: AuxChannel,
    pub aux_channel: AuxChannel,
    pub digital: bool,
}

/// Format an AUX channel name using the display-version naming rules.
// upstream: intel_dp_aux.c aux_ch_name()
pub fn aux_ch_name(display_version: u8, channel: AuxChannel) -> String {
    if display_version >= 13 && channel.ordinal() >= AuxChannel::XelpdpD.ordinal() {
        let letter = (b'D' + (channel.ordinal() - AuxChannel::XelpdpD.ordinal()) as u8) as char;
        alloc::format!("{letter}")
    } else if display_version >= 12 && channel.ordinal() >= AuxChannel::UsbC1.ordinal() {
        let number = (b'1' + (channel.ordinal() - AuxChannel::UsbC1.ordinal()) as u8) as char;
        alloc::format!("USBC{number}")
    } else {
        let letter = (b'A' + channel.ordinal().max(0) as u8) as char;
        alloc::format!("{letter}")
    }
}

/// Pack up to four bytes in the AUX channel's big-endian dword order.
// upstream: intel_dp_aux.c intel_dp_aux_pack()
pub fn intel_dp_aux_pack(source: &[u8]) -> u32 {
    let mut value = 0;
    for (index, byte) in source.iter().take(4).enumerate() {
        value |= u32::from(*byte) << ((3 - index) * 8);
    }
    value
}

/// Unpack at most four bytes in AUX register order.
// upstream: intel_dp_aux.c intel_dp_aux_unpack()
pub fn intel_dp_aux_unpack(source: u32, destination: &mut [u8]) {
    for (index, byte) in destination.iter_mut().take(4).enumerate() {
        *byte = (source >> ((3 - index) * 8)) as u8;
    }
}

fn ctl(state: &AuxState) -> AuxRegister {
    intel_dp_aux_register(state.platform, state.channel, None)
}

/// Wait for SEND_BUSY to clear, with interrupt and polling paths supplied by
/// the adapter; a timeout still returns the last status word.
// upstream: intel_dp_aux.c intel_dp_aux_wait_done()
pub fn intel_dp_aux_wait_done(io: &mut impl DpAuxIo, state: &AuxState) -> u32 {
    match io.wait_send_done(ctl(state), 10) {
        Ok(status) => status,
        Err(_) => {
            let status = io.read(ctl(state)).unwrap_or(SEND_BUSY);
            io.diagnostic(AuxDiagnostic::WaitTimeout(status));
            status
        }
    }
}

/// Return the G4x raw-clock divisor; only index zero is defined.
// upstream: intel_dp_aux.c g4x_get_aux_clock_divider()
pub fn g4x_get_aux_clock_divider(platform: AuxPlatform, index: u8) -> u32 {
    if index != 0 {
        0
    } else {
        div_round_closest(platform.rawclk_khz, 2000)
    }
}

/// Return the Ironlake/PCH-split AUX divisor, using CDCLK only for AUX A.
// upstream: intel_dp_aux.c ilk_get_aux_clock_divider()
pub fn ilk_get_aux_clock_divider(platform: AuxPlatform, channel: AuxChannel, index: u8) -> u32 {
    if index != 0 {
        return 0;
    }
    let frequency = if channel == AuxChannel::A {
        platform.cdclk_khz
    } else {
        platform.rawclk_khz
    };
    div_round_closest(frequency, 2000)
}

/// Apply the non-ULT Haswell AUX B/C/D two-divider workaround.
// upstream: intel_dp_aux.c hsw_get_aux_clock_divider()
pub fn hsw_get_aux_clock_divider(platform: AuxPlatform, channel: AuxChannel, index: u8) -> u32 {
    if channel != AuxChannel::A && platform.pch_lpt_h {
        return match index {
            0 => 63,
            1 => 72,
            _ => 0,
        };
    }
    ilk_get_aux_clock_divider(platform, channel, index)
}

/// SKL+ derives the AUX clock from CDCLK and uses a dummy nonzero divisor.
// upstream: intel_dp_aux.c skl_get_aux_clock_divider()
pub const fn skl_get_aux_clock_divider(index: u8) -> u32 {
    if index == 0 { 1 } else { 0 }
}

/// DP AUX synchronization length (16 precharge + 16 preamble symbols).
// upstream: intel_dp_aux.c intel_dp_aux_sync_len()
pub const fn intel_dp_aux_sync_len() -> u32 {
    16 + 16
}

/// Fast-wake firmware sync length, including the sink quirk adjustment.
// upstream: intel_dp_aux.c intel_dp_aux_fw_sync_len()
pub const fn intel_dp_aux_fw_sync_len(firmware_sync_quirk: bool) -> u32 {
    10 + if firmware_sync_quirk { 2 } else { 0 } + 8
}

/// Compute the G4x precharge extension in two-microsecond units.
// upstream: intel_dp_aux.c g4x_dp_aux_precharge_len()
pub const fn g4x_dp_aux_precharge_len() -> u32 {
    (intel_dp_aux_sync_len() - 10 - 16) / 2
}

/// Compose the G4x/Broadwell send-control word.
// upstream: intel_dp_aux.c g4x_get_aux_send_ctl()
pub fn g4x_get_aux_send_ctl(platform: AuxPlatform, send_bytes: usize, divider: u32) -> u32 {
    let timeout = if platform.broadwell { 1 << 26 } else { 0 };
    SEND_BUSY
        | DONE
        | INTERRUPT
        | TIMEOUT_ERROR
        | timeout
        | RECEIVE_ERROR
        | (((send_bytes as u32) << 20) & MESSAGE_SIZE_MASK)
        | ((g4x_dp_aux_precharge_len() << 16) & PRECHARGE_MASK)
        | (divider & BIT_CLOCK_2X_MASK)
}

/// Compose the SKL+ send-control word, preserving TBT and MTL power request.
// upstream: intel_dp_aux.c skl_get_aux_send_ctl()
pub fn skl_get_aux_send_ctl(state: &AuxState, send_bytes: usize, firmware_sync_quirk: bool) -> u32 {
    let mut value = SEND_BUSY
        | DONE
        | INTERRUPT
        | TIMEOUT_ERROR
        | (3 << 26)
        | RECEIVE_ERROR
        | (((send_bytes as u32) << 20) & MESSAGE_SIZE_MASK)
        | (((intel_dp_aux_fw_sync_len(firmware_sync_quirk) - 1) << 5) & FW_SYNC_MASK)
        | ((intel_dp_aux_sync_len() - 1) & SYNC_MASK);
    if state.tbt_alt_mode {
        value |= TBT_IO;
    }
    if state.platform.display_version >= 14 {
        value |= POWER_REQUEST;
    }
    value
}

/// Serialize one low-level AUX request, including power, PPS, retries, and
/// source cleanup order. Return values follow the Linux helper's negative errno
/// convention and positive receive-byte count.
// upstream: intel_dp_aux.c intel_dp_aux_xfer()
pub fn intel_dp_aux_xfer(
    io: &mut impl DpAuxIo,
    state: &mut AuxState,
    send: &[u8],
    receive: &mut [u8],
    send_flags: u32,
) -> i32 {
    if io.lock_port().is_err() {
        return -16;
    }
    if !state.edp && !io.connected() {
        io.unlock_port();
        return -6;
    }
    if io.power_get(state.aux_domain).is_err() {
        io.unlock_port();
        return -5;
    }
    let pps_locked = if state.edp || state.platform.valleyview || state.platform.cherryview {
        match io.pps_lock() {
            Ok(locked) => locked,
            Err(_) => {
                io.power_put_async(state.aux_domain);
                io.unlock_port();
                return -5;
            }
        }
    } else {
        false
    };
    let vdd = io.pps_vdd_on();
    io.cpu_latency_qos(Some(0));
    io.pps_check_power();

    let control = ctl(state);
    let mut status = 0;
    let mut attempt = 0;
    while attempt < 3 {
        status = match io.read(control) {
            Ok(value) => value,
            Err(_) => {
                status = SEND_BUSY;
                break;
            }
        };
        if status & SEND_BUSY == 0 {
            break;
        }
        io.sleep_ms(1);
        attempt += 1;
    }
    io.trace_register(control, status);
    if attempt == 3 || status & SEND_BUSY != 0 {
        let final_status = io.read(control).unwrap_or(status);
        if final_status != state.last_busy_status {
            io.diagnostic(AuxDiagnostic::NotStarted(final_status));
            state.last_busy_status = final_status;
        }
        cleanup(io, state, vdd, pps_locked);
        return -16;
    }
    if send.len() > AUX_MAX_PAYLOAD || receive.len() > AUX_MAX_PAYLOAD {
        io.diagnostic(AuxDiagnostic::InvalidBufferSize);
        cleanup(io, state, vdd, pps_locked);
        return -7;
    }

    let mut final_result = -16;
    let mut clock = 0u8;
    loop {
        let divider = aux_clock_divider(state, clock);
        if divider == 0 {
            break;
        }
        let send_control = aux_send_control(state, send.len(), divider) | send_flags;
        let mut tries = 0;
        while tries < 5 {
            let mut index = 0;
            while index < send.len() {
                let reg =
                    intel_dp_aux_register(state.platform, state.channel, Some((index / 4) as u8));
                if io
                    .write(
                        reg,
                        intel_dp_aux_pack(&send[index..(index + 4).min(send.len())]),
                    )
                    .is_err()
                {
                    final_result = -5;
                    break;
                }
                index += 4;
            }
            if final_result == -5 {
                break;
            }
            if io.write(control, send_control).is_err() {
                final_result = -5;
                break;
            }
            status = intel_dp_aux_wait_done(io, state);
            let _ = io.write(control, status | DONE | TIMEOUT_ERROR | RECEIVE_ERROR);
            if status & TIMEOUT_ERROR != 0 {
                tries += 1;
                continue;
            }
            if status & RECEIVE_ERROR != 0 {
                io.sleep_us_range(400, 500);
                tries += 1;
                continue;
            }
            if status & DONE != 0 {
                break;
            }
            tries += 1;
        }
        if final_result == -5 {
            break;
        }
        if status & DONE != 0 {
            break;
        }
        clock = clock.saturating_add(1);
    }

    if final_result != -5 {
        if status & DONE == 0 {
            io.diagnostic(AuxDiagnostic::NotDone(status));
            final_result = -16;
        } else if status & RECEIVE_ERROR != 0 {
            io.diagnostic(AuxDiagnostic::ReceiveError(status));
            final_result = -5;
        } else if status & TIMEOUT_ERROR != 0 {
            io.diagnostic(AuxDiagnostic::Timeout(status));
            final_result = -110;
        } else {
            let mut bytes = ((status & MESSAGE_SIZE_MASK) >> 20) as usize;
            if bytes == 0 || bytes > AUX_MAX_PAYLOAD {
                io.diagnostic(AuxDiagnostic::ForbiddenReceiveSize(bytes));
                final_result = -16;
            } else {
                bytes = bytes.min(receive.len());
                let mut index = 0;
                while index < bytes {
                    let reg = intel_dp_aux_register(
                        state.platform,
                        state.channel,
                        Some((index / 4) as u8),
                    );
                    match io.read(reg) {
                        Ok(value) => {
                            intel_dp_aux_unpack(value, &mut receive[index..(index + 4).min(bytes)])
                        }
                        Err(_) => {
                            final_result = -5;
                            break;
                        }
                    }
                    index += 4;
                }
                if final_result != -5 {
                    final_result = bytes as i32;
                }
            }
        }
    }
    cleanup(io, state, vdd, pps_locked);
    final_result
}

fn cleanup(io: &mut impl DpAuxIo, state: &AuxState, vdd: bool, pps_locked: bool) {
    io.cpu_latency_qos(None);
    if vdd {
        io.pps_vdd_off();
    }
    if pps_locked {
        io.pps_unlock();
    }
    io.power_put_async(state.aux_domain);
    io.unlock_port();
}

fn aux_clock_divider(state: &AuxState, index: u8) -> u32 {
    if state.platform.display_version >= 9 {
        skl_get_aux_clock_divider(index)
    } else if state.platform.broadwell || state.platform.haswell {
        hsw_get_aux_clock_divider(state.platform, state.channel, index)
    } else if state.platform.has_pch_split {
        ilk_get_aux_clock_divider(state.platform, state.channel, index)
    } else {
        g4x_get_aux_clock_divider(state.platform, index)
    }
}

fn aux_send_control(state: &AuxState, bytes: usize, divider: u32) -> u32 {
    if state.platform.display_version >= 9 {
        skl_get_aux_send_ctl(state, bytes, state.firmware_sync_length_quirk)
    } else {
        g4x_get_aux_send_ctl(state.platform, bytes, divider)
    }
}

/// Fill the four-byte DP AUX request header.
// upstream: intel_dp_aux.c intel_dp_aux_header()
pub fn intel_dp_aux_header(
    destination: &mut [u8; HEADER_SIZE],
    request: u8,
    address: u32,
    size: usize,
) {
    destination[0] = (request << 4) | ((address >> 16) as u8 & 0x0f);
    destination[1] = (address >> 8) as u8;
    destination[2] = address as u8;
    destination[3] = size.wrapping_sub(1) as u8;
}

/// Select hardware AKSV injection for the HDCP native-write address.
// upstream: intel_dp_aux.c intel_dp_aux_xfer_flags()
pub const fn intel_dp_aux_xfer_flags(request: u8, address: u32) -> u32 {
    if request & !AUX_I2C_MOT == AUX_NATIVE_WRITE && address == AUX_HDCP_AKSV {
        AKSV_SELECT
    } else {
        0
    }
}

/// Execute one DRM AUX message over the low-level channel helper.
// upstream: intel_dp_aux.c intel_dp_aux_transfer()
pub fn intel_dp_aux_transfer(
    io: &mut impl DpAuxIo,
    state: &mut AuxState,
    request: u8,
    address: u32,
    payload: Option<&[u8]>,
    receive: &mut [u8],
) -> AuxReply {
    let size = payload.map_or(receive.len(), <[u8]>::len);
    let mut header = [0; HEADER_SIZE];
    intel_dp_aux_header(&mut header, request, address, size);
    let flags = intel_dp_aux_xfer_flags(request, address);
    let request_kind = request & !AUX_I2C_MOT;
    match request_kind {
        AUX_NATIVE_WRITE | AUX_I2C_WRITE | AUX_I2C_WRITE_STATUS_UPDATE => {
            let mut tx = [0; AUX_MAX_PAYLOAD];
            let tx_size = if size != 0 {
                HEADER_SIZE + size
            } else {
                BARE_ADDRESS_SIZE
            };
            if tx_size > AUX_MAX_PAYLOAD {
                return AuxReply {
                    bytes: -7,
                    reply: 0,
                    short_write: None,
                };
            }
            tx[..HEADER_SIZE].copy_from_slice(&header);
            if let Some(data) = payload {
                if data.len() != size {
                    io.diagnostic(AuxDiagnostic::InvalidBufferSize);
                    return AuxReply {
                        bytes: -22,
                        reply: 0,
                        short_write: None,
                    };
                }
                tx[HEADER_SIZE..tx_size].copy_from_slice(data);
            }
            let mut rx = [0; 2];
            let ret = intel_dp_aux_xfer(io, state, &tx[..tx_size], &mut rx, flags);
            if ret > 0 {
                let short = if ret > 1 {
                    Some((rx[1] as usize).min(size) as u8)
                } else {
                    None
                };
                AuxReply {
                    bytes: short.map_or(size as i32, i32::from),
                    reply: rx[0] >> 4,
                    short_write: short,
                }
            } else {
                AuxReply {
                    bytes: ret,
                    reply: 0,
                    short_write: None,
                }
            }
        }
        AUX_NATIVE_READ | AUX_I2C_READ => {
            let tx_size = if size != 0 {
                HEADER_SIZE
            } else {
                BARE_ADDRESS_SIZE
            };
            let rx_size = size + 1;
            if rx_size > AUX_MAX_PAYLOAD {
                return AuxReply {
                    bytes: -7,
                    reply: 0,
                    short_write: None,
                };
            }
            let mut rx = [0; AUX_MAX_PAYLOAD];
            let ret = intel_dp_aux_xfer(io, state, &header[..tx_size], &mut rx[..rx_size], flags);
            if ret > 0 {
                let bytes = (ret as usize).saturating_sub(1).min(receive.len());
                receive[..bytes].copy_from_slice(&rx[1..1 + bytes]);
                AuxReply {
                    bytes: bytes as i32,
                    reply: rx[0] >> 4,
                    short_write: None,
                }
            } else {
                AuxReply {
                    bytes: ret,
                    reply: 0,
                    short_write: None,
                }
            }
        }
        _ => AuxReply {
            bytes: -22,
            reply: 0,
            short_write: None,
        },
    }
}

/// Resolve the Valleyview control-register selector; unsupported channels
/// deliberately fall back to AUX B after reporting the invalid case.
// upstream: intel_dp_aux.c vlv_aux_ctl_reg()
pub fn vlv_aux_ctl_reg(channel: AuxChannel) -> AuxRegister {
    map_register(AuxGeneration::Valleyview, channel, None)
}

/// Resolve one Valleyview data-register selector.
// upstream: intel_dp_aux.c vlv_aux_data_reg()
pub fn vlv_aux_data_reg(channel: AuxChannel, index: u8) -> AuxRegister {
    map_register(AuxGeneration::Valleyview, channel, Some(index))
}

/// Resolve the G4x control-register selector.
// upstream: intel_dp_aux.c g4x_aux_ctl_reg()
pub fn g4x_aux_ctl_reg(channel: AuxChannel) -> AuxRegister {
    map_register(AuxGeneration::G4x, channel, None)
}

/// Resolve one G4x data-register selector.
// upstream: intel_dp_aux.c g4x_aux_data_reg()
pub fn g4x_aux_data_reg(channel: AuxChannel, index: u8) -> AuxRegister {
    map_register(AuxGeneration::G4x, channel, Some(index))
}

/// Resolve the Ironlake/PCH-split control-register selector.
// upstream: intel_dp_aux.c ilk_aux_ctl_reg()
pub fn ilk_aux_ctl_reg(channel: AuxChannel) -> AuxRegister {
    map_register(AuxGeneration::PchSplit, channel, None)
}

/// Resolve one Ironlake/PCH-split data-register selector.
// upstream: intel_dp_aux.c ilk_aux_data_reg()
pub fn ilk_aux_data_reg(channel: AuxChannel, index: u8) -> AuxRegister {
    map_register(AuxGeneration::PchSplit, channel, Some(index))
}

/// Resolve the Skylake control-register selector.
// upstream: intel_dp_aux.c skl_aux_ctl_reg()
pub fn skl_aux_ctl_reg(channel: AuxChannel) -> AuxRegister {
    map_register(AuxGeneration::Skylake, channel, None)
}

/// Resolve one Skylake data-register selector.
// upstream: intel_dp_aux.c skl_aux_data_reg()
pub fn skl_aux_data_reg(channel: AuxChannel, index: u8) -> AuxRegister {
    map_register(AuxGeneration::Skylake, channel, Some(index))
}

/// Resolve the Tiger Lake control-register selector.
// upstream: intel_dp_aux.c tgl_aux_ctl_reg()
pub fn tgl_aux_ctl_reg(channel: AuxChannel) -> AuxRegister {
    map_register(AuxGeneration::Tigerlake, channel, None)
}

/// Resolve one Tiger Lake data-register selector.
// upstream: intel_dp_aux.c tgl_aux_data_reg()
pub fn tgl_aux_data_reg(channel: AuxChannel, index: u8) -> AuxRegister {
    map_register(AuxGeneration::Tigerlake, channel, Some(index))
}

/// Resolve the Xe-LPDP control-register selector.
// upstream: intel_dp_aux.c xelpdp_aux_ctl_reg()
pub fn xelpdp_aux_ctl_reg(channel: AuxChannel) -> AuxRegister {
    map_register(AuxGeneration::Xelpdp, channel, None)
}

/// Resolve one Xe-LPDP data-register selector.
// upstream: intel_dp_aux.c xelpdp_aux_data_reg()
pub fn xelpdp_aux_data_reg(channel: AuxChannel, index: u8) -> AuxRegister {
    map_register(AuxGeneration::Xelpdp, channel, Some(index))
}

/// Remove QoS and the allocated AUX name during port teardown.
// upstream: intel_dp_aux.c intel_dp_aux_fini()
pub fn intel_dp_aux_fini(io: &mut impl DpAuxIo, state: &mut AuxState) {
    if state.pm_qos_active {
        io.cpu_latency_qos(None);
        state.pm_qos_active = false;
    }
    state.name.clear();
}

/// Select register/divider/send-control implementations and initialize DRM's
/// AUX endpoint; `register_aux` is the narrow DRM-core adapter operation.
// upstream: intel_dp_aux.c intel_dp_aux_init()
pub fn intel_dp_aux_init(io: &mut impl DpAuxIo, state: &mut AuxState) -> Result<(), AuxError> {
    state.name = alloc::format!(
        "AUX {}/{}",
        aux_ch_name(state.platform.display_version, state.channel),
        state.encoder_name
    );
    io.register_aux()?;
    io.cpu_latency_qos(Some(u32::MAX));
    state.pm_qos_active = true;
    io.probe_dpcd(true);
    Ok(())
}

/// Select the platform's default AUX channel, including SKL's DDI-E alias.
// upstream: intel_dp_aux.c default_aux_ch()
pub const fn default_aux_ch(display_version: u8, port: AuxChannel) -> AuxChannel {
    if display_version == 9 && matches!(port, AuxChannel::E) {
        AuxChannel::A
    } else {
        port
    }
}

/// Find another digital encoder that owns a candidate AUX channel.
// upstream: intel_dp_aux.c get_encoder_by_aux_ch()
pub fn get_encoder_by_aux_ch(
    encoders: &[AuxEncoder],
    current: u32,
    channel: AuxChannel,
) -> Option<AuxEncoder> {
    encoders
        .iter()
        .copied()
        .find(|encoder| encoder.id != current && encoder.digital && encoder.aux_channel == channel)
}

/// Choose the VBT AUX channel or platform default and reject duplicate claims.
// upstream: intel_dp_aux.c intel_dp_aux_ch()
pub fn intel_dp_aux_ch(
    display_version: u8,
    vbt_channel: AuxChannel,
    encoder: AuxEncoder,
    encoders: &[AuxEncoder],
) -> Result<(AuxChannel, bool), AuxDiagnostic> {
    let (channel, from_vbt) = if vbt_channel == AuxChannel::None {
        (default_aux_ch(display_version, encoder.port), false)
    } else {
        (vbt_channel, true)
    };
    if channel == AuxChannel::None {
        return Err(AuxDiagnostic::InvalidChannel(channel));
    }
    if let Some(other) = get_encoder_by_aux_ch(encoders, encoder.id, channel) {
        return Err(AuxDiagnostic::DuplicateChannel { owner: other.id });
    }
    Ok((channel, from_vbt))
}

/// Wake AUX waiters after the shared GMBUS/AUX interrupt.
// upstream: intel_dp_aux.c intel_dp_aux_irq_handler()
pub fn intel_dp_aux_irq_handler(io: &mut impl DpAuxIo) {
    io.wake_aux_waiters();
}

/// Select the exact AUX register family and channel fallback used by i915.
pub const fn intel_dp_aux_register(
    platform: AuxPlatform,
    channel: AuxChannel,
    index: Option<u8>,
) -> AuxRegister {
    let generation = if platform.display_version >= 14 {
        AuxGeneration::Xelpdp
    } else if platform.display_version >= 12 {
        AuxGeneration::Tigerlake
    } else if platform.display_version >= 9 {
        AuxGeneration::Skylake
    } else if platform.has_pch_split {
        AuxGeneration::PchSplit
    } else if platform.valleyview || platform.cherryview {
        AuxGeneration::Valleyview
    } else {
        AuxGeneration::G4x
    };
    map_register(generation, channel, index)
}

const fn map_register(
    generation: AuxGeneration,
    channel: AuxChannel,
    index: Option<u8>,
) -> AuxRegister {
    let (valid, fallback) = match generation {
        AuxGeneration::Valleyview | AuxGeneration::G4x => (
            matches!(channel, AuxChannel::B | AuxChannel::C | AuxChannel::D),
            AuxChannel::B,
        ),
        AuxGeneration::PchSplit => (
            matches!(
                channel,
                AuxChannel::A | AuxChannel::B | AuxChannel::C | AuxChannel::D
            ),
            AuxChannel::A,
        ),
        AuxGeneration::Skylake => (
            matches!(
                channel,
                AuxChannel::A
                    | AuxChannel::B
                    | AuxChannel::C
                    | AuxChannel::D
                    | AuxChannel::E
                    | AuxChannel::F
            ),
            AuxChannel::A,
        ),
        AuxGeneration::Tigerlake => (
            matches!(
                channel,
                AuxChannel::A
                    | AuxChannel::B
                    | AuxChannel::C
                    | AuxChannel::UsbC1
                    | AuxChannel::UsbC2
                    | AuxChannel::UsbC3
                    | AuxChannel::UsbC4
                    | AuxChannel::UsbC5
                    | AuxChannel::UsbC6
            ),
            AuxChannel::A,
        ),
        AuxGeneration::Xelpdp => (
            matches!(
                channel,
                AuxChannel::A
                    | AuxChannel::B
                    | AuxChannel::UsbC1
                    | AuxChannel::UsbC2
                    | AuxChannel::UsbC3
                    | AuxChannel::UsbC4
            ),
            AuxChannel::A,
        ),
    };
    let selected = if valid { channel } else { fallback };
    let pch = matches!(generation, AuxGeneration::PchSplit)
        && matches!(selected, AuxChannel::B | AuxChannel::C | AuxChannel::D);
    AuxRegister {
        generation,
        channel: selected,
        data_index: index,
        pch,
    }
}

const fn div_round_closest(dividend: u32, divisor: u32) -> u32 {
    (dividend + divisor / 2) / divisor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aux_dword_packing_is_big_endian_and_bounded() {
        assert_eq!(
            intel_dp_aux_pack(&[0x12, 0x34, 0x56, 0x78, 0x9a]),
            0x1234_5678
        );
        let mut bytes = [0; 3];
        intel_dp_aux_unpack(0xaabb_ccdd, &mut bytes);
        assert_eq!(bytes, [0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn generation_and_port_select_registers_like_i915() {
        let platform = AuxPlatform {
            display_version: 13,
            has_pch_split: false,
            valleyview: false,
            cherryview: false,
            broadwell: false,
            haswell: false,
            pch_lpt_h: false,
            rawclk_khz: 24_000,
            cdclk_khz: 308_000,
        };
        assert_eq!(
            intel_dp_aux_register(platform, AuxChannel::UsbC5, None).channel,
            AuxChannel::UsbC5
        );
        assert_eq!(
            intel_dp_aux_register(platform, AuxChannel::F, Some(2)).channel,
            AuxChannel::A
        );
        assert_eq!(aux_ch_name(12, AuxChannel::UsbC4), "USBC4");
        assert_eq!(aux_ch_name(13, AuxChannel::XelpdpD), "D");
    }

    #[test]
    fn divider_and_send_control_preserve_platform_workarounds() {
        let platform = AuxPlatform {
            display_version: 8,
            has_pch_split: true,
            valleyview: false,
            cherryview: false,
            broadwell: false,
            haswell: true,
            pch_lpt_h: true,
            rawclk_khz: 24_000,
            cdclk_khz: 308_000,
        };
        assert_eq!(hsw_get_aux_clock_divider(platform, AuxChannel::B, 0), 63);
        assert_eq!(hsw_get_aux_clock_divider(platform, AuxChannel::B, 1), 72);
        assert_eq!(g4x_get_aux_clock_divider(platform, 0), 12);
        let state = AuxState {
            channel: AuxChannel::A,
            platform: AuxPlatform {
                display_version: 14,
                ..platform
            },
            name: String::new(),
            last_busy_status: 0,
            pm_qos_active: false,
            edp: false,
            tbt_alt_mode: true,
            firmware_sync_length_quirk: false,
            aux_domain: 0,
            encoder_name: String::new(),
        };
        assert_ne!(skl_get_aux_send_ctl(&state, 4, false) & TBT_IO, 0);
        assert_ne!(skl_get_aux_send_ctl(&state, 4, false) & POWER_REQUEST, 0);
    }

    #[test]
    fn message_header_and_aksv_flag_match_source() {
        let mut header = [0; HEADER_SIZE];
        intel_dp_aux_header(&mut header, AUX_NATIVE_WRITE, 0x12345, 2);
        assert_eq!(header, [0x81, 0x23, 0x45, 1]);
        assert_eq!(
            intel_dp_aux_xfer_flags(AUX_NATIVE_WRITE, AUX_HDCP_AKSV),
            AKSV_SELECT
        );
        assert_eq!(intel_dp_aux_xfer_flags(AUX_NATIVE_READ, AUX_HDCP_AKSV), 0);
    }
}
