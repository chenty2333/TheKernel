// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_bios.c
//
// This first display bring-up slice keeps the upstream VBT acquisition,
// validation, BDB section ownership, and child-device parsing paths used by
// ADL-P (DISPLAY_VER 13). The broader panel/DMI/DSI/ROM parsing routines are
// not on the current display 13 call path; PCI option-ROM fallback remains
// owned by the deferred intel_rom implementation.

#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_device_upstream::intel_display_device_enabled,
    intel_display_limits_types_upstream::{AuxCh, Port, AUX_CH_A, AUX_CH_B, AUX_CH_C, AUX_CH_D_XELPD, AUX_CH_E_XELPD, AUX_CH_NONE, AUX_CH_USBC1, AUX_CH_USBC2, AUX_CH_USBC3, AUX_CH_USBC4, PORT_A, PORT_B, PORT_C, PORT_D, PORT_D_XELPD, PORT_E, PORT_E_XELPD, PORT_F, PORT_G, PORT_H, PORT_I, PORT_NONE, PORT_TC1, PORT_TC2, PORT_TC3, PORT_TC4},
    intel_vbt_defs_types_upstream::{
        BdbGeneralDefinitions, BdbGeneralFeatures, BdbHeader, ChildDeviceConfig, VbtHeader,
    },
};

const GFP_KERNEL: u32 = tk_linux_kapi::config::GFP_KERNEL;
const GMBUS_PIN_VGADDC: i32 = 2;
const BDB_GENERAL_FEATURES: u8 = 1;
const BDB_GENERAL_DEFINITIONS: u8 = 2;
const BDB_DRIVER_FEATURES: u8 = 12;
const BDB_PSR: u8 = 9;
const BDB_SDVO_LVDS_OPTIONS: u8 = 22;
const BDB_SDVO_LVDS_DTD: u8 = 23;
const BDB_EDP: u8 = 27;
const BDB_LFP_OPTIONS: u8 = 40;
const BDB_LFP_DATA_PTRS: u8 = 41;
const BDB_LFP_DATA: u8 = 42;
const BDB_LFP_BACKLIGHT: u8 = 43;
const BDB_LFP_POWER: u8 = 44;
const BDB_MIPI_CONFIG: u8 = 52;
const BDB_MIPI_SEQUENCE: u8 = 53;
const BDB_COMPRESSION_PARAMETERS: u8 = 56;
const BDB_GENERIC_DTD: u8 = 58;

const DEVICE_TYPE_NONE: u16 = 0x0000;
const DEVICE_TYPE_INTERNAL_CONNECTOR: u16 = 1 << 12;
const DEVICE_TYPE_NOT_HDMI_OUTPUT: u16 = 1 << 11;
const DEVICE_TYPE_TMDS_DVI_SIGNALING: u16 = 1 << 4;
const DEVICE_TYPE_DISPLAYPORT_OUTPUT: u16 = 1 << 2;
const DEVICE_TYPE_MIPI_OUTPUT: u16 = 1 << 10;
const DEVICE_TYPE_ANALOG_OUTPUT: u16 = 1 << 0;
const DEVICE_TYPE_LFP: u16 = 0x0022;
const DEVICE_TYPE_INT_LFP: u16 = 0x1022;
const DEVICE_TYPE_TV: u16 = 0x0009;
const DEVICE_TYPE_INT_TV: u16 = 0x1009;
const DEVICE_TYPE_TV_SVIDEO_COMPOSITE: u16 = 0x0609;

const DVO_PORT_HDMIA: u8 = 0;
const DVO_PORT_HDMIB: u8 = 1;
const DVO_PORT_HDMIC: u8 = 2;
const DVO_PORT_HDMID: u8 = 3;
const DVO_PORT_HDMIE: u8 = 12;
const DVO_PORT_HDMIF: u8 = 14;
const DVO_PORT_HDMIG: u8 = 16;
const DVO_PORT_HDMIH: u8 = 18;
const DVO_PORT_HDMII: u8 = 20;
const DVO_PORT_CRT: u8 = 6;
const DVO_PORT_MIPIA: u8 = 21;
const DVO_PORT_MIPIC: u8 = 23;
const DVO_PORT_DPA: u8 = 10;
const DVO_PORT_DPB: u8 = 7;
const DVO_PORT_DPC: u8 = 8;
const DVO_PORT_DPD: u8 = 9;
const DVO_PORT_DPE: u8 = 11;
const DVO_PORT_DPF: u8 = 13;
const DVO_PORT_DPG: u8 = 15;
const DVO_PORT_DPH: u8 = 17;
const DVO_PORT_DPI: u8 = 19;

const CHILD_DEVICE_TYPE_OFFSET: usize = offset_of!(ChildDeviceConfig, device_type);
const CHILD_DVO_PORT_OFFSET: usize = offset_of!(ChildDeviceConfig, dvo_port);
const CHILD_DDC_PIN_OFFSET: usize = offset_of!(ChildDeviceConfig, ddc_pin);
const CHILD_AUX_CHANNEL_OFFSET: usize = offset_of!(ChildDeviceConfig, _anon_23_10) + 2;
const CHILD_DEVICE_FLAGS_OFFSET: usize = offset_of!(ChildDeviceConfig, _anon_23_10);
const CHILD_TYPEC_FLAGS_OFFSET: usize = offset_of!(ChildDeviceConfig, _bitfields_33);
const CHILD_LEVEL_SHIFT_OFFSET: usize = offset_of!(ChildDeviceConfig, _anon_4_2) + 3;
const CHILD_BOOST_OFFSET: usize = offset_of!(ChildDeviceConfig, _bitfields_37);
const CHILD_LINK_RATE_OFFSET: usize = offset_of!(ChildDeviceConfig, _bitfields_37) + 1;
const CHILD_EDP_RATES_OFFSET: usize = offset_of!(ChildDeviceConfig, _bitfields_40);
const BIOS_ENCODER_NODE_OFFSET: usize = offset_of!(IntelBiosEncoderData, node);

/// Linux `struct intel_bios_encoder_data`; the child layout is supplied by the
/// existing field-for-field `intel_vbt_defs` mirror.
#[repr(C)]
pub struct IntelBiosEncoderData {
    pub display: *mut IntelDisplay,
    pub child: ChildDeviceConfig,
    pub dsc: *mut c_void,
    pub node: tk_linux_kapi::types::ListHead,
}

/// The entries in `intel_display::vbt.bdb_blocks` retain the three-byte BDB
/// block header followed by its bytes, as Linux does.
#[repr(C)]
struct BdbBlockEntry {
    node: tk_linux_kapi::types::ListHead,
    section_id: i32,
    data: [u8; 0],
}

const _: () = assert!(size_of::<ChildDeviceConfig>() == 44);
const _: () = assert!(offset_of!(IntelBiosEncoderData, child) == 8);
const _: () = assert!(offset_of!(IntelBiosEncoderData, dsc) == 56);
const _: () = assert!(BIOS_ENCODER_NODE_OFFSET == 64);
const _: () = assert!(size_of::<IntelBiosEncoderData>() == 80);
const _: () = assert!(offset_of!(BdbBlockEntry, section_id) == 16);
const _: () = assert!(offset_of!(BdbBlockEntry, data) == 20);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VbtLayout {
    vbt_size: usize,
    bdb_offset: usize,
    bdb_header_size: usize,
    bdb_size: usize,
}

#[inline]
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

#[inline]
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

