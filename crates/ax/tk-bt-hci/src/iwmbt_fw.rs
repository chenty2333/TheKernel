//! Translated from FreeBSD `usr.sbin/bluetooth/iwmbtfw/iwmbt_fw.c` and
//! `iwmbt_fw.h` (BSD-2-Clause), FreeBSD main snapshot 2026-10-08.
//! Copyright (c) 2013 Adrian Chadd <adrian@freebsd.org>
//! Copyright (c) 2019 Vladimir Kondratyev <wulf@FreeBSD.org>
//! Copyright (c) 2023 Future Crew LLC.
//! The userspace file-descriptor loader is intentionally replaced by the
//! kernel rootfs firmware reader (`firmware::request`).

use alloc::{format, string::String, vec::Vec};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Version {
    pub status: u8,
    pub hw_platform: u8,
    pub hw_variant: u8,
    pub hw_revision: u8,
    pub fw_variant: u8,
    pub fw_revision: u8,
    pub fw_build_num: u8,
    pub fw_build_week: u8,
    pub fw_build_year: u8,
    pub fw_patch_num: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootParams {
    pub dev_revid: u16,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VersionTlv {
    pub cnvi_top: u32,
    pub cnvr_top: u32,
    pub cnvi_bt: u32,
    pub cnvr_bt: u32,
    pub dev_rev_id: u16,
    pub img_type: u8,
    pub timestamp: u16,
    pub build_type: u8,
    pub build_num: u32,
    pub secure_boot: u8,
    pub otp_lock: u8,
    pub api_lock: u8,
    pub debug_lock: u8,
    pub min_fw_build_nn: u8,
    pub min_fw_build_cw: u8,
    pub min_fw_build_yy: u8,
    pub limited_cce: u8,
    pub sbe_type: u8,
    pub otp_bd_addr: [u8; 6],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FirmwareError {
    InvalidStatus,
    TruncatedTlv,
    InvalidTlvLength { kind: u8, length: u8 },
    InvalidPatch,
    InvalidVersionEvent,
}

/// Decode the fixed-size Command Complete response copied by
/// `iwmbt_get_version()` from the event's return-parameter area.
pub fn parse_version_event(event: &[u8]) -> Result<Version, FirmwareError> {
    if event.len() != 15
        || event[0] != 0x0e
        || event[1] != 13
        || event[3] != 0x05
        || event[4] != 0xfc
    {
        return Err(FirmwareError::InvalidVersionEvent);
    }
    if event[5] != 0 {
        return Err(FirmwareError::InvalidStatus);
    }
    Ok(Version {
        status: event[5],
        hw_platform: event[6],
        hw_variant: event[7],
        hw_revision: event[8],
        fw_variant: event[9],
        fw_revision: event[10],
        fw_build_num: event[11],
        fw_build_week: event[12],
        fw_build_year: event[13],
        fw_patch_num: event[14],
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchCommand<'a> {
    pub opcode: u16,
    pub parameters: &'a [u8],
    pub expected_events: Vec<(u8, &'a [u8])>,
}

/// Parse the `01 <HCI command>` / `02 <expected event>` stream consumed by
/// `iwmbt_patch_fwfile()`. USB transfers are performed by the HCI transport.
// upstream: iwmbt_hw.c iwmbt_patch_fwfile()
pub fn parse_patch(image: &[u8]) -> Result<(Vec<PatchCommand<'_>>, bool), FirmwareError> {
    let mut commands = Vec::new();
    let mut offset = 0usize;
    let mut activate_patch = false;
    while offset < image.len() {
        if image.len() - offset < 4 || image[offset] != 1 {
            return Err(FirmwareError::InvalidPatch);
        }
        offset += 1;
        let opcode = u16::from_le_bytes([image[offset], image[offset + 1]]);
        let length = usize::from(image[offset + 2]);
        offset += 3;
        let end = offset
            .checked_add(length)
            .ok_or(FirmwareError::InvalidPatch)?;
        if end > image.len() {
            return Err(FirmwareError::InvalidPatch);
        }
        let parameters = &image[offset..end];
        offset = end;
        activate_patch |= opcode == 0xfc8e;
        let mut expected_events = Vec::new();
        while offset < image.len() && image[offset] == 2 {
            if image.len() - offset < 3 {
                return Err(FirmwareError::InvalidPatch);
            }
            let event_code = image[offset + 1];
            let event_len = usize::from(image[offset + 2]);
            offset += 3;
            let end = offset
                .checked_add(event_len)
                .ok_or(FirmwareError::InvalidPatch)?;
            if end > image.len() {
                return Err(FirmwareError::InvalidPatch);
            }
            expected_events
                .try_reserve(1)
                .map_err(|_| FirmwareError::InvalidPatch)?;
            expected_events.push((event_code, &image[offset..end]));
            offset = end;
        }
        commands
            .try_reserve(1)
            .map_err(|_| FirmwareError::InvalidPatch)?;
        commands.push(PatchCommand {
            opcode,
            parameters,
            expected_events,
        });
    }
    Ok((commands, activate_patch))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceFamily {
    Unknown,
    I7260,
    I8260,
    I9260,
}

/// Intel USB VID/PID table from the firmware utility.
// upstream: main.c iwmbt_is_supported()
pub fn supported_device(vendor_id: u16, product_id: u16) -> DeviceFamily {
    match (vendor_id, product_id) {
        (0x8087, 0x07dc | 0x0a2a | 0x0aa7) => DeviceFamily::I7260,
        (0x8087, 0x0a2b | 0x0aaa | 0x0025 | 0x0026 | 0x0029) => DeviceFamily::I8260,
        (0x8087, 0x0032 | 0x0033 | 0x0035 | 0x0036) => DeviceFamily::I9260,
        _ => DeviceFamily::Unknown,
    }
}

/// Return the same 16-bit CNVx top encoding used in FreeBSD iwmbtfw.
fn pack_cnvx_top(value: u32) -> u16 {
    (((value & 0x0f00_0000) >> 16) | ((value & 0xf) << 12) | ((value & 0x0ff0) >> 4)) as u16
}

/// Translate `iwmbt_get_fwname()`; the caller requests this relative path from
/// the kernel firmware provider after rootfs readiness.
// upstream: iwmbt_fw.c iwmbt_get_fwname()
pub fn get_fwname(
    ver: &Version,
    params: Option<&BootParams>,
    prefix: &str,
    suffix: &str,
) -> Option<String> {
    match ver.hw_variant {
        0x07 | 0x08 => Some(format!(
            "{prefix}/ibt-hw-{:x}.{:x}.{:x}-fw-{:x}.{:x}.{:x}.{:x}.{:x}.{suffix}",
            ver.hw_platform,
            ver.hw_variant,
            ver.hw_revision,
            ver.fw_variant,
            ver.fw_revision,
            ver.fw_build_num,
            ver.fw_build_week,
            ver.fw_build_year
        )),
        0x0b | 0x0c => Some(format!(
            "{prefix}/ibt-{}-{}.{suffix}",
            ver.hw_variant, params?.dev_revid
        )),
        0x11..=0x14 => Some(format!(
            "{prefix}/ibt-{}-{}-{}.{suffix}",
            ver.hw_variant, ver.hw_revision, ver.fw_revision
        )),
        _ => None,
    }
}

/// Translate `iwmbt_get_fwname_tlv()`.
// upstream: iwmbt_fw.c iwmbt_get_fwname_tlv()
pub fn get_fwname_tlv(ver: &VersionTlv, prefix: &str, suffix: &str) -> String {
    format!(
        "{prefix}/ibt-{:04x}-{:04x}.{suffix}",
        pack_cnvx_top(ver.cnvi_top),
        pack_cnvx_top(ver.cnvr_top)
    )
}

fn expect_len(kind: u8, length: u8, expected: u8) -> Result<(), FirmwareError> {
    if length == expected {
        Ok(())
    } else {
        Err(FirmwareError::InvalidTlvLength { kind, length })
    }
}

/// Translate `iwmbt_parse_tlv()` with bounds errors instead of C pointer
/// arithmetic; known TLV IDs and field layout are preserved.
// upstream: iwmbt_fw.c iwmbt_parse_tlv()
pub fn parse_tlv(data: &[u8], version: &mut VersionTlv) -> Result<(), FirmwareError> {
    let (&status, mut rest) = data.split_first().ok_or(FirmwareError::TruncatedTlv)?;
    if status != 0 {
        return Err(FirmwareError::InvalidStatus);
    }
    while rest.len() >= 2 {
        let kind = rest[0];
        let length = rest[1];
        rest = &rest[2..];
        if rest.len() < usize::from(length) {
            return Err(FirmwareError::TruncatedTlv);
        }
        let value = &rest[..usize::from(length)];
        let u16le = || u16::from_le_bytes([value[0], value[1]]);
        let u32le = || u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
        match kind {
            0x10 => {
                expect_len(kind, length, 4)?;
                version.cnvi_top = u32le();
            }
            0x11 => {
                expect_len(kind, length, 4)?;
                version.cnvr_top = u32le();
            }
            0x12 => {
                expect_len(kind, length, 4)?;
                version.cnvi_bt = u32le();
            }
            0x13 => {
                expect_len(kind, length, 4)?;
                version.cnvr_bt = u32le();
            }
            0x16 => {
                expect_len(kind, length, 2)?;
                version.dev_rev_id = u16le();
            }
            0x1c => {
                expect_len(kind, length, 1)?;
                version.img_type = value[0];
            }
            0x1d => {
                expect_len(kind, length, 2)?;
                version.min_fw_build_cw = value[0];
                version.min_fw_build_yy = value[1];
                version.timestamp = u16le();
            }
            0x1e => {
                expect_len(kind, length, 1)?;
                version.build_type = value[0];
            }
            0x1f => {
                expect_len(kind, length, 4)?;
                version.min_fw_build_nn = value[0];
                version.build_num = u32le();
            }
            0x28 => {
                expect_len(kind, length, 1)?;
                version.secure_boot = value[0];
            }
            0x2a => {
                expect_len(kind, length, 1)?;
                version.otp_lock = value[0];
            }
            0x2b => {
                expect_len(kind, length, 1)?;
                version.api_lock = value[0];
            }
            0x2c => {
                expect_len(kind, length, 1)?;
                version.debug_lock = value[0];
            }
            0x2d => {
                expect_len(kind, length, 3)?;
                version.min_fw_build_nn = value[0];
                version.min_fw_build_cw = value[1];
                version.min_fw_build_yy = value[2];
            }
            0x2e => {
                expect_len(kind, length, 1)?;
                version.limited_cce = value[0];
            }
            0x2f => {
                expect_len(kind, length, 1)?;
                version.sbe_type = value[0];
            }
            0x30 => {
                expect_len(kind, length, 6)?;
                version.otp_bd_addr.copy_from_slice(value);
            }
            _ => {}
        }
        rest = &rest[usize::from(length)..];
    }
    if !rest.is_empty() {
        return Err(FirmwareError::TruncatedTlv);
    }
    Ok(())
}
