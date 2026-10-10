// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc.c and
// intel_guc_fwif.h: GuC control words and soft-scratch parameter block.
// Copyright © 2014-2019 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use crate::{Error, GtIo, uc::Platform};

const GUC_CTL_LOG_PARAMS: usize = 0;
const GUC_CTL_WA: usize = 1;
const GUC_CTL_FEATURE: usize = 2;
const GUC_CTL_DEBUG: usize = 3;
const GUC_CTL_ADS: usize = 4;
const GUC_CTL_DEVID: usize = 5;
pub const GUC_CTL_MAX_DWORDS: usize = 14;

const GUC_LOG_VALID: u32 = 1 << 0;
const GUC_LOG_NOTIFY_ON_HALF_FULL: u32 = 1 << 1;
const GUC_LOG_CAPTURE_ALLOC_UNITS: u32 = 1 << 2;
const GUC_LOG_LOG_ALLOC_UNITS: u32 = 1 << 3;
const GUC_LOG_CRASH_SHIFT: u32 = 4;
const GUC_LOG_DEBUG_SHIFT: u32 = 6;
const GUC_LOG_CAPTURE_SHIFT: u32 = 10;
const GUC_LOG_BUF_ADDR_SHIFT: u32 = 12;
const GUC_LOG_VERBOSITY_SHIFT: u32 = 0;
const GUC_LOG_DISABLED: u32 = 1 << 6;

