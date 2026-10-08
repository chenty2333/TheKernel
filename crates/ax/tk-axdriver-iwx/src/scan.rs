//! UMAC scan command and scan-state transitions from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{
    CHAN_2GHZ, CMD_ASYNC, ChannelInfo, CommandError, EncodedCommand, HostCommand,
    ProbeRequestConfig, ProbeRequestError, build_scan_probe_request, encode_scan_probe_request,
};

pub const LONG_GROUP: u8 = 1;
pub const UMAC_SCAN_REQ: u8 = 0x0d;
pub const UMAC_SCAN_ABORT: u8 = 0x0e;
pub const UMAC_SCAN_COMPLETE: u8 = 0x0f;
pub const SCAN_CONFIG_COMMAND: u8 = 0x0c;
pub const REDUCED_SCAN_CONFIG_API: u8 = 56;
pub const SCAN_BAND_5GHZ: u8 = 0;
pub const SCAN_BAND_24GHZ: u8 = 1;
pub const SCAN_BAND_FLAG_SHIFT: u32 = 30;
pub const SCAN_PASSIVE_MAX_PSD: u8 = 0x80;
pub const SCAN_PRIORITY_EXT_6: u32 = 6;
pub const SCAN_ENABLE_CHANNEL_ORDER: u8 = 1 << 5;
pub const SCAN_GEN_PASS_ALL: u16 = 1 << 1;
pub const SCAN_GEN_NOTIFY_ITER_COMPLETE: u16 = 1 << 2;
pub const SCAN_GEN_ADAPTIVE_DWELL: u16 = 1 << 7;
pub const SCAN_GEN_FORCE_PASSIVE: u16 = 1 << 11;
pub const SCAN_MAX_CHANNELS: usize = 67;
pub const SCAN_ACTIVE_DWELL: u8 = 10;
pub const SCAN_PASSIVE_DWELL: u8 = 110;
pub const SCAN_FRAGMENTED_LMAC_1: u16 = 1 << 3;
pub const SCAN_FRAGMENTED_LMAC_2: u16 = 1 << 4;
const ADWELL_MAX_BUDGET_FULL: u16 = 300;
const ADWELL_MAX_BUDGET_DIRECTED: u16 = 100;
const ADWELL_DEFAULT_SOCIAL: u8 = 10;
const ADWELL_DEFAULT_2GHZ: u8 = 2;
const ADWELL_DEFAULT_5GHZ: u8 = 8;
const ADWELL_OVERRIDE_GO_FRIENDLY: u8 = 10;
const ADWELL_OVERRIDE_SOCIAL: u8 = 2;
const LMAC_LOW_BAND: usize = 0;
const LMAC_HIGH_BAND: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UmacScanVersion {
    V14,
    V17,
}

#[derive(Debug, Clone, Copy)]
pub struct UmacScanConfig<'a> {
    pub version: UmacScanVersion,
    pub background: bool,
    pub channels: &'a [ChannelInfo],
    pub firmware_channel_limit: usize,
    pub extended_channel_version: bool,
    pub channel_flags: u32,
    pub probe: ProbeRequestConfig<'a>,
    pub desired_ssid: &'a [u8],
    pub slot: u8,
    pub command_queue: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UmacScanError {
    TooManySsids,
    AllocationFailed,
    InvalidParameterBuffer,
    Probe(ProbeRequestError),
    Command(CommandError),
}

/// Construct passive/pass-all/adaptive dwell policy for UMAC scan v2 flags.
// upstream: if_iwx.c iwx_scan_umac_flags_v2()
pub const fn scan_umac_flags_v2(desired_ssid_empty: bool) -> u16 {
    SCAN_GEN_PASS_ALL
        | SCAN_GEN_NOTIFY_ITER_COMPLETE
        | SCAN_GEN_ADAPTIVE_DWELL
        | if desired_ssid_empty {
            SCAN_GEN_FORCE_PASSIVE
        } else {
            0
        }
}

