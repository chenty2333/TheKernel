// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_hdcp_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `HDCP_KEY_CONF` (intel_hdcp_regs.h)
pub const HDCP_KEY_CONF: I915RegT = mmio(0x66c00);

/// `HDCP_AKSV_SEND_TRIGGER` (intel_hdcp_regs.h)
pub const HDCP_AKSV_SEND_TRIGGER: u32 = reg_bit(31);

/// `HDCP_CLEAR_KEYS_TRIGGER` (intel_hdcp_regs.h)
pub const HDCP_CLEAR_KEYS_TRIGGER: u32 = reg_bit(30);

/// `HDCP_KEY_LOAD_TRIGGER` (intel_hdcp_regs.h)
pub const HDCP_KEY_LOAD_TRIGGER: u32 = reg_bit(8);

/// `HDCP_KEY_STATUS` (intel_hdcp_regs.h)
pub const HDCP_KEY_STATUS: I915RegT = mmio(0x66c04);

/// `HDCP_FUSE_IN_PROGRESS` (intel_hdcp_regs.h)
pub const HDCP_FUSE_IN_PROGRESS: u32 = reg_bit(7);

/// `HDCP_FUSE_ERROR` (intel_hdcp_regs.h)
pub const HDCP_FUSE_ERROR: u32 = reg_bit(6);

/// `HDCP_FUSE_DONE` (intel_hdcp_regs.h)
pub const HDCP_FUSE_DONE: u32 = reg_bit(5);

/// `HDCP_KEY_LOAD_STATUS` (intel_hdcp_regs.h)
pub const HDCP_KEY_LOAD_STATUS: u32 = reg_bit(1);

/// `HDCP_KEY_LOAD_DONE` (intel_hdcp_regs.h)
pub const HDCP_KEY_LOAD_DONE: u32 = reg_bit(0);

/// `HDCP_AKSV_LO` (intel_hdcp_regs.h)
pub const HDCP_AKSV_LO: I915RegT = mmio(0x66c10);

/// `HDCP_AKSV_HI` (intel_hdcp_regs.h)
pub const HDCP_AKSV_HI: I915RegT = mmio(0x66c14);

/// `HDCP_REP_CTL` (intel_hdcp_regs.h)
pub const HDCP_REP_CTL: I915RegT = mmio(0x66d00);

/// `HDCP_TRANSA_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_TRANSA_REP_PRESENT: u32 = reg_bit(31);

/// `HDCP_TRANSB_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_TRANSB_REP_PRESENT: u32 = reg_bit(30);

/// `HDCP_TRANSC_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_TRANSC_REP_PRESENT: u32 = reg_bit(29);

/// `HDCP_TRANSD_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_TRANSD_REP_PRESENT: u32 = reg_bit(28);

/// `HDCP_DDIB_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_DDIB_REP_PRESENT: u32 = reg_bit(30);

/// `HDCP_DDIA_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_DDIA_REP_PRESENT: u32 = reg_bit(29);

/// `HDCP_DDIC_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_DDIC_REP_PRESENT: u32 = reg_bit(28);

/// `HDCP_DDID_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_DDID_REP_PRESENT: u32 = reg_bit(27);

/// `HDCP_DDIF_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_DDIF_REP_PRESENT: u32 = reg_bit(26);

/// `HDCP_DDIE_REP_PRESENT` (intel_hdcp_regs.h)
pub const HDCP_DDIE_REP_PRESENT: u32 = reg_bit(25);

/// `HDCP_TRANSA_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_TRANSA_SHA1_M0: u32 = ((1 << 20));

/// `HDCP_TRANSB_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_TRANSB_SHA1_M0: u32 = ((2 << 20));

/// `HDCP_TRANSC_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_TRANSC_SHA1_M0: u32 = ((3 << 20));

/// `HDCP_TRANSD_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_TRANSD_SHA1_M0: u32 = ((4 << 20));