/// Validate only the VBT and BDB envelopes. Linux does not require the legacy
/// additive checksum to match, but it does reject truncated headers/sections.
fn validate_vbt_bytes(bytes: &[u8]) -> Option<VbtLayout> {
    const VBT_HEADER_SIZE: usize = size_of::<VbtHeader>();
    const BDB_HEADER_SIZE: usize = size_of::<BdbHeader>();

    if bytes.len() < VBT_HEADER_SIZE || bytes.get(..4)? != b"$VBT" {
        return None;
    }
    let header_size = read_u16(bytes, 22)? as usize;
    let vbt_size = read_u16(bytes, 24)? as usize;
    let bdb_offset = read_u32(bytes, 28)? as usize;
    if header_size < VBT_HEADER_SIZE || vbt_size < header_size || vbt_size > bytes.len() {
        return None;
    }
    let bdb_end = bdb_offset.checked_add(BDB_HEADER_SIZE)?;
    if bdb_offset < header_size || bdb_end > vbt_size {
        return None;
    }
    let bdb_bytes = bytes.get(bdb_offset..vbt_size)?;
    let bdb_header_size = read_u16(bdb_bytes, 18)? as usize;
    let bdb_size = read_u16(bdb_bytes, 20)? as usize;
    if bdb_header_size < BDB_HEADER_SIZE || bdb_size < bdb_header_size {
        return None;
    }
    let bdb_limit = bdb_offset.checked_add(bdb_size)?;
    if bdb_limit > vbt_size {
        return None;
    }
    Some(VbtLayout {
        vbt_size,
        bdb_offset,
        bdb_header_size,
        bdb_size,
    })
}

/// Public to the OpRegion module so it can validate a copied firmware buffer
/// without constructing a Rust slice from an untrusted physical mapping.
pub(crate) unsafe fn intel_bios_is_valid_vbt_bytes(buf: *const u8, size: usize) -> bool {
    if buf.is_null() || size < size_of::<VbtHeader>() {
        return false;
    }
    validate_vbt_bytes(unsafe { core::slice::from_raw_parts(buf, size) }).is_some()
}

/// Linux `intel_bios_is_valid_vbt()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_is_valid_vbt(
    _display: *mut IntelDisplay,
    buf: *const c_void,
    size: usize,
) -> bool {
    unsafe { intel_bios_is_valid_vbt_bytes(buf.cast(), size) }
}

unsafe fn find_raw_block<'a>(bdb: &'a [u8], section_id: u8) -> Option<&'a [u8]> {
    if bdb.len() < size_of::<BdbHeader>() {
        return None;
    }
    let bdb_header_size = read_u16(bdb, 18)? as usize;
    let bdb_size = read_u16(bdb, 20)? as usize;
    let bdb = bdb.get(..bdb_size)?;
    let mut offset = bdb_header_size;
    let mut visited = 0usize;
    while offset.checked_add(3)? <= bdb.len() {
        // BDB sections are byte id + little-endian u16 payload length. The
        // sequence section has a version-3 extended length and retains Linux's
        // special-size rule.
        let id = bdb[offset];
        let standard_size = read_u16(bdb, offset + 1)? as usize;
        let payload_start = offset.checked_add(3)?;
        let payload_size = if id == BDB_MIPI_SEQUENCE
            && bdb.get(payload_start).copied().unwrap_or(0) >= 3
        {
            read_u32(bdb, offset.checked_add(4)?)? as usize
        } else {
            standard_size
        };
        let payload_end = payload_start.checked_add(payload_size)?;
        if payload_end > bdb.len() {
            return None;
        }
        if id == section_id {
            return bdb.get(offset..payload_end);
        }
        offset = payload_end;
        visited += 1;
        // A u16 BDB size cannot contain more than 21,845 zero-length
        // sections; this guard also keeps malformed input work bounded.
        if visited > (u16::MAX as usize / 3) {
            return None;
        }
    }
    None
}

unsafe fn find_raw_section<'a>(bdb: &'a [u8], section_id: u8) -> Option<&'a [u8]> {
    unsafe { find_raw_block(bdb, section_id)?.get(3..) }
}

#[inline]
unsafe fn bdb_block_bytes(entry: *const BdbBlockEntry) -> *const u8 {
    unsafe { entry.cast::<u8>().add(offset_of!(BdbBlockEntry, data)) }
}

/// Linux `bdb_find_section()`, returning the selected block payload.
unsafe fn bdb_find_section(display: *mut IntelDisplay, section_id: u8) -> *const u8 {
    if display.is_null() {
        return ptr::null();
    }
    let head = unsafe { ptr::addr_of!((*display).vbt.bdb_blocks) };
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head.cast_mut()) {
        let entry = node.cast::<BdbBlockEntry>();
        if unsafe { (*entry).section_id == section_id as i32 } {
            return unsafe { bdb_block_bytes(entry).add(3) };
        }
        node = unsafe { (*node).next };
    }
    ptr::null()
}

#[inline]
unsafe fn encoder_from_node(node: *mut tk_linux_kapi::types::ListHead) -> *mut IntelBiosEncoderData {
    unsafe { node.cast::<u8>().sub(BIOS_ENCODER_NODE_OFFSET).cast() }
}

#[inline]
unsafe fn child_bytes(devdata: *const IntelBiosEncoderData) -> *const u8 {
    unsafe { ptr::addr_of!((*devdata).child).cast::<u8>() }
}

#[inline]
unsafe fn child_u8(devdata: *const IntelBiosEncoderData, offset: usize) -> u8 {
    if devdata.is_null() {
        0
    } else {
        unsafe { *child_bytes(devdata).add(offset) }
    }
}

#[inline]
unsafe fn child_u16(devdata: *const IntelBiosEncoderData, offset: usize) -> u16 {
    if devdata.is_null() {
        0
    } else {
        unsafe { ptr::read_unaligned(child_bytes(devdata).add(offset).cast::<u16>()) }
    }
}

#[inline]
unsafe fn child_flags(devdata: *const IntelBiosEncoderData) -> u16 {
    unsafe { child_u16(devdata, CHILD_DEVICE_FLAGS_OFFSET) }
}

#[inline]
unsafe fn child_typec_flags(devdata: *const IntelBiosEncoderData) -> u8 {
    unsafe { child_u8(devdata, CHILD_TYPEC_FLAGS_OFFSET) }
}

#[inline]
unsafe fn child_device_type(devdata: *const IntelBiosEncoderData) -> u16 {
    unsafe { child_u16(devdata, CHILD_DEVICE_TYPE_OFFSET) }
}

fn xelpd_dvo_port_to_port(dvo_port: u8) -> Port {
    match dvo_port {
        DVO_PORT_HDMIA | DVO_PORT_DPA => PORT_A,
        DVO_PORT_HDMIB | DVO_PORT_DPB => PORT_B,
        DVO_PORT_HDMIC | DVO_PORT_DPC => PORT_C,
        DVO_PORT_HDMID | DVO_PORT_DPD => PORT_D_XELPD,
        DVO_PORT_HDMIE | DVO_PORT_DPE => PORT_E_XELPD,
        DVO_PORT_HDMIF | DVO_PORT_DPF => PORT_TC1,
        DVO_PORT_HDMIG | DVO_PORT_DPG => PORT_TC2,
        DVO_PORT_HDMIH | DVO_PORT_DPH => PORT_TC3,
        DVO_PORT_HDMII | DVO_PORT_DPI => PORT_TC4,
        _ => PORT_NONE,
    }
}