fn scan_umac_dwell_common(general: &mut [u8], background: bool) -> Result<(), UmacScanError> {
    if general.len() < 36 {
        return Err(UmacScanError::InvalidParameterBuffer);
    }
    general[4] = SCAN_ACTIVE_DWELL;
    general[5] = SCAN_ACTIVE_DWELL;
    general[6] = ADWELL_DEFAULT_2GHZ;
    general[7] = ADWELL_DEFAULT_5GHZ;
    general[8] = ADWELL_DEFAULT_SOCIAL;
    put_u16(
        general,
        10,
        if background {
            ADWELL_MAX_BUDGET_DIRECTED
        } else {
            ADWELL_MAX_BUDGET_FULL
        },
    );
    let max_out_of_time = if background { 120u32 } else { 0 };
    for lmac in [LMAC_LOW_BAND, LMAC_HIGH_BAND] {
        put_u32(general, 12 + lmac * 4, max_out_of_time);
        put_u32(general, 20 + lmac * 4, max_out_of_time);
        general[32 + lmac] = SCAN_PASSIVE_DWELL;
    }
    put_u32(general, 28, SCAN_PRIORITY_EXT_6);
    Ok(())
}

/// Fill common dwell defaults for Scan General Params v10.
// upstream: if_iwx.c iwx_scan_umac_dwell_v10()
pub fn scan_umac_dwell_v10(general: &mut [u8], background: bool) -> Result<(), UmacScanError> {
    scan_umac_dwell_common(general, background)
}

/// Fill common dwell defaults for Scan General Params v11.
// upstream: if_iwx.c iwx_scan_umac_dwell_v11()
pub fn scan_umac_dwell_v11(general: &mut [u8], background: bool) -> Result<(), UmacScanError> {
    scan_umac_dwell_common(general, background)
}

/// Fill v10 flags, fragmented scan counts, and default start MAC id.
// upstream: if_iwx.c iwx_scan_umac_fill_general_p_v10()
pub fn scan_umac_fill_general_p_v10(
    general: &mut [u8],
    gen_flags: u16,
    background: bool,
) -> Result<(), UmacScanError> {
    scan_umac_dwell_v10(general, background)?;
    put_u16(general, 0, gen_flags);
    general[3] = 0;
    if gen_flags & SCAN_FRAGMENTED_LMAC_1 != 0 {
        general[34] = 3;
    }
    if gen_flags & SCAN_FRAGMENTED_LMAC_2 != 0 {
        general[35] = 3;
    }
    Ok(())
}

/// Fill v11 flags, fragmented scan counts, and default start MAC/link id.
// upstream: if_iwx.c iwx_scan_umac_fill_general_p_v11()
pub fn scan_umac_fill_general_p_v11(
    general: &mut [u8],
    gen_flags: u16,
    background: bool,
) -> Result<(), UmacScanError> {
    scan_umac_dwell_v11(general, background)?;
    put_u16(general, 0, gen_flags);
    general[3] = 0;
    if gen_flags & SCAN_FRAGMENTED_LMAC_1 != 0 {
        general[34] = 3;
    }
    if gen_flags & SCAN_FRAGMENTED_LMAC_2 != 0 {
        general[35] = 3;
    }
    Ok(())
}

/// Fill v6 channel parameters and copy its API v1-v4 channel array.
// upstream: if_iwx.c iwx_scan_umac_fill_ch_p_v6()
pub fn scan_umac_fill_ch_p_v6(
    output: &mut [u8],
    channels: &[ScanChannelConfig],
) -> Result<(), UmacScanError> {
    if output.len() < 4 + SCAN_MAX_CHANNELS * 8 || channels.len() > SCAN_MAX_CHANNELS {
        return Err(UmacScanError::InvalidParameterBuffer);
    }
    output[0] = SCAN_ENABLE_CHANNEL_ORDER;
    output[1] = channels.len() as u8;
    output[2] = ADWELL_OVERRIDE_GO_FRIENDLY;
    output[3] = ADWELL_OVERRIDE_SOCIAL;
    write_v1_v4_channels(output, 4, channels);
    Ok(())
}

