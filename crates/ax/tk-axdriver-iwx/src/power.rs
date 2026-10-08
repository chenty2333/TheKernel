//! U-APSD access-category and service-period policy from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

pub const WMM_AC_VO: u8 = 0x01;
pub const WMM_AC_VI: u8 = 0x02;
pub const WMM_AC_BK: u8 = 0x04;
pub const WMM_AC_BE: u8 = 0x08;
pub const WMM_AC_MASK: u8 = 0x0f;
pub const WMM_SP_ALL: u8 = 0;
pub const WMM_SP_2: u8 = 1;
pub const WMM_SP_4: u8 = 2;
pub const WMM_SP_6: u8 = 3;
pub const WMM_SP_MASK: u8 = 0x03;

/// Pick the highest-priority enabled non-ACM U-APSD trigger TID.
// upstream: if_iwx.c iwx_uapsd_qndp_tid()
pub fn uapsd_qndp_tid(requested_acs: u8, acm: [bool; 4]) -> u8 {
    if requested_acs & WMM_AC_VO != 0 && !acm[3] {
        6
    } else if requested_acs & WMM_AC_VI != 0 && !acm[2] {
        5
    } else if requested_acs & WMM_AC_BE != 0 && !acm[0] {
        0
    } else if requested_acs & WMM_AC_BK != 0 && !acm[1] {
        1
    } else {
        0
    }
}

/// Convert WMM QoS-info AC bits to firmware's duplicated trigger/delivery mask.
// upstream: if_iwx.c iwx_uapsd_acs()
pub const fn uapsd_ac_mask(requested_acs: u8) -> u8 {
    let mut acs = 0u8;
    if requested_acs & WMM_AC_BK != 0 {
        acs |= 1 << 0;
    }
    if requested_acs & WMM_AC_BE != 0 {
        acs |= 1 << 1;
    }
    if requested_acs & WMM_AC_VI != 0 {
        acs |= 1 << 2;
    }
    if requested_acs & WMM_AC_VO != 0 {
        acs |= 1 << 3;
    }
    acs | (acs << 4)
}

/// Convert WMM QoS-info AC bits to the MAC power-command U-APSD mask.
// upstream: if_iwx.c iwx_uapsd_ac_flags()
pub const fn uapsd_ac_flags(requested_acs: u8) -> u8 {
    let mut flags = 0u8;
    if requested_acs & WMM_AC_BE != 0 {
        flags |= 1 << 0;
    }
    if requested_acs & WMM_AC_BK != 0 {
        flags |= 1 << 1;
    }
    if requested_acs & WMM_AC_VI != 0 {
        flags |= 1 << 2;
    }
    if requested_acs & WMM_AC_VO != 0 {
        flags |= 1 << 3;
    }
    flags
}

/// Convert the WMM maximum service-period field to firmware's frame limit.
// upstream: if_iwx.c iwx_uapsd_sp_length()
pub const fn uapsd_service_period(max_service_period: u8) -> u8 {
    match max_service_period & WMM_SP_MASK {
        WMM_SP_2 => 2,
        WMM_SP_4 => 4,
        WMM_SP_6 => 6,
        _ => 128,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uapsd_priority_masks_and_service_period_follow_source_bits() {
        let all = WMM_AC_VO | WMM_AC_VI | WMM_AC_BE | WMM_AC_BK;
        assert_eq!(uapsd_qndp_tid(all, [false; 4]), 6);
        assert_eq!(uapsd_qndp_tid(all, [false, false, false, true]), 5);
        assert_eq!(uapsd_qndp_tid(all, [false, false, true, true]), 0);
        assert_eq!(uapsd_qndp_tid(WMM_AC_BK, [false; 4]), 1);
        assert_eq!(uapsd_ac_mask(all), 0xff);
        assert_eq!(uapsd_ac_flags(WMM_AC_VO | WMM_AC_BE), 0b1001);
        assert_eq!(uapsd_service_period(WMM_SP_2), 2);
        assert_eq!(uapsd_service_period(WMM_SP_4), 4);
        assert_eq!(uapsd_service_period(WMM_SP_6), 6);
        assert_eq!(uapsd_service_period(WMM_SP_ALL), 128);
    }
}
