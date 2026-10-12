// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_ddi_buf_trans.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct HswDdiBufTrans {
    pub trans1: u32,
    pub trans2: u32,
    pub i_boost: u8,
    pub _pad_9: [u8; 3],
}

#[repr(C)]
pub struct BxtDdiBufTrans {
    pub margin: u8,
    pub scale: u8,
    pub enable: u8,
    pub deemphasis: u8,
}

#[repr(C)]
pub struct IclDdiBufTrans {
    pub dw2_swing_sel: u8,
    pub dw7_n_scalar: u8,
    pub dw4_cursor_coeff: u8,
    pub dw4_post_cursor_2: u8,
    pub dw4_post_cursor_1: u8,
}

#[repr(C)]
pub struct IclMgPhyDdiBufTrans {
    pub cri_txdeemph_override_11_6: u8,
    pub cri_txdeemph_override_5_0: u8,
    pub cri_txdeemph_override_17_12: u8,
}

#[repr(C)]
pub struct TglDklPhyDdiBufTrans {
    pub vswing: u8,
    pub preshoot: u8,
    pub de_emphasis: u8,
}

#[repr(C)]
pub struct Dg2SnpsPhyBufTrans {
    pub vswing: u8,
    pub pre_cursor: u8,
    pub post_cursor: u8,
}

#[repr(C)]
pub struct Xe3plpdLtPhyBufTrans {
    pub txswing: u8,
    pub txswing_level: u8,
    pub pre_cursor: u8,
    pub main_cursor: u8,
    pub post_cursor: u8,
}

#[repr(C)]
pub union IntelDdiBufTransEntry {
    pub hsw: ManuallyDrop<HswDdiBufTrans>,
    pub bxt: ManuallyDrop<BxtDdiBufTrans>,
    pub icl: ManuallyDrop<IclDdiBufTrans>,
    pub mg: ManuallyDrop<IclMgPhyDdiBufTrans>,
    pub dkl: ManuallyDrop<TglDklPhyDdiBufTrans>,
    pub snps: ManuallyDrop<Dg2SnpsPhyBufTrans>,
    pub lt: ManuallyDrop<Xe3plpdLtPhyBufTrans>,
}

#[repr(C)]
pub struct IntelDdiBufTrans {
    pub entries: *mut IntelDdiBufTransEntry,
    pub num_entries: u8,
    pub hdmi_default_entry: u8,
    pub _pad_10: [u8; 6],
}

const _: () = assert!(size_of::<HswDdiBufTrans>() == 12);
const _: () = assert!(core::mem::offset_of!(HswDdiBufTrans, trans1) == 0);
const _: () = assert!(core::mem::offset_of!(HswDdiBufTrans, trans2) == 4);
const _: () = assert!(core::mem::offset_of!(HswDdiBufTrans, i_boost) == 8);
const _: () = assert!(size_of::<BxtDdiBufTrans>() == 4);
const _: () = assert!(core::mem::offset_of!(BxtDdiBufTrans, margin) == 0);
const _: () = assert!(core::mem::offset_of!(BxtDdiBufTrans, scale) == 1);
const _: () = assert!(core::mem::offset_of!(BxtDdiBufTrans, enable) == 2);
const _: () = assert!(core::mem::offset_of!(BxtDdiBufTrans, deemphasis) == 3);
const _: () = assert!(size_of::<IclDdiBufTrans>() == 5);
const _: () = assert!(core::mem::offset_of!(IclDdiBufTrans, dw2_swing_sel) == 0);
const _: () = assert!(core::mem::offset_of!(IclDdiBufTrans, dw7_n_scalar) == 1);
const _: () = assert!(core::mem::offset_of!(IclDdiBufTrans, dw4_cursor_coeff) == 2);
const _: () = assert!(core::mem::offset_of!(IclDdiBufTrans, dw4_post_cursor_2) == 3);
const _: () = assert!(core::mem::offset_of!(IclDdiBufTrans, dw4_post_cursor_1) == 4);
const _: () = assert!(size_of::<IclMgPhyDdiBufTrans>() == 3);
const _: () = assert!(core::mem::offset_of!(IclMgPhyDdiBufTrans, cri_txdeemph_override_11_6) == 0);
const _: () = assert!(core::mem::offset_of!(IclMgPhyDdiBufTrans, cri_txdeemph_override_5_0) == 1);
const _: () = assert!(core::mem::offset_of!(IclMgPhyDdiBufTrans, cri_txdeemph_override_17_12) == 2);
const _: () = assert!(size_of::<TglDklPhyDdiBufTrans>() == 3);
const _: () = assert!(core::mem::offset_of!(TglDklPhyDdiBufTrans, vswing) == 0);
const _: () = assert!(core::mem::offset_of!(TglDklPhyDdiBufTrans, preshoot) == 1);
const _: () = assert!(core::mem::offset_of!(TglDklPhyDdiBufTrans, de_emphasis) == 2);
const _: () = assert!(size_of::<Dg2SnpsPhyBufTrans>() == 3);
const _: () = assert!(core::mem::offset_of!(Dg2SnpsPhyBufTrans, vswing) == 0);
const _: () = assert!(core::mem::offset_of!(Dg2SnpsPhyBufTrans, pre_cursor) == 1);
const _: () = assert!(core::mem::offset_of!(Dg2SnpsPhyBufTrans, post_cursor) == 2);
const _: () = assert!(size_of::<Xe3plpdLtPhyBufTrans>() == 5);
const _: () = assert!(core::mem::offset_of!(Xe3plpdLtPhyBufTrans, txswing) == 0);
const _: () = assert!(core::mem::offset_of!(Xe3plpdLtPhyBufTrans, txswing_level) == 1);
const _: () = assert!(core::mem::offset_of!(Xe3plpdLtPhyBufTrans, pre_cursor) == 2);
const _: () = assert!(core::mem::offset_of!(Xe3plpdLtPhyBufTrans, main_cursor) == 3);
const _: () = assert!(core::mem::offset_of!(Xe3plpdLtPhyBufTrans, post_cursor) == 4);
const _: () = assert!(size_of::<IntelDdiBufTransEntry>() == 12);
const _: () = assert!(size_of::<IntelDdiBufTrans>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDdiBufTrans, entries) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDdiBufTrans, num_entries) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDdiBufTrans, hdmi_default_entry) == 9);
