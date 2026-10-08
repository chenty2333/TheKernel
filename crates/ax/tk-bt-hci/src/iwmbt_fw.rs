//! Translated from FreeBSD `usr.sbin/bluetooth/iwmbtfw/iwmbt_fw.c` and
//! `iwmbt_fw.h` (BSD-2-Clause), FreeBSD main snapshot 2026-10-08.
//! Copyright (c) 2013 Adrian Chadd <adrian@freebsd.org>
//! Copyright (c) 2019 Vladimir Kondratyev <wulf@FreeBSD.org>
//! Copyright (c) 2023 Future Crew LLC.
//! The userspace file-descriptor loader is intentionally replaced by the
//! kernel rootfs firmware reader (`firmware::request`).

use alloc::{format, string::String};

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