const GUC_WA_PRE_PARSER: u32 = 1 << 14;
const GUC_WA_CONTEXT_ISOLATION: u32 = 1 << 15;
const GUC_WA_RCS_CCS_SWITCHOUT: u32 = 1 << 16;
const GUC_WA_HOLD_CCS_SWITCHOUT: u32 = 1 << 17;
const GUC_WA_POLLCS: u32 = 1 << 18;
const GUC_WA_DUAL_QUEUE: u32 = 1 << 11;
const GUC_WA_ENABLE_TSC_CHECK_ON_RC6: u32 = 1 << 22;
const GUC_CTL_ENABLE_GUC_PXP_CTL: u32 = 1 << 1;
const GUC_CTL_ENABLE_SLPC: u32 = 1 << 2;
const GUC_CTL_DISABLE_SCHEDULER: u32 = 1 << 14;
const GUC_ADS_ADDR_SHIFT: u32 = 1;
const GUC_ADS_ADDR_MASK: u32 = 0x000f_ffff << GUC_ADS_ADDR_SHIFT;
const SOFT_SCRATCH_BASE: u32 = 0xc180;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogSection {
    /// Per-section flags produced by intel_guc_log.c::guc_log_init_sizes().
    pub flags: u32,
    pub count: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogConfig {
    pub ggtt_address: u32,
    pub crash: LogSection,
    pub debug: LogSection,
    pub capture: LogSection,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GraphicsIp {
    pub major: u8,
    pub minor: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GucWaInfo {
    pub graphics_ip: GraphicsIp,
    pub media_ip: GraphicsIp,
    pub pre_parser_wa: bool,
    pub is_wa_14014475959: bool,
    pub is_dg2: bool,
    pub is_dg2_g11: bool,
    pub ccs_present: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GucOptions {
    pub log_level: u8,
    pub log: LogConfig,
    pub submission: bool,
    pub slpc: bool,
    pub pxp_enabled: bool,
    pub ads_ggtt_address: u32,
    pub workarounds: GucWaInfo,
    pub firmware_version: (u8, u8, u8),
    pub device_id: u16,
    pub revision: u8,
}

pub fn guc_ctl_debug_flags(level: u8) -> u32 {
    if level <= 1 {
        GUC_LOG_DISABLED
    } else {
        u32::from(level - 2) << GUC_LOG_VERBOSITY_SHIFT
    }
}

pub fn guc_ctl_feature_flags(submission: bool, slpc: bool, pxp: bool) -> u32 {
    let mut flags = 0;
    if pxp {
        flags |= GUC_CTL_ENABLE_GUC_PXP_CTL;
    }
    if !submission {
        flags |= GUC_CTL_DISABLE_SCHEDULER;
    }
    if slpc {
        flags |= GUC_CTL_ENABLE_SLPC;
    }
    flags
}

pub fn guc_ctl_log_params_flags(log: LogConfig) -> Result<u32, Error> {
    if log.ggtt_address == 0 || log.ggtt_address & 0xfff != 0 {
        return Err(Error::Refused);
    }
    let offset = log.ggtt_address >> 12;
    if offset > 0x000f_ffff
        || log.crash.count > 3
        || log.debug.count > 15
        || log.capture.count > 3
        || log.debug.flags & !GUC_LOG_LOG_ALLOC_UNITS != 0
        || log.capture.flags & !GUC_LOG_CAPTURE_ALLOC_UNITS != 0
    {
        return Err(Error::Refused);
    }
    Ok(GUC_LOG_VALID
        | GUC_LOG_NOTIFY_ON_HALF_FULL
        | log.debug.flags
        | log.capture.flags
        | (u32::from(log.crash.count) << GUC_LOG_CRASH_SHIFT)
        | (u32::from(log.debug.count) << GUC_LOG_DEBUG_SHIFT)
        | (u32::from(log.capture.count) << GUC_LOG_CAPTURE_SHIFT)
        | (offset << GUC_LOG_BUF_ADDR_SHIFT))
}

pub fn guc_ctl_ads_flags(ads_ggtt_address: u32) -> Result<u32, Error> {
    if ads_ggtt_address == 0 || ads_ggtt_address & 0xfff != 0 {
        return Err(Error::Refused);
    }
    let pages = ads_ggtt_address >> 12;
    if pages > GUC_ADS_ADDR_MASK >> GUC_ADS_ADDR_SHIFT {
        return Err(Error::Refused);
    }
    Ok(pages << GUC_ADS_ADDR_SHIFT)
}

pub fn guc_ctl_wa_flags(wa: GucWaInfo, firmware_version: (u8, u8, u8)) -> u32 {
    let mut flags = 0;
    if wa.graphics_ip.major >= 11
        && wa.graphics_ip
            < (GraphicsIp {
                major: 12,
                minor: 55,
            })
    {
        flags |= GUC_WA_POLLCS;
    }
    if wa.is_wa_14014475959 || wa.is_dg2 {
        flags |= GUC_WA_HOLD_CCS_SWITCHOUT;
    }
    if (GraphicsIp {
        major: 12,
        minor: 70,
    } <= wa.graphics_ip)
        && wa.graphics_ip
            < (GraphicsIp {
                major: 12,
                minor: 75,
            })
    {
        flags |= GUC_WA_RCS_CCS_SWITCHOUT;
    }
    if wa.is_dg2
        || (wa.ccs_present
            && GraphicsIp {
                major: 12,
                minor: 70,
            } <= wa.graphics_ip)
    {
        flags |= GUC_WA_DUAL_QUEUE;
    }
    if wa.pre_parser_wa {
        flags |= GUC_WA_PRE_PARSER;
    }
    if wa.is_dg2_g11 {
        flags |= GUC_WA_CONTEXT_ISOLATION;
    }
    if firmware_version >= (70, 7, 0) {
        flags |= GUC_WA_ENABLE_TSC_CHECK_ON_RC6;
    }
    flags
}

pub const fn guc_ctl_devid(device_id: u16, revision: u8) -> u32 {
    ((device_id as u32) << 16) | revision as u32
}

pub fn guc_init_params(options: GucOptions) -> Result<[u32; GUC_CTL_MAX_DWORDS], Error> {
    if options.log_level > 5 || options.ads_ggtt_address == 0 {
        return Err(Error::Refused);
    }
    let mut params = [0; GUC_CTL_MAX_DWORDS];
    params[GUC_CTL_LOG_PARAMS] = guc_ctl_log_params_flags(options.log)?;
    params[GUC_CTL_WA] = guc_ctl_wa_flags(options.workarounds, options.firmware_version);
    params[GUC_CTL_FEATURE] =
        guc_ctl_feature_flags(options.submission, options.slpc, options.pxp_enabled);
    params[GUC_CTL_DEBUG] = guc_ctl_debug_flags(options.log_level);
    params[GUC_CTL_ADS] = guc_ctl_ads_flags(options.ads_ggtt_address)?;
    params[GUC_CTL_DEVID] = guc_ctl_devid(options.device_id, options.revision);
    Ok(params)
}

pub fn write_params(io: &impl GtIo, params: &[u32; GUC_CTL_MAX_DWORDS]) -> Result<(), Error> {
    write_params_with_base(io, SOFT_SCRATCH_BASE, params)
}

pub fn write_params_with_regs(
    io: &impl GtIo,
    regs: crate::guc_fw::GucSendRegs,
    params: &[u32; GUC_CTL_MAX_DWORDS],
) -> Result<(), Error> {
    write_params_with_base(io, regs.scratch_base, params)
}

pub fn write_params_with_base(
    io: &impl GtIo,
    scratch_base: u32,
    params: &[u32; GUC_CTL_MAX_DWORDS],
) -> Result<(), Error> {
    io.write(scratch_base, 0)?;
    for (index, value) in params.iter().copied().enumerate() {
        io.write(scratch_base + (index as u32 + 1) * 4, value)?;
    }
    Ok(())
}

pub fn gen12_options(
    platform: Platform,
    device_id: u16,
    revision: u8,
    firmware_version: (u8, u8, u8),
    ads_ggtt_address: u32,
    log: LogConfig,
) -> GucOptions {
    GucOptions {
        log_level: crate::guc_log::default_log_level(false, false, -1),
        log,
        submission: matches!(platform, Platform::AlderLakeP | Platform::AlderLakeN),
        slpc: false,
        pxp_enabled: false,
        ads_ggtt_address,
        workarounds: GucWaInfo {
            graphics_ip: GraphicsIp {
                major: 12,
                minor: 0,
            },
            media_ip: GraphicsIp {
                major: 12,
                minor: 0,
            },
            pre_parser_wa: true,
            is_wa_14014475959: false,
            is_dg2: false,
            is_dg2_g11: false,
            ccs_present: false,
        },
        firmware_version,
        device_id,
        revision,
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;
    use std::vec::Vec;

    use super::*;

    struct Io(RefCell<Vec<(u32, u32)>>);
    impl GtIo for Io {
        fn read(&self, offset: u32) -> Result<u32, Error> {
            Err(Error::Unavailable(offset))
        }
        fn write(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.0.borrow_mut().push((offset, value));
            Ok(())
        }
        fn now_us(&self) -> u64 {
            0
        }
        fn delay_us(&self, _micros: u32) {}
    }

    fn options(platform: Platform) -> GucOptions {
        gen12_options(
            platform,
            0x46d0,
            0,
            (70, 12, 1),
            0x20_0000,
            LogConfig {
                ggtt_address: 0x10_0000,
                crash: LogSection { flags: 0, count: 1 },
                debug: LogSection {
                    flags: 1 << 3,
                    count: 2,
                },
                capture: LogSection {
                    flags: 1 << 2,
                    count: 1,
                },
            },
        )
    }

    #[test]
    fn n305_control_words_preserve_source_flags_and_reserved_slots() {
        let params = guc_init_params(options(Platform::AlderLakeN)).unwrap();
        assert_eq!(params[GUC_CTL_LOG_PARAMS], 0x0010_049f);
        assert_eq!(
            params[GUC_CTL_WA],
            GUC_WA_POLLCS | GUC_WA_PRE_PARSER | GUC_WA_ENABLE_TSC_CHECK_ON_RC6
        );
        assert_eq!(params[GUC_CTL_FEATURE], 0);
        assert_eq!(params[GUC_CTL_DEBUG], GUC_LOG_DISABLED);
        assert_eq!(params[GUC_CTL_ADS], 0x400);
        assert_eq!(params[GUC_CTL_DEVID], 0x46d0_0000);
        assert!(params[6..].iter().all(|word| *word == 0));

        let tgl = guc_init_params(options(Platform::TigerLake)).unwrap();
        assert_eq!(tgl[GUC_CTL_FEATURE], GUC_CTL_DISABLE_SCHEDULER);
    }

    #[test]
    fn parameter_writer_uses_soft_scratch_zero_then_all_control_dwords() {
        let io = Io(RefCell::new(Vec::new()));
        let params = guc_init_params(options(Platform::AlderLakeN)).unwrap();
        write_params(&io, &params).unwrap();
        assert_eq!(io.0.borrow().len(), GUC_CTL_MAX_DWORDS + 1);
        assert_eq!(io.0.borrow()[0], (SOFT_SCRATCH_BASE, 0));
        assert_eq!(io.0.borrow()[1], (SOFT_SCRATCH_BASE + 4, params[0]));
        assert_eq!(
            io.0.borrow()[GUC_CTL_MAX_DWORDS],
            (SOFT_SCRATCH_BASE + 56, params[13])
        );

        let media = Io(RefCell::new(Vec::new()));
        write_params_with_regs(&media, crate::guc_fw::MEDIA_GUC_SEND_REGS, &params).unwrap();
        assert_eq!(media.0.borrow()[0], (0x190310, 0));
        assert_eq!(media.0.borrow()[1], (0x190314, params[0]));
    }

    #[test]
    fn log_and_ads_addresses_must_be_page_aligned_and_encodable() {
        let mut invalid = options(Platform::AlderLakeN);
        invalid.log.ggtt_address |= 1;
        assert_eq!(guc_init_params(invalid), Err(Error::Refused));
        assert_eq!(guc_ctl_ads_flags(0), Err(Error::Refused));
        assert_eq!(guc_ctl_ads_flags(1), Err(Error::Refused));
        assert_eq!(guc_ctl_ads_flags(0xffff_f000), Ok(0x001f_fffe));
    }

    #[test]
    fn workarounds_include_the_four_gen12_gated_control_bits() {
        let gfx_1270 = GraphicsIp {
            major: 12,
            minor: 70,
        };
        let flags = guc_ctl_wa_flags(
            GucWaInfo {
                graphics_ip: gfx_1270,
                media_ip: GraphicsIp {
                    major: 12,
                    minor: 0,
                },
                pre_parser_wa: false,
                is_wa_14014475959: false,
                is_dg2: false,
                is_dg2_g11: false,
                ccs_present: true,
            },
            (70, 6, 0),
        );
        assert_eq!(
            flags & (GUC_WA_RCS_CCS_SWITCHOUT | GUC_WA_DUAL_QUEUE),
            GUC_WA_RCS_CCS_SWITCHOUT | GUC_WA_DUAL_QUEUE
        );
        assert_eq!(flags & GUC_WA_HOLD_CCS_SWITCHOUT, 0);
        let flags = guc_ctl_wa_flags(
            GucWaInfo {
                is_wa_14014475959: true,
                is_dg2_g11: true,
                ..GucWaInfo {
                    graphics_ip: gfx_1270,
                    media_ip: GraphicsIp {
                        major: 12,
                        minor: 0,
                    },
                    pre_parser_wa: false,
                    is_wa_14014475959: false,
                    is_dg2: false,
                    is_dg2_g11: false,
                    ccs_present: false,
                }
            },
            (70, 6, 0),
        );
        assert_ne!(flags & GUC_WA_HOLD_CCS_SWITCHOUT, 0);
        assert_ne!(flags & GUC_WA_CONTEXT_ISOLATION, 0);
    }
}