/// `HDCP_DDIB_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_DDIB_SHA1_M0: u32 = ((1 << 20));

/// `HDCP_DDIA_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_DDIA_SHA1_M0: u32 = ((2 << 20));

/// `HDCP_DDIC_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_DDIC_SHA1_M0: u32 = ((3 << 20));

/// `HDCP_DDID_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_DDID_SHA1_M0: u32 = ((4 << 20));

/// `HDCP_DDIF_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_DDIF_SHA1_M0: u32 = ((5 << 20));

/// `HDCP_DDIE_SHA1_M0` (intel_hdcp_regs.h)
pub const HDCP_DDIE_SHA1_M0: u32 = ((6 << 20));

/// `HDCP_SHA1_BUSY` (intel_hdcp_regs.h)
pub const HDCP_SHA1_BUSY: u32 = reg_bit(16);

/// `HDCP_SHA1_READY` (intel_hdcp_regs.h)
pub const HDCP_SHA1_READY: u32 = reg_bit(17);

/// `HDCP_SHA1_COMPLETE` (intel_hdcp_regs.h)
pub const HDCP_SHA1_COMPLETE: u32 = reg_bit(18);

/// `HDCP_SHA1_V_MATCH` (intel_hdcp_regs.h)
pub const HDCP_SHA1_V_MATCH: u32 = reg_bit(19);

/// `HDCP_SHA1_TEXT_32` (intel_hdcp_regs.h)
pub const HDCP_SHA1_TEXT_32: u32 = ((1 << 1));

/// `HDCP_SHA1_COMPLETE_HASH` (intel_hdcp_regs.h)
pub const HDCP_SHA1_COMPLETE_HASH: u32 = ((2 << 1));

/// `HDCP_SHA1_TEXT_24` (intel_hdcp_regs.h)
pub const HDCP_SHA1_TEXT_24: u32 = ((4 << 1));

/// `HDCP_SHA1_TEXT_16` (intel_hdcp_regs.h)
pub const HDCP_SHA1_TEXT_16: u32 = ((5 << 1));

/// `HDCP_SHA1_TEXT_8` (intel_hdcp_regs.h)
pub const HDCP_SHA1_TEXT_8: u32 = ((6 << 1));

/// `HDCP_SHA1_TEXT_0` (intel_hdcp_regs.h)
pub const HDCP_SHA1_TEXT_0: u32 = ((7 << 1));

/// `HDCP_SHA_V_PRIME_H0` (intel_hdcp_regs.h)
pub const HDCP_SHA_V_PRIME_H0: I915RegT = mmio(0x66d04);

/// `HDCP_SHA_V_PRIME_H1` (intel_hdcp_regs.h)
pub const HDCP_SHA_V_PRIME_H1: I915RegT = mmio(0x66d08);

/// `HDCP_SHA_V_PRIME_H2` (intel_hdcp_regs.h)
pub const HDCP_SHA_V_PRIME_H2: I915RegT = mmio(0x66d0C);

/// `HDCP_SHA_V_PRIME_H3` (intel_hdcp_regs.h)
pub const HDCP_SHA_V_PRIME_H3: I915RegT = mmio(0x66d10);

/// `HDCP_SHA_V_PRIME_H4` (intel_hdcp_regs.h)
pub const HDCP_SHA_V_PRIME_H4: I915RegT = mmio(0x66d14);

/// `HDCP_SHA_V_PRIME(h)` (intel_hdcp_regs.h)
pub const fn HDCP_SHA_V_PRIME(h: u32) -> I915RegT {
    mmio(((0x66d04 + ((h) * 4))))
}

/// `HDCP_SHA_TEXT` (intel_hdcp_regs.h)
pub const HDCP_SHA_TEXT: I915RegT = mmio(0x66d18);

/// `_PORTA_HDCP_AUTHENC` (intel_hdcp_regs.h)
pub const _PORTA_HDCP_AUTHENC: u32 = 0x66800;

