// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:
//! `include/linux/kmsg_dump.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::types::ListHead;

#[repr(C)]
pub struct KmsgDumper {
    pub list: ListHead,
    pub dump: Option<unsafe extern "C" fn(*mut KmsgDumper, *mut KmsgDumpDetail)>,
    pub max_reason: KmsgDumpReason,
    pub registered: bool,
}

pub type KmsgDumpReason = u32;
pub const KMSG_DUMP_UNDEF: KmsgDumpReason = 0;
pub const KMSG_DUMP_PANIC: KmsgDumpReason = 1;
pub const KMSG_DUMP_OOPS: KmsgDumpReason = 2;
pub const KMSG_DUMP_EMERG: KmsgDumpReason = 3;
pub const KMSG_DUMP_SHUTDOWN: KmsgDumpReason = 4;
pub const KMSG_DUMP_MAX: KmsgDumpReason = 5;

#[repr(C)]
pub struct KmsgDumpDetail {
    _opaque: [u8; 0],
}

const _: () = assert!(size_of::<KmsgDumper>() == 32);
const _: () = assert!(core::mem::offset_of!(KmsgDumper, list) == 0);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(KmsgDumper, dump) == 16);
const _: () = assert!(core::mem::offset_of!(KmsgDumper, max_reason) == 24);
const _: () = assert!(size_of::<KmsgDumpReason>() == 4);
const _: () = assert!(core::mem::offset_of!(KmsgDumper, registered) == 28);