unsafe fn dvo_port_type(dvo_port: u8) -> u8 {
    match dvo_port {
        DVO_PORT_HDMIA | DVO_PORT_HDMIB | DVO_PORT_HDMIC | DVO_PORT_HDMID
        | DVO_PORT_HDMIE | DVO_PORT_HDMIF | DVO_PORT_HDMIG | DVO_PORT_HDMIH
        | DVO_PORT_HDMII => DVO_PORT_HDMIA,
        DVO_PORT_DPA | DVO_PORT_DPB | DVO_PORT_DPC | DVO_PORT_DPD | DVO_PORT_DPE
        | DVO_PORT_DPF | DVO_PORT_DPG | DVO_PORT_DPH | DVO_PORT_DPI => DVO_PORT_DPA,
        _ => dvo_port,
    }
}

#[inline]
unsafe fn gmbus_pin_valid_adlp(pin: u8) -> bool {
    matches!(pin, 1 | 2 | 9 | 10 | 11 | 12)
}

fn selected_section_min_size(section_id: u8) -> Option<usize> {
    Some(match section_id {
        BDB_GENERAL_FEATURES => size_of::<BdbGeneralFeatures>(),
        BDB_GENERAL_DEFINITIONS => size_of::<BdbGeneralDefinitions>(),
        BDB_DRIVER_FEATURES => size_of::<crate::intel_vbt_defs_types_upstream::BdbDriverFeatures>(),
        BDB_PSR => size_of::<crate::intel_vbt_defs_types_upstream::BdbPsr>(),
        BDB_SDVO_LVDS_OPTIONS => size_of::<crate::intel_vbt_defs_types_upstream::BdbSdvoLvdsOptions>(),
        BDB_SDVO_LVDS_DTD => size_of::<crate::intel_vbt_defs_types_upstream::BdbSdvoLvdsDtd>(),
        BDB_EDP => size_of::<crate::intel_vbt_defs_types_upstream::BdbEdp>(),
        BDB_LFP_OPTIONS => size_of::<crate::intel_vbt_defs_types_upstream::BdbLfpOptions>(),
        BDB_LFP_DATA_PTRS => size_of::<crate::intel_vbt_defs_types_upstream::BdbLfpDataPtrs>(),
        // BDB_LFP_DATA is size/version dependent; resolved in the caller.
        BDB_LFP_DATA => 0,
        BDB_LFP_BACKLIGHT => size_of::<crate::intel_vbt_defs_types_upstream::BdbLfpBacklight>(),
        BDB_LFP_POWER => size_of::<crate::intel_vbt_defs_types_upstream::BdbLfpPower>(),
        BDB_MIPI_CONFIG => size_of::<crate::intel_vbt_defs_types_upstream::BdbMipiConfig>(),
        BDB_MIPI_SEQUENCE => size_of::<crate::intel_vbt_defs_types_upstream::BdbMipiSequence>(),
        BDB_COMPRESSION_PARAMETERS => size_of::<crate::intel_vbt_defs_types_upstream::BdbCompressionParameters>(),
        BDB_GENERIC_DTD => size_of::<crate::intel_vbt_defs_types_upstream::BdbGenericDtd>(),
        _ => return None,
    })
}

unsafe fn lfp_data_min_size(bdb: &[u8]) -> usize {
    let base = size_of::<crate::intel_vbt_defs_types_upstream::BdbLfpData>();
    let Some(ptrs) = (unsafe { find_raw_section(bdb, BDB_LFP_DATA_PTRS) }) else {
        return 0;
    };
    let name_offset = offset_of!(crate::intel_vbt_defs_types_upstream::BdbLfpDataPtrs, panel_name);
    let table_size = ptrs.get(name_offset + 2).copied().unwrap_or(0);
    if table_size == 0 {
        return base;
    }
    let offset = read_u16(ptrs, name_offset).unwrap_or(0) as usize;
    offset
        .checked_add(size_of::<crate::intel_vbt_defs_types_upstream::BdbLfpDataTail>())
        .map(|tail| base.max(tail))
        .unwrap_or(usize::MAX)
}

unsafe fn init_bdb_blocks(display: *mut IntelDisplay, bdb: &[u8]) {
    // Keep the same BDB records that intel_bios.c retains for its panel,
    // compression, and mode parsing helpers.
    for section_id in [
        BDB_GENERAL_FEATURES,
        BDB_GENERAL_DEFINITIONS,
        BDB_PSR,
        BDB_DRIVER_FEATURES,
        BDB_SDVO_LVDS_OPTIONS,
        BDB_SDVO_LVDS_DTD,
        BDB_EDP,
        BDB_LFP_OPTIONS,
        BDB_LFP_DATA_PTRS,
        BDB_LFP_DATA,
        BDB_LFP_BACKLIGHT,
        BDB_LFP_POWER,
        BDB_MIPI_CONFIG,
        BDB_MIPI_SEQUENCE,
        BDB_COMPRESSION_PARAMETERS,
        BDB_GENERIC_DTD,
    ] {
        let Some(mut min_size) = selected_section_min_size(section_id) else {
            continue;
        };
        if section_id == BDB_LFP_DATA {
            min_size = unsafe { lfp_data_min_size(bdb) };
        }
        let Some(block) = (unsafe { find_raw_block(bdb, section_id) }) else {
            continue;
        };
        let Some(data) = block.get(3..) else { continue };
        if data.len() < min_size || data.len() > u16::MAX as usize {
            continue;
        }
        let Some(total) = offset_of!(BdbBlockEntry, data).checked_add(block.len())
        else {
            continue;
        };
        let entry = unsafe { tk_linux_kapi::memory::kmalloc(total, GFP_KERNEL).cast::<BdbBlockEntry>() };
        if entry.is_null() {
            continue;
        }
        let node = unsafe { ptr::addr_of_mut!((*entry).node) };
        unsafe {
            (*entry).section_id = section_id as i32;
            ptr::copy_nonoverlapping(block.as_ptr(), bdb_block_bytes(entry).cast_mut(), block.len());
            tk_linux_kapi::list::list_add_tail(
                node,
                ptr::addr_of_mut!((*display).vbt.bdb_blocks),
            );
        }
    }
}

#[inline]
fn has_bit(value: u8, bit: u32) -> bool {
    value & (1 << bit) != 0
}

unsafe fn intel_bios_ssc_frequency(display: *mut IntelDisplay, alternate: bool) -> c_int {
    match unsafe { display_ver(display) } {
        2 => if alternate { 66_667 } else { 48_000 },
        3 | 4 => if alternate { 100_000 } else { 96_000 },
        _ => if alternate { 100_000 } else { 120_000 },
    }
}