/// `_PORTB_HDCP_AUTHENC` (intel_hdcp_regs.h)
pub const _PORTB_HDCP_AUTHENC: u32 = 0x66500;

/// `_PORTC_HDCP_AUTHENC` (intel_hdcp_regs.h)
pub const _PORTC_HDCP_AUTHENC: u32 = 0x66600;

/// `_PORTD_HDCP_AUTHENC` (intel_hdcp_regs.h)
pub const _PORTD_HDCP_AUTHENC: u32 = 0x66700;

/// `_PORTE_HDCP_AUTHENC` (intel_hdcp_regs.h)
pub const _PORTE_HDCP_AUTHENC: u32 = 0x66A00;

/// `_PORTF_HDCP_AUTHENC` (intel_hdcp_regs.h)
pub const _PORTF_HDCP_AUTHENC: u32 = 0x66900;

/// `_PORT_HDCP_AUTHENC(port, x)` (intel_hdcp_regs.h)
pub const fn _PORT_HDCP_AUTHENC(port: u32, x: u32) -> I915RegT {
    mmio((pick(port, &[_PORTA_HDCP_AUTHENC, _PORTB_HDCP_AUTHENC, _PORTC_HDCP_AUTHENC, _PORTD_HDCP_AUTHENC, _PORTE_HDCP_AUTHENC, _PORTF_HDCP_AUTHENC]) + (x)))
}

/// `PORT_HDCP_CONF(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP_CONF(port: u32) -> I915RegT {
    _PORT_HDCP_AUTHENC(port, 0x0)
}

/// `_TRANSA_HDCP_CONF` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP_CONF: u32 = 0x66400;

/// `_TRANSB_HDCP_CONF` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP_CONF: u32 = 0x66500;

/// `TRANS_HDCP_CONF(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP_CONF(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP_CONF, _TRANSB_HDCP_CONF)
}

/// `HDCP_CONF_CAPTURE_AN` (intel_hdcp_regs.h)
pub const HDCP_CONF_CAPTURE_AN: u32 = reg_bit(0);

/// `HDCP_CONF_AUTH_AND_ENC` (intel_hdcp_regs.h)
pub const HDCP_CONF_AUTH_AND_ENC: u32 = ((reg_bit(1) | reg_bit(0)));

/// `PORT_HDCP_ANINIT(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP_ANINIT(port: u32) -> I915RegT {
    _PORT_HDCP_AUTHENC(port, 0x4)
}

/// `_TRANSA_HDCP_ANINIT` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP_ANINIT: u32 = 0x66404;

/// `_TRANSB_HDCP_ANINIT` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP_ANINIT: u32 = 0x66504;

/// `TRANS_HDCP_ANINIT(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP_ANINIT(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP_ANINIT, _TRANSB_HDCP_ANINIT)
}

/// `PORT_HDCP_ANLO(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP_ANLO(port: u32) -> I915RegT {
    _PORT_HDCP_AUTHENC(port, 0x8)
}

/// `_TRANSA_HDCP_ANLO` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP_ANLO: u32 = 0x66408;

/// `_TRANSB_HDCP_ANLO` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP_ANLO: u32 = 0x66508;

/// `TRANS_HDCP_ANLO(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP_ANLO(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP_ANLO, _TRANSB_HDCP_ANLO)
}

/// `PORT_HDCP_ANHI(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP_ANHI(port: u32) -> I915RegT {
    _PORT_HDCP_AUTHENC(port, 0xC)
}

/// `_TRANSA_HDCP_ANHI` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP_ANHI: u32 = 0x6640C;

/// `_TRANSB_HDCP_ANHI` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP_ANHI: u32 = 0x6650C;

/// `TRANS_HDCP_ANHI(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP_ANHI(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP_ANHI, _TRANSB_HDCP_ANHI)
}

/// `PORT_HDCP_BKSVLO(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP_BKSVLO(port: u32) -> I915RegT {
    _PORT_HDCP_AUTHENC(port, 0x10)
}

