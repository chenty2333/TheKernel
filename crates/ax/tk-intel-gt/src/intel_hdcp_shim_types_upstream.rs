// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_hdcp_shim.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use crate::i915_external_placeholder_types_upstream::{HdcpWiredProtocol};
use crate::intel_display_limits_types_upstream::{Transcoder};
use crate::intel_display_types_upstream::{IntelConnector, IntelDigitalPort};

#[repr(C)]
pub struct IntelHdcpShim {
    pub write_an_aksv: Option<unsafe extern "C" fn(*mut IntelDigitalPort, *mut u8) -> i32>,
    pub read_bksv: Option<unsafe extern "C" fn(*mut IntelDigitalPort, *mut u8) -> i32>,
    pub read_bstatus: Option<unsafe extern "C" fn(*mut IntelDigitalPort, *mut u8) -> i32>,
    pub repeater_present: Option<unsafe extern "C" fn(*mut IntelDigitalPort, *mut bool) -> i32>,
    pub read_ri_prime: Option<unsafe extern "C" fn(*mut IntelDigitalPort, *mut u8) -> i32>,
    pub read_ksv_ready: Option<unsafe extern "C" fn(*mut IntelDigitalPort, *mut bool) -> i32>,
    pub read_ksv_fifo: Option<unsafe extern "C" fn(*mut IntelDigitalPort, i32, *mut u8) -> i32>,
    pub read_v_prime_part: Option<unsafe extern "C" fn(*mut IntelDigitalPort, i32, *mut u32) -> i32>,
    pub toggle_signalling: Option<unsafe extern "C" fn(*mut IntelDigitalPort, Transcoder, bool) -> i32>,
    pub stream_encryption: Option<unsafe extern "C" fn(*mut IntelConnector, bool) -> i32>,
    pub check_link: Option<unsafe extern "C" fn(*mut IntelDigitalPort, *mut IntelConnector) -> bool>,
    pub hdcp_get_capability: Option<unsafe extern "C" fn(*mut IntelDigitalPort, *mut bool) -> i32>,
    pub protocol: HdcpWiredProtocol,
    pub _pad_100: [u8; 4],
    pub hdcp_2_2_get_capability: Option<unsafe extern "C" fn(*mut IntelConnector, *mut bool) -> i32>,
    pub write_2_2_msg: Option<unsafe extern "C" fn(*mut IntelConnector, *mut c_void, usize) -> i32>,
    pub read_2_2_msg: Option<unsafe extern "C" fn(*mut IntelConnector, u8, *mut c_void, usize) -> i32>,
    pub config_stream_type: Option<unsafe extern "C" fn(*mut IntelConnector, bool, u8) -> i32>,
    pub stream_2_2_encryption: Option<unsafe extern "C" fn(*mut IntelConnector, bool) -> i32>,
    pub check_2_2_link: Option<unsafe extern "C" fn(*mut IntelDigitalPort, *mut IntelConnector) -> i32>,
    pub get_remote_hdcp_capability: Option<unsafe extern "C" fn(*mut IntelConnector, *mut bool, *mut bool) -> i32>,
}

const _: () = assert!(size_of::<IntelHdcpShim>() == 160);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, write_an_aksv) == 0);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, read_bksv) == 8);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, read_bstatus) == 16);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, repeater_present) == 24);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, read_ri_prime) == 32);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, read_ksv_ready) == 40);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, read_ksv_fifo) == 48);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, read_v_prime_part) == 56);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, toggle_signalling) == 64);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, stream_encryption) == 72);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, check_link) == 80);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, hdcp_get_capability) == 88);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, protocol) == 96);
const _: () = assert!(size_of::<HdcpWiredProtocol>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, hdcp_2_2_get_capability) == 104);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, write_2_2_msg) == 112);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, read_2_2_msg) == 120);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, config_stream_type) == 128);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, stream_2_2_encryption) == 136);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, check_2_2_link) == 144);
const _: () = assert!(core::mem::offset_of!(IntelHdcpShim, get_remote_hdcp_capability) == 152);