unsafe fn parse_general_features(display: *mut IntelDisplay) {
    let data = unsafe { bdb_find_section(display, BDB_GENERAL_FEATURES) };
    if data.is_null() {
        return;
    }
    // BDB_GENERAL_FEATURES is a byte-oriented bitfield record.
    let second = unsafe { *data.add(1) };
    let third = unsafe { *data.add(2) };
    let device_support = unsafe { *data.add(4) };
    let afc_config = unsafe { *data.add(6) } & 0x03;
    let version = unsafe { (*display).vbt.version };
    let vbt = unsafe { ptr::addr_of_mut!((*display).vbt) };
    let mut flags = unsafe { (*vbt)._bitfields_2[0] };
    flags = (flags & !0x01) | u8::from(has_bit(device_support, 1)); // int_tv_support
    if version >= 155 && unsafe { (*display).info.__runtime_info.ip.ver >= 5 } {
        flags = (flags & !0x02) | (u8::from(has_bit(device_support, 0)) << 1); // int_crt_support
    }
    flags = (flags & !(1 << 2)) | (u8::from(has_bit(second, 1)) << 2); // enable_ssc
    flags = (flags & !(1 << 4)) | (u8::from(has_bit(second, 6)) << 4); // display_clock_mode
    flags = (flags & !(1 << 5)) | (u8::from(has_bit(third, 3)) << 5); // fdi_rx_polarity
    unsafe { (*vbt)._bitfields_2[0] = flags };
    unsafe {
        (*vbt).lvds_ssc_freq = intel_bios_ssc_frequency(display, has_bit(second, 2));
        if version >= 181 {
            (*vbt).orientation = if has_bit(third, 2) { 1 } else { 0 };
        } else {
            (*vbt).orientation = -1;
        }
        if version >= 249 {
            if afc_config != 0 {
                (*vbt).override_afc_startup = true;
                (*vbt).override_afc_startup_val = if afc_config == 1 { 0 } else { 7 };
            }
        }
    }
}

unsafe fn parse_driver_features(display: *mut IntelDisplay) {
    let driver = unsafe { bdb_find_section(display, BDB_DRIVER_FEATURES) };
    if driver.is_null() {
        return;
    }
    let version = unsafe { (*display).vbt.version };
    let display_ver = unsafe { display_ver(display) };
    let lvds_config = (unsafe { *driver.add(8) } >> 3) & 0x03;
    let disable_lvds = if display_ver >= 5 {
        lvds_config != 1 // BDB_DRIVER_FEATURE_INT_LVDS
    } else {
        version >= 134 && lvds_config != 1 && lvds_config != 3
    };
    if disable_lvds {
        unsafe { (*display).vbt._bitfields_2[0] &= !(1 << 3) };
    }
}

unsafe fn parse_general_definitions(display: *mut IntelDisplay) {
    let data = unsafe { bdb_find_section(display, BDB_GENERAL_DEFINITIONS) };
    if data.is_null() {
        return;
    }
    let bdb_header = unsafe { data.sub(3) };
    let block_size = unsafe { ptr::read_unaligned(bdb_header.add(1).cast::<u16>()) } as usize;
    if block_size < size_of::<BdbGeneralDefinitions>() {
        return;
    }
    let child_dev_size = unsafe { *data.add(offset_of!(BdbGeneralDefinitions, child_dev_size)) } as usize;
    let device_type_offset = offset_of!(BdbGeneralDefinitions, devices);
    if child_dev_size < 33 {
        return;
    }
    let Some(device_bytes) = block_size.checked_sub(device_type_offset) else {
        return;
    };
    let child_count = device_bytes / child_dev_size;

    for index in 0..child_count {
        let Some(child_offset) = index.checked_mul(child_dev_size).and_then(|x| x.checked_add(device_type_offset)) else {
            break;
        };
        let device_type = unsafe { ptr::read_unaligned(data.add(child_offset + CHILD_DEVICE_TYPE_OFFSET).cast::<u16>()) };
        if device_type == DEVICE_TYPE_NONE {
            continue;
        }
        let entry = unsafe {
            tk_linux_kapi::memory::kmalloc(size_of::<IntelBiosEncoderData>(), GFP_KERNEL)
                .cast::<IntelBiosEncoderData>()
        };
        if entry.is_null() {
            break;
        }
        unsafe {
            (*entry).display = display;
            (*entry).dsc = ptr::null_mut();
            ptr::write_bytes(
                ptr::addr_of_mut!((*entry).child).cast::<u8>(),
                0,
                size_of::<ChildDeviceConfig>(),
            );
            ptr::copy_nonoverlapping(
                data.add(child_offset),
                ptr::addr_of_mut!((*entry).child).cast::<u8>(),
                child_dev_size.min(size_of::<ChildDeviceConfig>()),
            );
            tk_linux_kapi::list::list_add_tail(
                ptr::addr_of_mut!((*entry).node),
                ptr::addr_of_mut!((*display).vbt.display_devices),
            );
        }
    }

    // Linux accepts the VBT CRT DDC pin only when the platform's GMBUS
    // implementation recognizes it. DISPLAY_VER 13 uses the ICP/ADLP pin
    // table (BXT pins 1/2 and Type-C pins 9..12).
    let pin = unsafe { *data } as i32;
    if gmbus_pin_valid_adlp(pin as u8) {
        unsafe { (*display).vbt.crt_ddc_pin = pin };
    }
}

unsafe fn parse_compression_parameters(display: *mut IntelDisplay) {
    if unsafe { (*display).vbt.version < 198 } {
        return;
    }
    let params = unsafe { bdb_find_section(display, BDB_COMPRESSION_PARAMETERS) };
    if params.is_null() {
        return;
    }
    let entry_size = unsafe { ptr::read_unaligned(params.cast::<u16>()) } as usize;
    if entry_size != size_of::<crate::intel_vbt_defs_types_upstream::DscCompressionParametersEntry>() {
        return;
    }
    let block_size = unsafe { ptr::read_unaligned(params.sub(2).cast::<u16>()) } as usize;
    if block_size < size_of::<crate::intel_vbt_defs_types_upstream::BdbCompressionParameters>() {
        return;
    }
    let head = unsafe { ptr::addr_of!((*display).vbt.display_devices) };
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head.cast_mut()) {
        let devdata = unsafe { encoder_from_node(node) };
        let child_bits = unsafe { child_u8(devdata, offset_of!(ChildDeviceConfig, _anon_4_2) + 6) };
        if child_bits & (1 << 1) != 0 && child_bits & (1 << 2) == 0 {
            let index = (unsafe {
                child_u8(devdata, offset_of!(ChildDeviceConfig, _anon_4_2) + 7)
            } & 0x0f) as usize;
            let dsc_size = size_of::<crate::intel_vbt_defs_types_upstream::DscCompressionParametersEntry>();
            let offset = 2usize.saturating_add(index.saturating_mul(entry_size));
            if index < 16 && offset.checked_add(dsc_size).is_some_and(|end| end <= block_size) {
                let dsc = unsafe { tk_linux_kapi::memory::kmalloc(dsc_size, GFP_KERNEL) };
                if !dsc.is_null() {
                    unsafe {
                        ptr::copy_nonoverlapping(params.add(offset), dsc.cast::<u8>(), dsc_size);
                        (*devdata).dsc = dsc;
                    }
                }
            }
        }
        node = unsafe { (*node).next };
    }
}

unsafe fn parse_ddi_ports(display: *mut IntelDisplay) {
    let head = unsafe { ptr::addr_of!((*display).vbt.display_devices) };
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head.cast_mut()) {
        let devdata = unsafe { encoder_from_node(node) };
        let port = unsafe { intel_bios_encoder_port(devdata) };
        if port != PORT_NONE && unsafe { (*display).vbt.version >= 264 } {
            // Dedicated-external PHYs are outside the Type-C subsystem. Linux
            // sanitizes the three Type-C-only claims before any DDI consumer
            // observes this child record.
            let bits = unsafe { child_typec_flags(devdata) };
            if bits & (1 << 2) != 0 {
                let byte = unsafe {
                    ptr::addr_of_mut!((*devdata).child)
                        .cast::<u8>()
                        .add(CHILD_TYPEC_FLAGS_OFFSET)
                };
                unsafe { *byte &= !((1 << 0) | (1 << 1) | (1 << 3)) };
            }
        }
        node = unsafe { (*node).next };
    }
}