/// `_TRANSA_HDCP_BKSVLO` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP_BKSVLO: u32 = 0x66410;

/// `_TRANSB_HDCP_BKSVLO` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP_BKSVLO: u32 = 0x66510;

/// `TRANS_HDCP_BKSVLO(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP_BKSVLO(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP_BKSVLO, _TRANSB_HDCP_BKSVLO)
}

/// `PORT_HDCP_BKSVHI(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP_BKSVHI(port: u32) -> I915RegT {
    _PORT_HDCP_AUTHENC(port, 0x14)
}

/// `_TRANSA_HDCP_BKSVHI` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP_BKSVHI: u32 = 0x66414;

/// `_TRANSB_HDCP_BKSVHI` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP_BKSVHI: u32 = 0x66514;

/// `TRANS_HDCP_BKSVHI(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP_BKSVHI(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP_BKSVHI, _TRANSB_HDCP_BKSVHI)
}

/// `PORT_HDCP_RPRIME(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP_RPRIME(port: u32) -> I915RegT {
    _PORT_HDCP_AUTHENC(port, 0x18)
}

/// `_TRANSA_HDCP_RPRIME` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP_RPRIME: u32 = 0x66418;

/// `_TRANSB_HDCP_RPRIME` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP_RPRIME: u32 = 0x66518;

/// `TRANS_HDCP_RPRIME(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP_RPRIME(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP_RPRIME, _TRANSB_HDCP_RPRIME)
}

/// `PORT_HDCP_STATUS(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP_STATUS(port: u32) -> I915RegT {
    _PORT_HDCP_AUTHENC(port, 0x1C)
}

/// `_TRANSA_HDCP_STATUS` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP_STATUS: u32 = 0x6641C;

/// `_TRANSB_HDCP_STATUS` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP_STATUS: u32 = 0x6651C;

/// `TRANS_HDCP_STATUS(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP_STATUS(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP_STATUS, _TRANSB_HDCP_STATUS)
}

/// `HDCP_STATUS_STREAM_A_ENC` (intel_hdcp_regs.h)
pub const HDCP_STATUS_STREAM_A_ENC: u32 = reg_bit(31);

/// `HDCP_STATUS_STREAM_B_ENC` (intel_hdcp_regs.h)
pub const HDCP_STATUS_STREAM_B_ENC: u32 = reg_bit(30);

/// `HDCP_STATUS_STREAM_C_ENC` (intel_hdcp_regs.h)
pub const HDCP_STATUS_STREAM_C_ENC: u32 = reg_bit(29);

/// `HDCP_STATUS_STREAM_D_ENC` (intel_hdcp_regs.h)
pub const HDCP_STATUS_STREAM_D_ENC: u32 = reg_bit(28);

/// `HDCP_STATUS_AUTH` (intel_hdcp_regs.h)
pub const HDCP_STATUS_AUTH: u32 = reg_bit(21);

/// `HDCP_STATUS_ENC` (intel_hdcp_regs.h)
pub const HDCP_STATUS_ENC: u32 = reg_bit(20);

/// `HDCP_STATUS_RI_MATCH` (intel_hdcp_regs.h)
pub const HDCP_STATUS_RI_MATCH: u32 = reg_bit(19);

/// `HDCP_STATUS_R0_READY` (intel_hdcp_regs.h)
pub const HDCP_STATUS_R0_READY: u32 = reg_bit(18);

/// `HDCP_STATUS_AN_READY` (intel_hdcp_regs.h)
pub const HDCP_STATUS_AN_READY: u32 = reg_bit(17);

/// `HDCP_STATUS_CIPHER` (intel_hdcp_regs.h)
pub const HDCP_STATUS_CIPHER: u32 = reg_bit(16);

/// `HDCP_STATUS_FRAME_CNT(x)` (intel_hdcp_regs.h)
pub const fn HDCP_STATUS_FRAME_CNT(x: u32) -> u32 {
    (((((x) >> 8)) & 0xff))
}