/// Fill v7 channel parameters and copy its v5 channel array.
// upstream: if_iwx.c iwx_scan_umac_fill_ch_p_v7()
pub fn scan_umac_fill_ch_p_v7(
    output: &mut [u8],
    channels: &[ScanChannelConfigV5],
) -> Result<(), UmacScanError> {
    if output.len() < 4 + SCAN_MAX_CHANNELS * 8 || channels.len() > SCAN_MAX_CHANNELS {
        return Err(UmacScanError::InvalidParameterBuffer);
    }
    output[0] = SCAN_ENABLE_CHANNEL_ORDER;
    output[1] = channels.len() as u8;
    output[2] = ADWELL_OVERRIDE_GO_FRIENDLY;
    output[3] = ADWELL_OVERRIDE_SOCIAL;
    write_v5_channels(output, 4, channels);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanConfigError {
    Unsupported,
    Command(CommandError),
}

impl From<CommandError> for ScanConfigError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

/// Build the reduced scan configuration command and select its deprecated
/// broadcast station ID by command-version policy.
// upstream: if_iwx.c iwx_config_umac_scan_reduced()
pub fn reduced_scan_config_command(
    api_supported: bool,
    command_version: u8,
    valid_tx_antennas: u8,
    valid_rx_antennas: u8,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, ScanConfigError> {
    if !api_supported {
        return Err(ScanConfigError::Unsupported);
    }
    let mut payload = [0u8; 12];
    if command_version == 99 || command_version < 5 {
        payload[2] = 0xff;
    }
    payload[4..8].copy_from_slice(&u32::from(valid_tx_antennas).to_le_bytes());
    payload[8..12].copy_from_slice(&u32::from(valid_rx_antennas).to_le_bytes());
    let command = HostCommand {
        id: (u32::from(LONG_GROUP) << 8) | u32::from(SCAN_CONFIG_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, queue)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanChannelConfig {
    pub flags: u32,
    pub channel_num: u8,
    pub band: Option<u8>,
    pub iter_count: u8,
    pub iter_interval: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanChannelConfigV5 {
    pub flags: u32,
    pub channel_num: u8,
    pub psd_20: u8,
    pub iter_count: u8,
    pub iter_interval: u8,
}

/// Fill source-order valid UMAC channels with API v1-v4 channel layouts.
// upstream: if_iwx.c iwx_umac_scan_fill_channels()
pub fn fill_umac_scan_channels(
    channels: &[ChannelInfo],
    item_capacity: usize,
    firmware_channel_limit: usize,
    extended_channel_version: bool,
    channel_flags: u32,
) -> Vec<ScanChannelConfig> {
    let mut result = Vec::new();
    let count = channels
        .iter()
        .filter(|channel| channel.flags != 0)
        .count()
        .min(item_capacity)
        .min(firmware_channel_limit);
    if result.try_reserve_exact(count).is_err() {
        return result;
    }
    for channel in channels
        .iter()
        .filter(|channel| channel.flags != 0)
        .take(count)
    {
        result.push(ScanChannelConfig {
            flags: channel_flags,
            channel_num: channel.channel,
            band: extended_channel_version.then_some(if channel.flags & CHAN_2GHZ != 0 {
                SCAN_BAND_24GHZ
            } else {
                SCAN_BAND_5GHZ
            }),
            iter_count: 1,
            iter_interval: 0,
        });
    }
    result
}

/// Fill v5 channel entries with band bits in flags and a fresh PSD sentinel.
// upstream: if_iwx.c iwx_umac_scan_fill_channels_v5()
pub fn fill_umac_scan_channels_v5(
    channels: &[ChannelInfo],
    item_capacity: usize,
    firmware_channel_limit: usize,
    channel_flags: u32,
) -> Vec<ScanChannelConfigV5> {
    let mut result = Vec::new();
    let count = channels
        .iter()
        .filter(|channel| channel.flags != 0)
        .count()
        .min(item_capacity)
        .min(firmware_channel_limit);
    if result.try_reserve_exact(count).is_err() {
        return result;
    }
    for channel in channels
        .iter()
        .filter(|channel| channel.flags != 0)
        .take(count)
    {
        let band = if channel.flags & CHAN_2GHZ != 0 {
            SCAN_BAND_24GHZ
        } else {
            SCAN_BAND_5GHZ
        };
        result.push(ScanChannelConfigV5 {
            flags: channel_flags | (u32::from(band) << SCAN_BAND_FLAG_SHIFT),
            channel_num: channel.channel,
            psd_20: SCAN_PASSIVE_MAX_PSD,
            iter_count: 1,
            iter_interval: 0,
        });
    }
    result
}

/// Build one fixed-array SCAN_REQ_UMAC wire command.
pub fn build_umac_scan_request(
    config: UmacScanConfig<'_>,
) -> Result<EncodedCommand, UmacScanError> {
    if config.desired_ssid.len() > 32 {
        return Err(UmacScanError::TooManySsids);
    }
    let channels = match config.version {
        UmacScanVersion::V14 => fill_umac_scan_channels(
            config.channels,
            SCAN_MAX_CHANNELS,
            config.firmware_channel_limit,
            config.extended_channel_version,
            config.channel_flags | u32::from(!config.desired_ssid.is_empty()),
        )
        .len(),
        UmacScanVersion::V17 => fill_umac_scan_channels_v5(
            config.channels,
            SCAN_MAX_CHANNELS,
            config.firmware_channel_limit,
            config.channel_flags | u32::from(!config.desired_ssid.is_empty()),
        )
        .len(),
    };
    let valid_count = config
        .channels
        .iter()
        .filter(|channel| channel.flags != 0)
        .count()
        .min(SCAN_MAX_CHANNELS)
        .min(config.firmware_channel_limit);
    if channels != valid_count {
        return Err(UmacScanError::AllocationFailed);
    }
    let expected_size = 8 + 36 + 540 + 12 + 1344;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(expected_size)
        .map_err(|_| UmacScanError::AllocationFailed)?;
    payload.resize(expected_size, 0);
    put_u32(&mut payload, 0, 0); // scan UID
    put_u32(&mut payload, 4, SCAN_PRIORITY_EXT_6);
    let general = 8;
    let force_passive = config.desired_ssid.is_empty();
    let gen_flags = scan_umac_flags_v2(force_passive);
    let general_params = &mut payload[general..general + 36];
    match config.version {
        UmacScanVersion::V14 => {
            scan_umac_fill_general_p_v10(general_params, gen_flags, config.background)?;
        }
        UmacScanVersion::V17 => {
            scan_umac_fill_general_p_v11(general_params, gen_flags, config.background)?;
        }
    }
    let channel_params = general + 36;
    match config.version {
        UmacScanVersion::V14 => {
            let channel_configs = fill_umac_scan_channels(
                config.channels,
                SCAN_MAX_CHANNELS,
                config.firmware_channel_limit,
                config.extended_channel_version,
                config.channel_flags | u32::from(!config.desired_ssid.is_empty()),
            );
            scan_umac_fill_ch_p_v6(
                &mut payload[channel_params..channel_params + 540],
                &channel_configs,
            )?;
        }
        UmacScanVersion::V17 => {
            let channel_configs = fill_umac_scan_channels_v5(
                config.channels,
                SCAN_MAX_CHANNELS,
                config.firmware_channel_limit,
                config.channel_flags | u32::from(!config.desired_ssid.is_empty()),
            );
            scan_umac_fill_ch_p_v7(
                &mut payload[channel_params..channel_params + 540],
                &channel_configs,
            )?;
        }
    }
    let periodic = channel_params + 540;
    payload[periodic + 2] = 1; // first schedule: one iteration.
    let probe_params = periodic + 12;
    let probe = build_scan_probe_request(config.probe).map_err(UmacScanError::Probe)?;
    let encoded_probe = encode_scan_probe_request(&probe);
    payload[probe_params..probe_params + encoded_probe.len()].copy_from_slice(&encoded_probe);
    if !config.desired_ssid.is_empty() {
        let direct_ssid = probe_params + encoded_probe.len() + 4;
        payload[direct_ssid] = 0;
        payload[direct_ssid + 1] = config.desired_ssid.len() as u8;
        payload[direct_ssid + 2..direct_ssid + 2 + config.desired_ssid.len()]
            .copy_from_slice(config.desired_ssid);
    }
    let command = HostCommand {
        id: (u32::from(LONG_GROUP) << 8) | u32::from(UMAC_SCAN_REQ),
        flags: if config.background { CMD_ASYNC } else { 0 },
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, config.slot, config.command_queue)
        .map_err(UmacScanError::Command)
}

/// Build the OpenBSD v14 scan request layout.
// upstream: if_iwx.c iwx_umac_scan_v14()
pub fn umac_scan_v14_command(
    mut config: UmacScanConfig<'_>,
) -> Result<EncodedCommand, UmacScanError> {
    config.version = UmacScanVersion::V14;
    build_umac_scan_request(config)
}

/// Build the OpenBSD v17 scan request layout.
// upstream: if_iwx.c iwx_umac_scan_v17()
pub fn umac_scan_v17_command(
    mut config: UmacScanConfig<'_>,
) -> Result<EncodedCommand, UmacScanError> {
    config.version = UmacScanVersion::V17;
    build_umac_scan_request(config)
}

/// Select firmware scan request version 17, falling back to the v14 layout.
// upstream: if_iwx.c iwx_initiate_scan()
pub fn initiate_scan_command(
    config: UmacScanConfig<'_>,
    command_version: u8,
) -> Result<EncodedCommand, UmacScanError> {
    if command_version == 17 {
        umac_scan_v17_command(config)
    } else {
        umac_scan_v14_command(config)
    }
}

fn write_v1_v4_channels(payload: &mut [u8], offset: usize, channels: &[ScanChannelConfig]) {
    for (index, channel) in channels.iter().enumerate() {
        let at = offset + index * 8;
        put_u32(payload, at, channel.flags);
        if let Some(band) = channel.band {
            payload[at + 4] = channel.channel_num;
            payload[at + 5] = band;
            payload[at + 6] = channel.iter_count;
            payload[at + 7] = channel.iter_interval as u8;
        } else {
            payload[at + 4] = channel.channel_num;
            payload[at + 5] = channel.iter_count;
            payload[at + 6..at + 8].copy_from_slice(&channel.iter_interval.to_le_bytes());
        }
    }
}

fn write_v5_channels(payload: &mut [u8], offset: usize, channels: &[ScanChannelConfigV5]) {
    for (index, channel) in channels.iter().enumerate() {
        let at = offset + index * 8;
        put_u32(payload, at, channel.flags);
        payload[at + 4] = channel.channel_num;
        payload[at + 5] = channel.psd_20;
        payload[at + 6] = channel.iter_count;
        payload[at + 7] = channel.iter_interval;
    }
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// Current driver flags and net80211 state that `iwx_scan()` mutates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanState {
    pub scanning: bool,
    pub background_scan: bool,
    pub in_scan_state: bool,
    pub link_up: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScanError<E> {
    AbortBackground(E),
    Initiate(E),
}

/// Build the 8-byte UMAC scan abort command with uid and reserved flags zero.
// upstream: if_iwx.c iwx_umac_scan_abort()
pub fn scan_abort_command(slot: u8, command_queue: u8) -> Result<EncodedCommand, CommandError> {
    let payload = [0u8; 8];
    let command = HostCommand {
        id: (u32::from(LONG_GROUP) << 8) | u32::from(UMAC_SCAN_ABORT),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, command_queue)
}

/// Begin foreground scan, aborting background scan first and retaining source order.
// upstream: if_iwx.c iwx_scan()
pub fn begin_foreground_scan<E>(
    state: &mut ScanState,
    current_mode_auto: bool,
    mut abort_background: impl FnMut() -> Result<(), E>,
    mut initiate_scan: impl FnMut(bool) -> Result<(), E>,
    mut set_mode_auto: impl FnMut(),
    mut set_link_down: impl FnMut(),
    mut cleanup_bss: impl FnMut(),
    mut enter_scan_state: impl FnMut(),
    mut wake_init: impl FnMut(),
) -> Result<(), ScanError<E>> {
    if state.background_scan {
        abort_background().map_err(ScanError::AbortBackground)?;
        state.background_scan = false;
        state.scanning = false;
    }
    initiate_scan(false).map_err(ScanError::Initiate)?;
    if current_mode_auto {
        set_mode_auto();
    }
    state.scanning = true;
    if !state.background_scan {
        set_link_down();
        state.link_up = false;
        cleanup_bss();
        enter_scan_state();
        state.in_scan_state = true;
    }
    wake_init();
    Ok(())
}

/// Begin background scan unless foreground scan is already active.
// upstream: if_iwx.c iwx_bgscan()
pub fn begin_background_scan<E>(
    state: &mut ScanState,
    mut initiate_scan: impl FnMut(bool) -> Result<(), E>,
) -> Result<bool, E> {
    if state.scanning {
        return Ok(false);
    }
    initiate_scan(true)?;
    state.background_scan = true;
    Ok(true)
}

/// Clear scan flags only after firmware confirms the abort command succeeded.
// upstream: if_iwx.c iwx_scan_abort()
pub fn abort_scan<E>(
    state: &mut ScanState,
    mut send_abort: impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    send_abort()?;
    state.scanning = false;
    state.background_scan = false;
    Ok(())
}

/// End an active foreground/background scan and call net80211 completion once.
// upstream: if_iwx.c iwx_endscan()
pub fn end_scan(state: &mut ScanState, mut complete_net80211: impl FnMut()) -> bool {
    if !state.scanning && !state.background_scan {
        return false;
    }
    state.scanning = false;
    state.background_scan = false;
    state.in_scan_state = false;
    complete_net80211();
    true
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;
    use crate::{CHAN_A, PROBE_REQUEST_WIRE_BYTES};

    #[test]
    fn scan_v10_v11_dwell_and_general_fields_match_source_helpers() {
        assert_eq!(
            scan_umac_flags_v2(false),
            SCAN_GEN_PASS_ALL | SCAN_GEN_NOTIFY_ITER_COMPLETE | SCAN_GEN_ADAPTIVE_DWELL
        );
        assert_eq!(
            scan_umac_flags_v2(true),
            scan_umac_flags_v2(false) | SCAN_GEN_FORCE_PASSIVE
        );
        let flags = scan_umac_flags_v2(true) | SCAN_FRAGMENTED_LMAC_1;
        let mut v10 = [0; 36];
        scan_umac_fill_general_p_v10(&mut v10, flags, true).unwrap();
        assert_eq!(u16::from_le_bytes([v10[0], v10[1]]), flags);
        assert_eq!(v10[4..6], [SCAN_ACTIVE_DWELL; 2]);
        assert_eq!(v10[6..9], [2, 8, 10]);
        assert_eq!(u16::from_le_bytes([v10[10], v10[11]]), 100);
        assert_eq!(u32::from_le_bytes(v10[12..16].try_into().unwrap()), 120);
        assert_eq!(u32::from_le_bytes(v10[20..24].try_into().unwrap()), 120);
        assert_eq!(u32::from_le_bytes(v10[28..32].try_into().unwrap()), 6);
        assert_eq!(v10[32..34], [SCAN_PASSIVE_DWELL; 2]);
        assert_eq!(v10[34], 3);
        assert_eq!(v10[35], 0);

        let mut v11 = [0; 36];
        scan_umac_fill_general_p_v11(&mut v11, flags | SCAN_FRAGMENTED_LMAC_2, false).unwrap();
        assert_eq!(v11[3], 0);
        assert_eq!(u16::from_le_bytes([v11[10], v11[11]]), 300);
        assert_eq!(u32::from_le_bytes(v11[12..16].try_into().unwrap()), 0);
        assert_eq!(v11[34..36], [3, 3]);
    }

    #[test]
    fn scan_v6_v7_channel_helpers_set_order_and_adaptive_overrides() {
        let v6 = [ScanChannelConfig {
            flags: 0x1234_5678,
            channel_num: 6,
            band: Some(SCAN_BAND_24GHZ),
            iter_count: 1,
            iter_interval: 2,
        }];
        let mut bytes = alloc::vec![0; 540];
        scan_umac_fill_ch_p_v6(&mut bytes, &v6).unwrap();
        assert_eq!(bytes[..4], [SCAN_ENABLE_CHANNEL_ORDER, 1, 10, 2]);
        assert_eq!(&bytes[4..8], &0x1234_5678u32.to_le_bytes());
        assert_eq!(bytes[8..12], [6, SCAN_BAND_24GHZ, 1, 2]);

        let v7 = [ScanChannelConfigV5 {
            flags: 0xaabb_ccdd,
            channel_num: 36,
            psd_20: SCAN_PASSIVE_MAX_PSD,
            iter_count: 1,
            iter_interval: 0,
        }];
        scan_umac_fill_ch_p_v7(&mut bytes, &v7).unwrap();
        assert_eq!(bytes[..4], [SCAN_ENABLE_CHANNEL_ORDER, 1, 10, 2]);
        assert_eq!(&bytes[4..8], &0xaabb_ccddu32.to_le_bytes());
        assert_eq!(bytes[8..12], [36, SCAN_PASSIVE_MAX_PSD, 1, 0]);
    }

    #[test]
    fn scan_abort_command_has_source_wide_id_and_zero_payload() {
        let command = scan_abort_command(7, 0).unwrap();
        assert_eq!(
            command.wire_id,
            (u32::from(LONG_GROUP) << 8) | u32::from(UMAC_SCAN_ABORT)
        );
        assert_eq!(&command.bytes[8..], &[0; 8]);
    }

    #[test]
    fn channel_fill_respects_valid_channel_caps_and_generation_layouts() {
        let channels = [
            ChannelInfo {
                channel: 1,
                frequency_mhz: 2412,
                flags: CHAN_2GHZ,
                extended_flags: 0,
            },
            ChannelInfo {
                channel: 2,
                frequency_mhz: 0,
                flags: 0,
                extended_flags: 0,
            },
            ChannelInfo {
                channel: 36,
                frequency_mhz: 5180,
                flags: 0,
                extended_flags: 0,
            },
            ChannelInfo {
                channel: 40,
                frequency_mhz: 5200,
                flags: CHAN_A,
                extended_flags: 0,
            },
        ];
        let old = fill_umac_scan_channels(&channels, 8, 1, false, 0x1234);
        assert_eq!(
            old,
            [ScanChannelConfig {
                flags: 0x1234,
                channel_num: 1,
                band: None,
                iter_count: 1,
                iter_interval: 0
            }]
        );
        let extended = fill_umac_scan_channels(&channels, 8, 8, true, 0x55);
        assert_eq!(extended[0].band, Some(SCAN_BAND_24GHZ));
        assert_eq!(extended[1].band, Some(SCAN_BAND_5GHZ));
        let v5 = fill_umac_scan_channels_v5(&channels, 1, 4, 0x12);
        assert_eq!(
            v5,
            [ScanChannelConfigV5 {
                flags: 0x12 | (1 << SCAN_BAND_FLAG_SHIFT),
                channel_num: 1,
                psd_20: SCAN_PASSIVE_MAX_PSD,
                iter_count: 1,
                iter_interval: 0
            }]
        );
    }

    fn full_scan_config<'a>(
        version: UmacScanVersion,
        channels: &'a [ChannelInfo],
        ssid: &'a [u8],
        background: bool,
    ) -> UmacScanConfig<'a> {
        UmacScanConfig {
            version,
            background,
            channels,
            firmware_channel_limit: 64,
            extended_channel_version: true,
            channel_flags: 0,
            probe: ProbeRequestConfig {
                station_address: [0, 1, 2, 3, 4, 5],
                rates_2ghz: &[2, 4, 11, 22],
                rates_5ghz: &[12, 24, 48],
                supports_5ghz: true,
                include_ds_parameter: true,
                vht_capabilities_ie: None,
                ht_capabilities_ie: None,
            },
            desired_ssid: ssid,
            slot: 2,
            command_queue: 0,
        }
    }

    #[test]
    fn umac_scan_v14_and_v17_build_fixed_request_arrays_and_async_flags() {
        let channels = [
            ChannelInfo {
                channel: 1,
                frequency_mhz: 2412,
                flags: CHAN_2GHZ,
                extended_flags: 0,
            },
            ChannelInfo {
                channel: 36,
                frequency_mhz: 5180,
                flags: CHAN_A,
                extended_flags: 0,
            },
        ];
        let ssid = b"hidden";
        let v14 = build_umac_scan_request(full_scan_config(
            UmacScanVersion::V14,
            &channels,
            ssid,
            false,
        ))
        .unwrap();
        assert_eq!(
            v14.wire_id,
            (u32::from(LONG_GROUP) << 8) | u32::from(UMAC_SCAN_REQ)
        );
        assert_eq!(v14.flags, 0);
        assert_eq!(v14.bytes.len(), 8 + 8 + 36 + 540 + 12 + 1344);
        assert_eq!(
            u16::from_le_bytes(v14.bytes[16..18].try_into().unwrap()),
            SCAN_GEN_PASS_ALL | SCAN_GEN_NOTIFY_ITER_COMPLETE | SCAN_GEN_ADAPTIVE_DWELL
        );
        assert_eq!(v14.bytes[8 + 45], 2);
        assert_eq!(v14.bytes[8 + 48 + 4], 1);
        assert_eq!(v14.bytes[8 + 48 + 5], SCAN_BAND_24GHZ);
        let probe_params = 8 + 8 + 36 + 540 + 12;
        let direct_ssid = probe_params + PROBE_REQUEST_WIRE_BYTES + 4;
        assert_eq!(
            &v14.bytes[direct_ssid..direct_ssid + 8],
            &[0, 6, b'h', b'i', b'd', b'd', b'e', b'n']
        );

        let v17 =
            build_umac_scan_request(full_scan_config(UmacScanVersion::V17, &channels, &[], true))
                .unwrap();
        assert_eq!(
            initiate_scan_command(
                full_scan_config(UmacScanVersion::V14, &channels, &[], true),
                17,
            )
            .unwrap()
            .bytes,
            v17.bytes
        );
        assert_eq!(
            initiate_scan_command(
                full_scan_config(UmacScanVersion::V17, &channels, &[], true),
                18,
            )
            .unwrap()
            .bytes,
            build_umac_scan_request(full_scan_config(UmacScanVersion::V14, &channels, &[], true))
                .unwrap()
                .bytes
        );
        assert_eq!(v17.flags, CMD_ASYNC);
        assert_eq!(
            u16::from_le_bytes(v17.bytes[16..18].try_into().unwrap()),
            SCAN_GEN_PASS_ALL
                | SCAN_GEN_NOTIFY_ITER_COMPLETE
                | SCAN_GEN_ADAPTIVE_DWELL
                | SCAN_GEN_FORCE_PASSIVE
        );
        assert_eq!(v17.bytes[8 + 48 + 5], SCAN_PASSIVE_MAX_PSD);
        assert_ne!(
            u32::from_le_bytes(v17.bytes[8 + 48..8 + 52].try_into().unwrap())
                & (1 << SCAN_BAND_FLAG_SHIFT),
            0
        );
    }

    #[test]
    fn reduced_scan_config_checks_capability_and_legacy_broadcast_station_id() {
        assert_eq!(
            reduced_scan_config_command(false, 6, 3, 1, 0, 0),
            Err(ScanConfigError::Unsupported)
        );
        let legacy = reduced_scan_config_command(true, 4, 3, 1, 2, 0).unwrap();
        assert_eq!(legacy.bytes.len(), 20);
        assert_eq!(legacy.bytes[10], 0xff);
        assert_eq!(&legacy.bytes[12..16], &3u32.to_le_bytes());
        assert_eq!(&legacy.bytes[16..20], &1u32.to_le_bytes());
        let modern = reduced_scan_config_command(true, 5, 3, 1, 2, 0).unwrap();
        assert_eq!(modern.bytes[10], 0);
    }

    #[test]
    fn foreground_scan_aborts_background_before_initiating_and_updates_state() {
        let events = RefCell::new(Vec::new());
        let mut state = ScanState {
            background_scan: true,
            link_up: true,
            ..ScanState::default()
        };
        begin_foreground_scan(
            &mut state,
            true,
            || {
                events.borrow_mut().push(1);
                Ok::<(), ()>(())
            },
            |background| {
                assert!(!background);
                events.borrow_mut().push(2);
                Ok(())
            },
            || events.borrow_mut().push(3),
            || events.borrow_mut().push(4),
            || events.borrow_mut().push(5),
            || events.borrow_mut().push(6),
            || events.borrow_mut().push(7),
        )
        .unwrap();
        assert_eq!(*events.borrow(), [1, 2, 3, 4, 5, 6, 7]);
        assert!(state.scanning && state.in_scan_state);
        assert!(!state.background_scan && !state.link_up);
    }

    #[test]
    fn scan_abort_failure_keeps_flags_and_completion_clears_once() {
        let mut state = ScanState {
            scanning: true,
            background_scan: true,
            in_scan_state: true,
            link_up: false,
        };
        assert_eq!(abort_scan(&mut state, || Err::<(), _>(1)), Err(1));
        assert!(state.scanning && state.background_scan);
        let mut calls = 0;
        assert!(end_scan(&mut state, || calls += 1));
        assert!(!end_scan(&mut state, || calls += 1));
        assert_eq!(calls, 1);
    }
}