unsafe fn init_vbt_defaults(display: *mut IntelDisplay) {
    let vbt = unsafe { ptr::addr_of_mut!((*display).vbt) };
    unsafe {
        (*vbt).crt_ddc_pin = GMBUS_PIN_VGADDC;
        (*vbt)._bitfields_2[0] |= (1 << 0) | (1 << 1) | (1 << 3) | (1 << 2);
        (*vbt).lvds_ssc_freq = intel_bios_ssc_frequency(display, (*display).info.__runtime_info.ip.ver < 5);
    }
}

unsafe fn add_missing_child(display: *mut IntelDisplay, port: Port) -> bool {
    let child = unsafe {
        tk_linux_kapi::memory::kmalloc(size_of::<IntelBiosEncoderData>(), GFP_KERNEL)
            .cast::<IntelBiosEncoderData>()
    };
    if child.is_null() {
        return false;
    }
    let dvo_port = match port {
        PORT_A => DVO_PORT_HDMIA,
        PORT_B => DVO_PORT_HDMIB,
        PORT_C => DVO_PORT_HDMIC,
        PORT_D_XELPD => DVO_PORT_HDMID,
        PORT_E_XELPD => DVO_PORT_HDMIE,
        PORT_TC1 => DVO_PORT_HDMIF,
        PORT_TC2 => DVO_PORT_HDMIG,
        PORT_TC3 => DVO_PORT_HDMIH,
        PORT_TC4 => DVO_PORT_HDMII,
        PORT_F => DVO_PORT_HDMIF,
        _ => {
            unsafe { tk_linux_kapi::memory::kfree(child.cast::<c_void>()) };
            return false;
        }
    };
    unsafe {
        (*child).display = display;
        (*child).dsc = ptr::null_mut();
        ptr::write_bytes(ptr::addr_of_mut!((*child).child).cast::<u8>(), 0, size_of::<ChildDeviceConfig>());
        let child_bytes = ptr::addr_of_mut!((*child).child).cast::<u8>();
        *child_bytes.add(CHILD_DVO_PORT_OFFSET) = dvo_port;
        let mut device_type = DEVICE_TYPE_DISPLAYPORT_OUTPUT;
        if port != PORT_E_XELPD {
            device_type |= DEVICE_TYPE_TMDS_DVI_SIGNALING;
        }
        if port == PORT_A {
            device_type |= DEVICE_TYPE_INTERNAL_CONNECTOR;
        }
        ptr::write_unaligned(
            child_bytes.add(CHILD_DEVICE_TYPE_OFFSET).cast::<u16>(),
            device_type,
        );
        tk_linux_kapi::list::list_add_tail(
            ptr::addr_of_mut!((*child).node),
            ptr::addr_of_mut!((*display).vbt.display_devices),
        );
    }
    true
}

unsafe fn init_vbt_missing_defaults(display: *mut IntelDisplay) {
    if unsafe { (*display).info.__runtime_info.ip.ver < 5 } {
        return;
    }
    let port_mask = unsafe { (*display).info.__runtime_info.port_mask };
    for port in [
        PORT_A,
        PORT_B,
        PORT_C,
        PORT_TC1,
        PORT_TC2,
        PORT_TC3,
        PORT_TC4,
        PORT_D_XELPD,
        PORT_E_XELPD,
    ] {
        if port_mask & (1 << port) == 0 {
            continue;
        }
        // Linux excludes Type-C PHYs here; the XELPD mapping is A/B/C/D/E
        // followed by TC1..TC4, so preserve only the non-TC ports.
        if (PORT_TC1..=PORT_TC4).contains(&port) {
            continue;
        }
        if !unsafe { add_missing_child(display, port) } {
            break;
        }
    }
}

unsafe fn find_child_by_port(display: *mut IntelDisplay, port: Port) -> *mut IntelBiosEncoderData {
    if display.is_null() {
        return ptr::null_mut();
    }
    let head = unsafe { ptr::addr_of!((*display).vbt.display_devices) };
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head.cast_mut()) {
        let entry = unsafe { encoder_from_node(node) };
        if unsafe { intel_bios_encoder_port(entry) == port } {
            return entry;
        }
        node = unsafe { (*node).next };
    }
    ptr::null_mut()
}

unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    if display.is_null() {
        0
    } else {
        unsafe { (*display).info.__runtime_info.ip.ver }
    }
}

unsafe fn intel_bios_get_vbt(display: *mut IntelDisplay, size_out: *mut usize) -> *mut u8 {
    if !size_out.is_null() {
        unsafe { *size_out = 0 };
    }
    // Upstream tries a user-selected firmware file before the OpRegion.
    let name = unsafe { (*display).params.vbt_firmware };
    if !name.is_null() && unsafe { *name != 0 } {
        let mut fw = ptr::null();
        let ret = unsafe {
            tk_linux_kapi::firmware::tk_linux_firmware_request_nowarn(
                &mut fw,
                name,
                (*(*display).drm).dev,
            )
        };
        if ret == 0 && !fw.is_null() {
            let record = unsafe { &*fw };
            if record.size <= u16::MAX as usize
                && unsafe { intel_bios_is_valid_vbt_bytes(record.data, record.size) }
            {
                let copy = unsafe { tk_linux_kapi::memory::kmalloc(record.size, GFP_KERNEL).cast::<u8>() };
                if !copy.is_null() {
                    unsafe { ptr::copy_nonoverlapping(record.data, copy, record.size) };
                    if !size_out.is_null() {
                        unsafe { *size_out = record.size };
                    }
                    unsafe { tk_linux_kapi::firmware::tk_linux_firmware_release(fw) };
                    return copy;
                }
            }
            unsafe { tk_linux_kapi::firmware::tk_linux_firmware_release(fw) };
        }
    }

    let mut size = 0usize;
    let vbt = unsafe { crate::intel_opregion_upstream::intel_opregion_get_vbt(display, &mut size) }.cast_mut().cast::<u8>();
    if vbt.is_null()
        || size > u16::MAX as usize
        || !unsafe { intel_bios_is_valid_vbt_bytes(vbt, size) }
    {
        if !vbt.is_null() {
            unsafe { tk_linux_kapi::memory::kfree(vbt.cast::<c_void>()) };
        }
        return ptr::null_mut();
    }
    if !size_out.is_null() {
        unsafe { *size_out = size };
    }
    vbt
}

/// Linux `intel_bios_init()` for the display 13 VBT path. The OpRegion setup
/// must have run earlier through `intel_opregion_setup()`; this function does
/// not manufacture a successful setup or silently replace a missing VBT with
/// a static TC-port map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_init(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    unsafe {
        tk_linux_kapi::list::INIT_LIST_HEAD(ptr::addr_of_mut!((*display).vbt.display_devices));
        tk_linux_kapi::list::INIT_LIST_HEAD(ptr::addr_of_mut!((*display).vbt.bdb_blocks));
    }
    if !unsafe { intel_display_device_enabled(display) } {
        return;
    }
    unsafe { init_vbt_defaults(display) };

    let mut size = 0usize;
    let vbt = unsafe { intel_bios_get_vbt(display, &mut size) };
    if vbt.is_null() {
        // This remains the Linux fallback for non-TC ports. It intentionally
        // cannot report the XELPD Type-C ports present, which is why the real
        // OpRegion mailbox path above is CORE and must be wired before probe.
        unsafe { init_vbt_missing_defaults(display) };
        return;
    }

    let bytes = unsafe { core::slice::from_raw_parts(vbt, size) };
    if let Some(layout) = validate_vbt_bytes(bytes) {
        let bdb = &bytes[layout.bdb_offset..layout.bdb_offset + layout.bdb_size];
        let version = read_u16(bdb, 16).unwrap_or(0);
        unsafe {
            (*display).vbt.version = version;
            init_bdb_blocks(display, bdb);
            parse_general_features(display);
            parse_general_definitions(display);
            parse_driver_features(display);
            parse_compression_parameters(display);
            parse_ddi_ports(display);
        }
    } else {
        unsafe { init_vbt_missing_defaults(display) };
    }
    unsafe { tk_linux_kapi::memory::kfree(vbt.cast::<c_void>()) };
}