/// `_PORTA_HDCP2_BASE` (intel_hdcp_regs.h)
pub const _PORTA_HDCP2_BASE: u32 = 0x66800;

/// `_PORTB_HDCP2_BASE` (intel_hdcp_regs.h)
pub const _PORTB_HDCP2_BASE: u32 = 0x66500;

/// `_PORTC_HDCP2_BASE` (intel_hdcp_regs.h)
pub const _PORTC_HDCP2_BASE: u32 = 0x66600;

/// `_PORTD_HDCP2_BASE` (intel_hdcp_regs.h)
pub const _PORTD_HDCP2_BASE: u32 = 0x66700;

/// `_PORTE_HDCP2_BASE` (intel_hdcp_regs.h)
pub const _PORTE_HDCP2_BASE: u32 = 0x66A00;

/// `_PORTF_HDCP2_BASE` (intel_hdcp_regs.h)
pub const _PORTF_HDCP2_BASE: u32 = 0x66900;

/// `_PORT_HDCP2_BASE(port, x)` (intel_hdcp_regs.h)
pub const fn _PORT_HDCP2_BASE(port: u32, x: u32) -> I915RegT {
    mmio((pick((port), &[_PORTA_HDCP2_BASE, _PORTB_HDCP2_BASE, _PORTC_HDCP2_BASE, _PORTD_HDCP2_BASE, _PORTE_HDCP2_BASE, _PORTF_HDCP2_BASE]) + (x)))
}

/// `PORT_HDCP2_AUTH(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP2_AUTH(port: u32) -> I915RegT {
    _PORT_HDCP2_BASE(port, 0x98)
}

/// `_TRANSA_HDCP2_AUTH` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP2_AUTH: u32 = 0x66498;

/// `_TRANSB_HDCP2_AUTH` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP2_AUTH: u32 = 0x66598;

/// `TRANS_HDCP2_AUTH(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP2_AUTH(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP2_AUTH, _TRANSB_HDCP2_AUTH)
}

/// `AUTH_LINK_AUTHENTICATED` (intel_hdcp_regs.h)
pub const AUTH_LINK_AUTHENTICATED: u32 = reg_bit(31);

/// `AUTH_LINK_TYPE` (intel_hdcp_regs.h)
pub const AUTH_LINK_TYPE: u32 = reg_bit(30);

/// `AUTH_FORCE_CLR_INPUTCTR` (intel_hdcp_regs.h)
pub const AUTH_FORCE_CLR_INPUTCTR: u32 = reg_bit(19);

/// `AUTH_CLR_KEYS` (intel_hdcp_regs.h)
pub const AUTH_CLR_KEYS: u32 = reg_bit(18);

/// `PORT_HDCP2_CTL(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP2_CTL(port: u32) -> I915RegT {
    _PORT_HDCP2_BASE(port, 0xB0)
}

/// `_TRANSA_HDCP2_CTL` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP2_CTL: u32 = 0x664B0;

/// `_TRANSB_HDCP2_CTL` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP2_CTL: u32 = 0x665B0;

/// `TRANS_HDCP2_CTL(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP2_CTL(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP2_CTL, _TRANSB_HDCP2_CTL)
}

/// `CTL_LINK_ENCRYPTION_REQ` (intel_hdcp_regs.h)
pub const CTL_LINK_ENCRYPTION_REQ: u32 = reg_bit(31);

/// `PORT_HDCP2_STATUS(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP2_STATUS(port: u32) -> I915RegT {
    _PORT_HDCP2_BASE(port, 0xB4)
}

/// `_TRANSA_HDCP2_STATUS` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP2_STATUS: u32 = 0x664B4;

/// `_TRANSB_HDCP2_STATUS` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP2_STATUS: u32 = 0x665B4;

/// `TRANS_HDCP2_STATUS(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP2_STATUS(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP2_STATUS, _TRANSB_HDCP2_STATUS)
}

/// `LINK_TYPE_STATUS` (intel_hdcp_regs.h)
pub const LINK_TYPE_STATUS: u32 = reg_bit(22);

/// `LINK_AUTH_STATUS` (intel_hdcp_regs.h)
pub const LINK_AUTH_STATUS: u32 = reg_bit(21);

/// `LINK_ENCRYPTION_STATUS` (intel_hdcp_regs.h)
pub const LINK_ENCRYPTION_STATUS: u32 = reg_bit(20);

/// `_PIPEA_HDCP2_STREAM_STATUS` (intel_hdcp_regs.h)
pub const _PIPEA_HDCP2_STREAM_STATUS: u32 = 0x668C0;

/// `_PIPEB_HDCP2_STREAM_STATUS` (intel_hdcp_regs.h)
pub const _PIPEB_HDCP2_STREAM_STATUS: u32 = 0x665C0;

/// `_PIPEC_HDCP2_STREAM_STATUS` (intel_hdcp_regs.h)
pub const _PIPEC_HDCP2_STREAM_STATUS: u32 = 0x666C0;

/// `_PIPED_HDCP2_STREAM_STATUS` (intel_hdcp_regs.h)
pub const _PIPED_HDCP2_STREAM_STATUS: u32 = 0x667C0;

/// `PIPE_HDCP2_STREAM_STATUS(pipe)` (intel_hdcp_regs.h)
pub const fn PIPE_HDCP2_STREAM_STATUS(pipe: u32) -> I915RegT {
    mmio(pick((pipe), &[_PIPEA_HDCP2_STREAM_STATUS, _PIPEB_HDCP2_STREAM_STATUS, _PIPEC_HDCP2_STREAM_STATUS, _PIPED_HDCP2_STREAM_STATUS]))
}

/// `_TRANSA_HDCP2_STREAM_STATUS` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP2_STREAM_STATUS: u32 = 0x664C0;

/// `_TRANSB_HDCP2_STREAM_STATUS` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP2_STREAM_STATUS: u32 = 0x665C0;

/// `TRANS_HDCP2_STREAM_STATUS(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP2_STREAM_STATUS(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP2_STREAM_STATUS, _TRANSB_HDCP2_STREAM_STATUS)
}

/// `STREAM_ENCRYPTION_STATUS` (intel_hdcp_regs.h)
pub const STREAM_ENCRYPTION_STATUS: u32 = reg_bit(31);

/// `STREAM_TYPE_STATUS_MASK` (intel_hdcp_regs.h)
pub const STREAM_TYPE_STATUS_MASK: u32 = reg_genmask(30, 30);

/// `_PORTA_HDCP2_AUTH_STREAM` (intel_hdcp_regs.h)
pub const _PORTA_HDCP2_AUTH_STREAM: u32 = 0x66F00;

/// `_PORTB_HDCP2_AUTH_STREAM` (intel_hdcp_regs.h)
pub const _PORTB_HDCP2_AUTH_STREAM: u32 = 0x66F04;

/// `PORT_HDCP2_AUTH_STREAM(port)` (intel_hdcp_regs.h)
pub const fn PORT_HDCP2_AUTH_STREAM(port: u32) -> I915RegT {
    _MMIO_PORT(port, _PORTA_HDCP2_AUTH_STREAM, _PORTB_HDCP2_AUTH_STREAM)
}

/// `_TRANSA_HDCP2_AUTH_STREAM` (intel_hdcp_regs.h)
pub const _TRANSA_HDCP2_AUTH_STREAM: u32 = 0x66F00;

/// `_TRANSB_HDCP2_AUTH_STREAM` (intel_hdcp_regs.h)
pub const _TRANSB_HDCP2_AUTH_STREAM: u32 = 0x66F04;