/// Free BDB and encoder entries allocated by `intel_bios_init()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_driver_remove(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let display_head = unsafe { ptr::addr_of_mut!((*display).vbt.display_devices) };
    let mut node = unsafe { (*display_head).next };
    while !ptr::eq(node, display_head) {
        let next = unsafe { (*node).next };
        let devdata = unsafe { encoder_from_node(node) };
        unsafe {
            tk_linux_kapi::list::list_del_init(node);
            if !(*devdata).dsc.is_null() {
                tk_linux_kapi::memory::kfree((*devdata).dsc);
            }
            tk_linux_kapi::memory::kfree(devdata.cast::<c_void>());
        }
        node = next;
    }
    unsafe { tk_linux_kapi::list::INIT_LIST_HEAD(display_head) };

    let bdb_head = unsafe { ptr::addr_of_mut!((*display).vbt.bdb_blocks) };
    let mut node = unsafe { (*bdb_head).next };
    while !ptr::eq(node, bdb_head) {
        let next = unsafe { (*node).next };
        unsafe {
            tk_linux_kapi::list::list_del_init(node);
            tk_linux_kapi::memory::kfree(node.cast::<c_void>());
        }
        node = next;
    }
    unsafe { tk_linux_kapi::list::INIT_LIST_HEAD(bdb_head) };
}

/// Linux `intel_bios_encoder_port()` for the XELPD port mapping selected by
/// ADL-P (display version 13).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_port(devdata: *const IntelBiosEncoderData) -> Port {
    if devdata.is_null() {
        return PORT_NONE;
    }
    let display = unsafe { (*devdata).display };
    let dvo = unsafe { child_u8(devdata, CHILD_DVO_PORT_OFFSET) };
    let version = unsafe { display_ver(display) };
    let mut port = if version >= 13 {
        xelpd_dvo_port_to_port(dvo)
    } else {
        match dvo {
            DVO_PORT_HDMIA | DVO_PORT_DPA => PORT_A,
            DVO_PORT_HDMIB | DVO_PORT_DPB => PORT_B,
            DVO_PORT_HDMIC | DVO_PORT_DPC => PORT_C,
            DVO_PORT_HDMID | DVO_PORT_DPD => PORT_D,
            DVO_PORT_HDMIE | DVO_PORT_DPE | DVO_PORT_CRT => PORT_E,
            DVO_PORT_HDMIF | DVO_PORT_DPF => PORT_F,
            DVO_PORT_HDMIG | DVO_PORT_DPG => PORT_G,
            DVO_PORT_HDMIH | DVO_PORT_DPH => PORT_H,
            DVO_PORT_HDMII | DVO_PORT_DPI => PORT_I,
            _ => PORT_NONE,
        }
    };
    if port == PORT_NONE && version >= 11 {
        port = match dvo {
            DVO_PORT_MIPIA => PORT_A,
            DVO_PORT_MIPIC if version >= 11 => PORT_B,
            DVO_PORT_MIPIC => PORT_C,
            _ => PORT_NONE,
        };
    }
    port
}

/// `intel_bios_is_port_present()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_is_port_present(display: *mut IntelDisplay, port: Port) -> bool {
    if display.is_null() {
        return false;
    }
    if unsafe { display_ver(display) < 5 } {
        return true;
    }
    !unsafe { find_child_by_port(display, port) }.is_null()
}

/// `intel_bios_encoder_data_lookup()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_data_lookup(
    display: *mut IntelDisplay,
    port: Port,
) -> *const IntelBiosEncoderData {
    unsafe { find_child_by_port(display, port) }
}

/// Child-device feature predicates used by the DDI and connector code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_dp(devdata: *const IntelBiosEncoderData) -> bool {
    child_device_type(devdata) & DEVICE_TYPE_DISPLAYPORT_OUTPUT != 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_dvi(devdata: *const IntelBiosEncoderData) -> bool {
    child_device_type(devdata) & DEVICE_TYPE_TMDS_DVI_SIGNALING != 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_hdmi(devdata: *const IntelBiosEncoderData) -> bool {
    (unsafe { intel_bios_encoder_supports_dvi(devdata) })
        && (child_device_type(devdata) & (1 << 11) == 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_edp(devdata: *const IntelBiosEncoderData) -> bool {
    (unsafe { intel_bios_encoder_supports_dp(devdata) })
        && (child_device_type(devdata) & DEVICE_TYPE_INTERNAL_CONNECTOR != 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_crt(devdata: *const IntelBiosEncoderData) -> bool {
    child_device_type(devdata) & DEVICE_TYPE_ANALOG_OUTPUT != 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_dsi(devdata: *const IntelBiosEncoderData) -> bool {
    child_device_type(devdata) & DEVICE_TYPE_MIPI_OUTPUT != 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_is_lspcon(devdata: *const IntelBiosEncoderData) -> bool {
    if devdata.is_null() {
        return false;
    }
    let display = unsafe { (*devdata).display };
    matches!(unsafe { display_ver(display) }, 9 | 10)
        && unsafe { child_flags(devdata) & (1 << 2) != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_typec_usb(devdata: *const IntelBiosEncoderData) -> bool {
    !devdata.is_null()
        && unsafe { (*(*devdata).display).vbt.version >= 195 }
        && unsafe { child_typec_flags(devdata) & 1 != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_tbt(devdata: *const IntelBiosEncoderData) -> bool {
    !devdata.is_null()
        && unsafe { (*(*devdata).display).vbt.version >= 209 }
        && unsafe { child_typec_flags(devdata) & (1 << 1) != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_is_dedicated_external(devdata: *const IntelBiosEncoderData) -> bool {
    !devdata.is_null()
        && unsafe { (*(*devdata).display).vbt.version >= 264 }
        && unsafe { child_typec_flags(devdata) & (1 << 2) != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_dyn_port_over_tc(devdata: *const IntelBiosEncoderData) -> bool {
    !devdata.is_null()
        && unsafe { (*(*devdata).display).vbt.version >= 264 }
        && unsafe { child_typec_flags(devdata) & (1 << 3) != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_lane_reversal(devdata: *const IntelBiosEncoderData) -> bool {
    !devdata.is_null() && unsafe { child_flags(devdata) & (1 << 1) != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_hpd_invert(devdata: *const IntelBiosEncoderData) -> bool {
    !devdata.is_null() && unsafe { child_flags(devdata) & (1 << 4) != 0 }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_supports_dp_dual_mode(devdata: *const IntelBiosEncoderData) -> bool {
    if devdata.is_null()
        || !unsafe { intel_bios_encoder_supports_dp(devdata) }
        || !unsafe { intel_bios_encoder_supports_hdmi(devdata) }
    {
        return false;
    }
    let dvo = unsafe { child_u8(devdata, CHILD_DVO_PORT_OFFSET) };
    let typ = unsafe { dvo_port_type(dvo) };
    typ == DVO_PORT_DPA
        || (typ == DVO_PORT_HDMIA && unsafe { child_u8(devdata, CHILD_AUX_CHANNEL_OFFSET) != 0 })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_hdmi_level_shift(devdata: *const IntelBiosEncoderData) -> c_int {
    if devdata.is_null() {
        return -1;
    }
    let display = unsafe { (*devdata).display };
    if unsafe { (*display).vbt.version < 158 || display_ver(display) >= 14 } {
        return -1;
    }
    unsafe { (child_u8(devdata, CHILD_LEVEL_SHIFT_OFFSET) & 0x1f) as c_int }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_hdmi_max_tmds_clock(devdata: *const IntelBiosEncoderData) -> c_int {
    if devdata.is_null() || unsafe { (*(*devdata).display).vbt.version < 204 } {
        return 0;
    }
    match unsafe { child_u8(devdata, CHILD_LEVEL_SHIFT_OFFSET) >> 5 } {
        1 => 297_000,
        2 => 165_000,
        3 => 594_000,
        4 => 340_000,
        5 => 300_000,
        _ => 0,
    }
}

#[inline]
unsafe fn translate_iboost(value: u8) -> c_int {
    [1, 3, 7].get(value as usize).copied().unwrap_or(0) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_dp_boost_level(devdata: *const IntelBiosEncoderData) -> c_int {
    if devdata.is_null() || unsafe { (*(*devdata).display).vbt.version < 196 } {
        return 0;
    }
    let flags = unsafe { child_flags(devdata) };
    if flags & (1 << 3) == 0 {
        return 0;
    }
    unsafe { translate_iboost(child_u8(devdata, CHILD_BOOST_OFFSET) & 0x0f) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_hdmi_boost_level(devdata: *const IntelBiosEncoderData) -> c_int {
    if devdata.is_null() || unsafe { (*(*devdata).display).vbt.version < 196 } {
        return 0;
    }
    let flags = unsafe { child_flags(devdata) };
    if flags & (1 << 3) == 0 {
        return 0;
    }
    unsafe { translate_iboost(child_u8(devdata, CHILD_BOOST_OFFSET) >> 4) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_hdmi_ddc_pin(devdata: *const IntelBiosEncoderData) -> c_int {
    if devdata.is_null() {
        return 0;
    }
    match unsafe { child_u8(devdata, CHILD_DDC_PIN_OFFSET) } {
        1 => 1,
        2 => 2,
        3 => 9,
        4 => 10,
        5 => 11,
        6 => 12,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_dp_max_link_rate(devdata: *const IntelBiosEncoderData) -> c_int {
    if devdata.is_null() {
        return 0;
    }
    let version = unsafe { (*(*devdata).display).vbt.version };
    if version < 216 {
        return 0;
    }
    let rate = unsafe { child_u8(devdata, CHILD_LINK_RATE_OFFSET) & 0x07 };
    if version >= 230 {
        match rate {
            1 => 162_000,
            2 => 270_000,
            3 => 540_000,
            4 => 810_000,
            5 => 1_000_000,
            6 => 1_350_000,
            7 => 2_000_000,
            _ => 0,
        }
    } else {
        match rate {
            0 => 810_000,
            1 => 540_000,
            2 => 270_000,
            3 => 162_000,
            _ => 0,
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_dp_max_lane_count(devdata: *const IntelBiosEncoderData) -> c_int {
    if devdata.is_null() || unsafe { (*(*devdata).display).vbt.version < 244 } {
        return 0;
    }
    let encoded = unsafe { child_flags(devdata) };
    (((encoded >> 6) & 0x03) | (((encoded >> 8) & 0x03) << 2)) as c_int + 1
}

fn edp_rate_override_mask(rate: c_int) -> u32 {
    match rate {
        2_000_000 => 1 << 11,
        1_350_000 => 1 << 10,
        1_000_000 => 1 << 9,
        810_000 => 1 << 8,
        675_000 => 1 << 7,
        540_000 => 1 << 6,
        432_000 => 1 << 5,
        324_000 => 1 << 4,
        270_000 => 1 << 3,
        243_000 => 1 << 2,
        216_000 => 1 << 1,
        162_000 => 1,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_encoder_reject_edp_rate(
    devdata: *const IntelBiosEncoderData,
    rate: c_int,
) -> bool {
    if devdata.is_null() || unsafe { (*(*devdata).display).vbt.version < 263 } {
        return false;
    }
    let rates = unsafe { ptr::read_unaligned(child_bytes(devdata).add(CHILD_EDP_RATES_OFFSET).cast::<u16>()) } & 0x0fff;
    rates != 0x0fff && (rates as u32 & edp_rate_override_mask(rate)) != 0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_dp_aux_ch(devdata: *const IntelBiosEncoderData) -> AuxCh {
    if devdata.is_null() {
        return AUX_CH_NONE;
    }
    let raw = unsafe { child_u8(devdata, CHILD_AUX_CHANNEL_OFFSET) };
    let mapping = [
        (AUX_CH_A, 0x40),
        (AUX_CH_B, 0x10),
        (AUX_CH_C, 0x20),
        (AUX_CH_D_XELPD, 0x30),
        (AUX_CH_E_XELPD, 0x50),
        (AUX_CH_USBC1, 0x60),
        (AUX_CH_USBC2, 0x70),
        (AUX_CH_USBC3, 0x80),
        (AUX_CH_USBC4, 0x90),
    ];
    mapping.iter().find_map(|(ch, value)| (*value == raw).then_some(*ch)).unwrap_or(AUX_CH_NONE)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_dp_has_shared_aux_ch(devdata: *const IntelBiosEncoderData) -> bool {
    if devdata.is_null() {
        return false;
    }
    let raw = unsafe { child_u8(devdata, CHILD_AUX_CHANNEL_OFFSET) };
    if raw == 0 {
        return false;
    }
    let display = unsafe { (*devdata).display };
    let head = unsafe { ptr::addr_of!((*display).vbt.display_devices) };
    let mut node = unsafe { (*head).next };
    let mut count = 0;
    while !ptr::eq(node, head.cast_mut()) {
        let other = unsafe { encoder_from_node(node) };
        if unsafe { intel_bios_encoder_supports_dp(other) }
            && unsafe { child_u8(other, CHILD_AUX_CHANNEL_OFFSET) } == raw
        {
            count += 1;
        }
        node = unsafe { (*node).next };
    }
    count > 1
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_is_tv_present(display: *mut IntelDisplay) -> bool {
    if display.is_null() || unsafe { (*display).vbt._bitfields_2[0] & 1 == 0 } {
        return false;
    }
    let head = unsafe { ptr::addr_of!((*display).vbt.display_devices) };
    if unsafe { tk_linux_kapi::list::list_empty(&*head) } {
        return true;
    }
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head.cast_mut()) {
        let entry = unsafe { encoder_from_node(node) };
        let ty = unsafe { child_device_type(entry) };
        if matches!(ty, DEVICE_TYPE_INT_TV | DEVICE_TYPE_TV | DEVICE_TYPE_TV_SVIDEO_COMPOSITE)
            && unsafe { child_u16(entry, offset_of!(ChildDeviceConfig, addin_offset)) != 0 }
        {
            return true;
        }
        node = unsafe { (*node).next };
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_is_lvds_present(
    display: *mut IntelDisplay,
    i2c_pin: *mut u8,
) -> bool {
    if display.is_null() {
        return false;
    }
    let head = unsafe { ptr::addr_of!((*display).vbt.display_devices) };
    if unsafe { tk_linux_kapi::list::list_empty(&*head) } {
        return true;
    }
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head.cast_mut()) {
        let entry = unsafe { encoder_from_node(node) };
        let ty = unsafe { child_device_type(entry) };
        if ty == DEVICE_TYPE_INT_LFP || ty == DEVICE_TYPE_LFP {
            let pin = unsafe { child_u8(entry, offset_of!(ChildDeviceConfig, i2c_pin)) };
            if !i2c_pin.is_null() && unsafe { gmbus_pin_valid_adlp(pin) } {
                unsafe { *i2c_pin = pin };
            }
            if unsafe { child_u16(entry, offset_of!(ChildDeviceConfig, addin_offset)) != 0 } {
                return true;
            }
            return unsafe { crate::intel_opregion_upstream::intel_opregion_vbt_present(display) };
        }
        node = unsafe { (*node).next };
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_is_dsi_present(display: *mut IntelDisplay, port: *mut Port) -> bool {
    if display.is_null() {
        return false;
    }
    let head = unsafe { ptr::addr_of!((*display).vbt.display_devices) };
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head.cast_mut()) {
        let entry = unsafe { encoder_from_node(node) };
        if unsafe { child_device_type(entry) & DEVICE_TYPE_MIPI_OUTPUT != 0 } {
            let dvo = unsafe { child_u8(entry, CHILD_DVO_PORT_OFFSET) };
            let dsi_port = match dvo {
                DVO_PORT_MIPIA => PORT_A,
                DVO_PORT_MIPIC if unsafe { display_ver(display) >= 11 } => PORT_B,
                DVO_PORT_MIPIC => PORT_C,
                _ => PORT_NONE,
            };
            if dsi_port != PORT_NONE {
                if !port.is_null() {
                    unsafe { *port = dsi_port };
                }
                return true;
            }
        }
        node = unsafe { (*node).next };
    }
    false
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bios_for_each_encoder(
    display: *mut IntelDisplay,
    func: Option<unsafe extern "C" fn(*mut IntelDisplay, *const IntelBiosEncoderData)>,
) {
    if display.is_null() {
        return;
    }
    let Some(func) = func else { return };
    let head = unsafe { ptr::addr_of!((*display).vbt.display_devices) };
    let mut node = unsafe { (*head).next };
    while !ptr::eq(node, head.cast_mut()) {
        let devdata = unsafe { encoder_from_node(node) };
        unsafe { func(display, devdata) };
        node = unsafe { (*node).next };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{vec, vec::Vec};

    fn valid_vbt() -> Vec<u8> {
        let mut bytes = vec![0; 48 + 22];
        bytes[..4].copy_from_slice(b"$VBT");
        bytes[22..24].copy_from_slice(&(48u16).to_le_bytes());
        bytes[24..26].copy_from_slice(&(70u16).to_le_bytes());
        bytes[28..32].copy_from_slice(&(48u32).to_le_bytes());
        bytes[48..64].copy_from_slice(b"BIOS_DATA_BLOCK\0");
        bytes[64..66].copy_from_slice(&(220u16).to_le_bytes());
        bytes[66..68].copy_from_slice(&(22u16).to_le_bytes());
        bytes[68..70].copy_from_slice(&(22u16).to_le_bytes());
        bytes
    }

    #[test]
    fn accepts_bounded_vbt_and_bdb_envelopes() {
        let bytes = valid_vbt();
        assert_eq!(
            validate_vbt_bytes(&bytes),
            Some(VbtLayout {
                vbt_size: 70,
                bdb_offset: 48,
                bdb_header_size: 22,
                bdb_size: 22,
            })
        );
    }

    #[test]
    fn rejects_short_signature_and_overflowing_offsets() {
        let mut bytes = valid_vbt();
        assert_eq!(validate_vbt_bytes(&bytes[..47]), None);
        bytes[0] = b'X';
        assert_eq!(validate_vbt_bytes(&bytes), None);
        bytes[0] = b'$';
        bytes[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(validate_vbt_bytes(&bytes), None);
    }

    #[test]
    fn rejects_truncated_bdb_and_malformed_section_lengths() {
        let mut bytes = valid_vbt();
        bytes[66..68].copy_from_slice(&(u16::MAX).to_le_bytes());
        assert_eq!(validate_vbt_bytes(&bytes), None);

        let mut bdb = vec![0; 28];
        bdb[18..20].copy_from_slice(&(22u16).to_le_bytes());
        bdb[20..22].copy_from_slice(&(28u16).to_le_bytes());
        bdb[22] = BDB_GENERAL_FEATURES;
        bdb[23..25].copy_from_slice(&(u16::MAX).to_le_bytes());
        assert!(unsafe { find_raw_section(&bdb, BDB_GENERAL_FEATURES) }.is_none());
    }

    #[test]
    fn section_walk_preserves_header_and_handles_mipi_extended_size() {
        let mut bdb = vec![0; 22 + 3 + 4];
        bdb[18..20].copy_from_slice(&(22u16).to_le_bytes());
        let bdb_len = bdb.len() as u16;
        bdb[20..22].copy_from_slice(&bdb_len.to_le_bytes());
        bdb[22] = BDB_GENERAL_FEATURES;
        bdb[23..25].copy_from_slice(&(4u16).to_le_bytes());
        bdb[25..29].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            unsafe { find_raw_section(&bdb, BDB_GENERAL_FEATURES) },
            Some(&bdb[25..29][..])
        );
        assert_eq!(
            unsafe { find_raw_block(&bdb, BDB_GENERAL_FEATURES) },
            Some(&bdb[22..29][..])
        );

        let mut mipi = vec![0; 22 + 3 + 8];
        mipi[18..20].copy_from_slice(&(22u16).to_le_bytes());
        let mipi_len = mipi.len() as u16;
        mipi[20..22].copy_from_slice(&mipi_len.to_le_bytes());
        mipi[22] = BDB_MIPI_SEQUENCE;
        mipi[25] = 3;
        mipi[26..30].copy_from_slice(&(8u32).to_le_bytes());
        assert_eq!(unsafe { find_raw_section(&mipi, BDB_MIPI_SEQUENCE) }.unwrap().len(), 8);
    }

    #[test]
    fn display_13_dvo_map_covers_ddi_and_type_c_ports() {
        for (dvo, port) in [
            (DVO_PORT_HDMIA, PORT_A),
            (DVO_PORT_DPB, PORT_B),
            (DVO_PORT_HDMIC, PORT_C),
            (DVO_PORT_DPD, PORT_D_XELPD),
            (DVO_PORT_HDMIE, PORT_E_XELPD),
            (DVO_PORT_DPF, PORT_TC1),
            (DVO_PORT_HDMIG, PORT_TC2),
            (DVO_PORT_DPH, PORT_TC3),
            (DVO_PORT_HDMII, PORT_TC4),
        ] {
            assert_eq!(xelpd_dvo_port_to_port(dvo), port);
        }
        assert_eq!(xelpd_dvo_port_to_port(DVO_PORT_CRT), PORT_NONE);
    }
}