/// `TRANS_HDCP2_AUTH_STREAM(trans)` (intel_hdcp_regs.h)
pub const fn TRANS_HDCP2_AUTH_STREAM(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANSA_HDCP2_AUTH_STREAM, _TRANSB_HDCP2_AUTH_STREAM)
}

/// `AUTH_STREAM_TYPE_MASK` (intel_hdcp_regs.h)
pub const AUTH_STREAM_TYPE_MASK: u32 = reg_genmask(31, 31);

/// `TRANS_HDCP(__i915)` (intel_hdcp_regs.h)
pub unsafe fn TRANS_HDCP(display: *const IntelDisplay) -> bool {
    unsafe { DISPLAY_VER(display) >= 12 }
}

/// `HDCP_CONF(dev_priv, trans, port)` (intel_hdcp_regs.h)
pub unsafe fn HDCP_CONF(display: *const IntelDisplay, trans: u32, port: u32) -> I915RegT {
    if unsafe { TRANS_HDCP(display) } {
        TRANS_HDCP_CONF(trans)
    } else {
        PORT_HDCP_CONF(port)
    }
}

/// `HDCP_ANINIT(dev_priv, trans, port)` (intel_hdcp_regs.h)
pub unsafe fn HDCP_ANINIT(display: *const IntelDisplay, trans: u32, port: u32) -> I915RegT {
    if unsafe { TRANS_HDCP(display) } {
        TRANS_HDCP_ANINIT(trans)
    } else {
        PORT_HDCP_ANINIT(port)
    }
}

/// `HDCP_ANLO(dev_priv, trans, port)` (intel_hdcp_regs.h)
pub unsafe fn HDCP_ANLO(display: *const IntelDisplay, trans: u32, port: u32) -> I915RegT {
    if unsafe { TRANS_HDCP(display) } {
        TRANS_HDCP_ANLO(trans)
    } else {
        PORT_HDCP_ANLO(port)
    }
}

/// `HDCP_ANHI(dev_priv, trans, port)` (intel_hdcp_regs.h)
pub unsafe fn HDCP_ANHI(display: *const IntelDisplay, trans: u32, port: u32) -> I915RegT {
    if unsafe { TRANS_HDCP(display) } {
        TRANS_HDCP_ANHI(trans)
    } else {
        PORT_HDCP_ANHI(port)
    }
}

/// `HDCP_BKSVLO(dev_priv, trans, port)` (intel_hdcp_regs.h)
pub unsafe fn HDCP_BKSVLO(display: *const IntelDisplay, trans: u32, port: u32) -> I915RegT {
    if unsafe { TRANS_HDCP(display) } {
        TRANS_HDCP_BKSVLO(trans)
    } else {
        PORT_HDCP_BKSVLO(port)
    }
}

/// `HDCP_BKSVHI(dev_priv, trans, port)` (intel_hdcp_regs.h)
pub unsafe fn HDCP_BKSVHI(display: *const IntelDisplay, trans: u32, port: u32) -> I915RegT {
    if unsafe { TRANS_HDCP(display) } {
        TRANS_HDCP_BKSVHI(trans)
    } else {
        PORT_HDCP_BKSVHI(port)
    }
}

/// `HDCP_RPRIME(dev_priv, trans, port)` (intel_hdcp_regs.h)
pub unsafe fn HDCP_RPRIME(display: *const IntelDisplay, trans: u32, port: u32) -> I915RegT {
    if unsafe { TRANS_HDCP(display) } {
        TRANS_HDCP_RPRIME(trans)
    } else {
        PORT_HDCP_RPRIME(port)
    }
}

/// `HDCP_STATUS(dev_priv, trans, port)` (intel_hdcp_regs.h)
pub unsafe fn HDCP_STATUS(display: *const IntelDisplay, trans: u32, port: u32) -> I915RegT {
    if unsafe { TRANS_HDCP(display) } {
        TRANS_HDCP_STATUS(trans)
    } else {
        PORT_HDCP_STATUS(port)
    }
}
