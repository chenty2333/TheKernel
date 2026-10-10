// SPDX-License-Identifier: MIT
// Copyright © 2014-2019 Intel Corporation.
// Source: Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_huc_fw.c.
// The packed HECI/PXP message definitions below follow their own MIT headers;
// lower-level GSC, PXP, firmware, and GEM operations remain owner boundaries.

#![allow(dead_code, non_snake_case, unsafe_code)]

use core::{
    ffi::c_void,
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    i915_gem_object_api_upstream::i915_gem_object_unpin_map,
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_gem_pages_upstream::i915_gem_object_pin_map_unlocked,
    intel_gsc_uc_types_upstream::IntelGscUc,
    intel_gt_api_upstream::huc_to_gt,
    intel_huc_types_upstream::{
        INTEL_HUC_AUTH_BY_GSC, IntelHuc, intel_huc_is_authenticated, intel_huc_is_loaded_by_gsc,
        intel_huc_wait_for_auth_complete,
    },
    intel_uc_fw_types_upstream::{
        INTEL_UC_FIRMWARE_RUNNING, INTEL_UC_FIRMWARE_TRANSFERRED, IntelUcFw,
    },
    linux::{
        config::{EBUSY, EINVAL, EIO, ENODEV, EPROTO},
        gem::DrmGemObject,
        i915::i915_ggtt_offset,
        wait::msleep,
    },
};

const PCI_VENDOR_ID_INTEL: u16 = 0x8086;
const INTEL_GSC_CPD_HEADER_MARKER: u32 = 0x4450_4324;
const INTEL_GSC_CPD_ENTRY_OFFSET_MASK: u32 = 0x01ff_ffff;
const PXP43_CMDID_NEW_HUC_AUTH: u32 = 0x3f;
const PXP43_HUC_AUTH_INOUT_SIZE: usize = 4096;
const HECI_MEADDRESS_PXP: u8 = 17;
const GSC_OUTFLAG_MSG_PENDING: u32 = 1 << 0;
const PXP_STATUS_SUCCESS: u32 = 0;
const PXP_STATUS_OP_NOT_PERMITTED: u32 = 0x4013;
const HUC_UKERNEL: u32 = 1 << 9;
const ENODATA: i32 = 61;
const ENOEXEC: i32 = 8;

unsafe fn gem_object_size(obj: *mut DrmI915GemObject) -> u64 {
    let offset = offset_of!(DrmI915GemObject, base) + offset_of!(DrmGemObject, size);
    unsafe { ptr::read_unaligned(obj.cast::<u8>().add(offset).cast::<u64>()) }
}

// SPDX-License-Identifier: MIT; Copyright © 2023 Intel Corporation.
// Layout from gt/uc/intel_gsc_uc_heci_cmd_submit.h.
#[repr(C, packed)]
struct IntelGscMtlHeader {
    validity_marker: u32,
    heci_client_id: u8,
    reserved1: u8,
    header_version: u16,
    host_session_handle: u64,
    gsc_message_handle: u64,
    message_size: u32,
    flags: u32,
    status: u32,
}
const _: [(); 36] = [(); size_of::<IntelGscMtlHeader>()];

// SPDX-License-Identifier: MIT; Copyright © 2022 Intel Corporation.
// Layouts from pxp/intel_pxp_cmd_interface_cmn.h and _43.h.
#[repr(C, packed)]
struct PxpCmdHeader {
    api_version: u32,
    command_id: u32,
    status_or_stream_id: u32,
    buffer_len: u32,
}
#[repr(C, packed)]
struct Pxp43NewHucAuthIn {
    header: PxpCmdHeader,
    huc_base_address: u64,
    huc_size: u32,
}
#[repr(C, packed)]
struct Pxp43HucAuthOut {
    header: PxpCmdHeader,
}
#[repr(C, packed)]
struct MtlHucAuthMsgIn {
    header: IntelGscMtlHeader,
    huc_in: Pxp43NewHucAuthIn,
}
#[repr(C, packed)]
struct MtlHucAuthMsgOut {
    header: IntelGscMtlHeader,
    huc_out: Pxp43HucAuthOut,
}
const _: [(); 64] = [(); size_of::<MtlHucAuthMsgIn>()];
const _: [(); 52] = [(); size_of::<MtlHucAuthMsgOut>()];

// Layouts from gt/uc/intel_gsc_binary_headers.h (MIT, Copyright © 2023 Intel).
#[repr(C, packed)]
struct IntelGscCpdHeaderV2 {
    header_marker: u32,
    num_of_entries: u32,
    header_version: u8,
    entry_version: u8,
    header_length: u8,
    flags: u8,
    partition_name: u32,
    crc32: u32,
}
#[repr(C, packed)]
struct IntelGscCpdEntry {
    name: [u8; 12],
    offset: u32,
    length: u32,
    reserved: [u8; 4],
}
const _: [(); 20] = [(); size_of::<IntelGscCpdHeaderV2>()];
const _: [(); 24] = [(); size_of::<IntelGscCpdEntry>()];

unsafe extern "C" {
    fn intel_gsc_uc_heci_cmd_emit_mtl_header(
        header: *mut IntelGscMtlHeader,
        heci_client_id: u8,
        message_size: u32,
        host_session_id: u64,
    );
    fn intel_gsc_uc_heci_cmd_submit_packet(
        gsc: *mut IntelGscUc,
        addr_in: u64,
        size_in: u32,
        addr_out: u64,
        size_out: u32,
    ) -> i32;
    fn intel_pxp_huc_load_and_auth(pxp: *mut c_void) -> i32;
}

macro_rules! huc_err {
    ($huc:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __huc = $huc;
        let __gt = unsafe { huc_to_gt(__huc) };
        gt_err!(__gt, $format $(, $argument)*);
    }};
}

#[inline]
fn cpd_entry_name_is(name: &[u8; 12], expected: &[u8]) -> bool {
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    &name[..end] == expected
}

// upstream: intel_huc_fw.c intel_huc_fw_auth_via_gsccs()
pub unsafe fn intel_huc_fw_auth_via_gsccs(huc: *mut IntelHuc) -> i32 {
    let gt = unsafe { huc_to_gt(huc) };
    let vma = unsafe { (*huc).heci_pkt };
    if vma.is_null() {
        return -ENODEV;
    }

    let obj = unsafe { (*vma).obj };
    let packet_offset = unsafe { i915_ggtt_offset(vma) } as u64;
    let map_type =
        unsafe { crate::intel_gt_api_upstream::intel_gt_coherent_map_type(gt, obj, true) };
    let packet_vaddr = unsafe { i915_gem_object_pin_map_unlocked(obj, map_type) };
    if crate::linux_config::IS_ERR(packet_vaddr) {
        return crate::linux_config::PTR_ERR(packet_vaddr);
    }

    let msg_in = packet_vaddr.cast::<MtlHucAuthMsgIn>();
    let msg_out = packet_vaddr
        .cast::<u8>()
        .wrapping_add(PXP43_HUC_AUTH_INOUT_SIZE)
        .cast::<MtlHucAuthMsgOut>();
    let mut retry = 5;
    let mut err;

    unsafe {
        intel_gsc_uc_heci_cmd_emit_mtl_header(
            ptr::addr_of_mut!((*msg_in).header),
            HECI_MEADDRESS_PXP,
            size_of::<MtlHucAuthMsgIn>() as u32,
            0,
        );
        ptr::addr_of_mut!((*msg_in).huc_in.header.api_version).write_unaligned((4u32 << 16) | 3);
        ptr::addr_of_mut!((*msg_in).huc_in.header.command_id)
            .write_unaligned(PXP43_CMDID_NEW_HUC_AUTH);
        ptr::addr_of_mut!((*msg_in).huc_in.header.status_or_stream_id).write_unaligned(0);
        ptr::addr_of_mut!((*msg_in).huc_in.header.buffer_len)
            .write_unaligned((size_of::<Pxp43NewHucAuthIn>() - size_of::<PxpCmdHeader>()) as u32);
        ptr::addr_of_mut!((*msg_in).huc_in.huc_base_address)
            .write_unaligned((*huc).fw.vma_res.start);
        ptr::addr_of_mut!((*msg_in).huc_in.huc_size)
            .write_unaligned(gem_object_size((*huc).fw.obj) as u32);
    }

    loop {
        err = unsafe {
            intel_gsc_uc_heci_cmd_submit_packet(
                ptr::addr_of_mut!((*gt).uc.gsc),
                packet_offset,
                size_of::<MtlHucAuthMsgIn>() as u32,
                packet_offset + PXP43_HUC_AUTH_INOUT_SIZE as u64,
                PXP43_HUC_AUTH_INOUT_SIZE as u32,
            )
        };
        if err != 0 {
            huc_err!(huc, "failed to submit GSC request to auth: %d\\n", err);
            break;
        }

        let flags = unsafe { ptr::addr_of!((*msg_out).header.flags).read_unaligned() };
        if flags & GSC_OUTFLAG_MSG_PENDING != 0 {
            let handle =
                unsafe { ptr::addr_of!((*msg_out).header.gsc_message_handle).read_unaligned() };
            unsafe {
                ptr::addr_of_mut!((*msg_in).header.gsc_message_handle).write_unaligned(handle);
            }
            err = -EBUSY;
            msleep(50);
        }
        retry -= 1;
        if retry == 0 || err != -EBUSY {
            break;
        }
    }

    if err == 0 {
        let reply_size = unsafe { ptr::addr_of!((*msg_out).header.message_size).read_unaligned() };
        if reply_size as usize != size_of::<MtlHucAuthMsgOut>() {
            huc_err!(
                huc,
                "invalid GSC reply length %u [expected %zu]\\n",
                reply_size,
                size_of::<MtlHucAuthMsgOut>()
            );
            err = -EPROTO;
        }
    }
    if err == 0 {
        let status = unsafe {
            ptr::addr_of!((*msg_out).huc_out.header.status_or_stream_id).read_unaligned()
        };
        if status != PXP_STATUS_SUCCESS && status != PXP_STATUS_OP_NOT_PERMITTED {
            huc_err!(huc, "auth failed with GSC error = 0x%x\\n", status);
            err = -EIO;
        }
    }

    unsafe { i915_gem_object_unpin_map(obj) };
    err
}

// upstream: intel_huc_fw.c css_valid()
fn css_valid(data: *const c_void, size: usize) -> bool {
    if size < 128 {
        return false;
    }
    let module_type = unsafe { data.cast::<u8>().cast::<u32>().read_unaligned() };
    let module_vendor = unsafe { data.cast::<u8>().add(16).cast::<u32>().read_unaligned() };
    module_type == 0x6 && module_vendor == PCI_VENDOR_ID_INTEL as u32
}

// upstream: intel_huc_fw.c entry_offset()
#[inline]
fn entry_offset(entry: *const IntelGscCpdEntry) -> u32 {
    unsafe { ptr::addr_of!((*entry).offset).read_unaligned() & INTEL_GSC_CPD_ENTRY_OFFSET_MASK }
}

// upstream: intel_huc_fw.c intel_huc_fw_get_binary_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_huc_fw_get_binary_info(
    huc_fw: *mut IntelUcFw,
    data: *const c_void,
    size: usize,
) -> i32 {
    let huc = huc_fw.cast::<IntelHuc>();
    let header = data.cast::<IntelGscCpdHeaderV2>();
    let mut min_size = size_of::<IntelGscCpdHeaderV2>();
    if !unsafe { (*huc_fw).has_gsc_headers } {
        huc_err!(huc, "Invalid FW type for GSC header parsing!\\n");
        return -EINVAL;
    }
    if size < min_size {
        huc_err!(huc, "FW too small! %zu < %zu\\n", size, min_size);
        return -ENODATA;
    }

    let marker = unsafe { ptr::addr_of!((*header).header_marker).read_unaligned() };
    if marker != INTEL_GSC_CPD_HEADER_MARKER {
        huc_err!(huc, "invalid marker for CPD header: 0x%08x!\\n", marker);
        return -EINVAL;
    }
    let header_version = unsafe { ptr::addr_of!((*header).header_version).read_unaligned() };
    let entry_version = unsafe { ptr::addr_of!((*header).entry_version).read_unaligned() };
    if header_version != 2 || entry_version != 1 {
        huc_err!(
            huc,
            "invalid CPD header/entry version %u:%u!\\n",
            header_version,
            entry_version
        );
        return -EINVAL;
    }
    let header_length = unsafe { ptr::addr_of!((*header).header_length).read_unaligned() } as usize;
    if header_length < size_of::<IntelGscCpdHeaderV2>() {
        huc_err!(huc, "invalid CPD header length %u!\\n", header_length);
        return -EINVAL;
    }
    let entries = unsafe { ptr::addr_of!((*header).num_of_entries).read_unaligned() } as usize;
    min_size = header_length + size_of::<IntelGscCpdEntry>() * entries;
    if size < min_size {
        huc_err!(huc, "FW too small! %zu < %zu\\n", size, min_size);
        return -ENODATA;
    }

    let mut entry = unsafe {
        data.cast::<u8>()
            .add(header_length)
            .cast::<IntelGscCpdEntry>()
    };
    for _ in 0..entries {
        let name = unsafe { &(*entry).name };
        if cpd_entry_name_is(name, b"HUCP.man") {
            let offset = entry_offset(entry) as usize;
            unsafe {
                crate::intel_uc_fw_upstream::intel_uc_fw_version_from_gsc_manifest(
                    ptr::addr_of_mut!((*huc_fw).file_selected.ver),
                    data.cast::<u8>().add(offset).cast(),
                );
            }
        }
        if cpd_entry_name_is(name, b"huc_fw") {
            let offset = entry_offset(entry) as usize;
            if offset < size
                && css_valid(
                    unsafe { data.cast::<u8>().add(offset).cast() },
                    size - offset,
                )
            {
                unsafe {
                    (*huc_fw).dma_start_offset = offset as u32;
                }
            }
        }
        entry = unsafe { entry.add(1) };
    }
    0
}

// upstream: intel_huc_fw.c intel_huc_fw_load_and_auth_via_gsc()
pub unsafe fn intel_huc_fw_load_and_auth_via_gsc(huc: *mut IntelHuc) -> i32 {
    if !unsafe { intel_huc_is_loaded_by_gsc(huc) } {
        return -ENODEV;
    }
    let fw = unsafe { ptr::addr_of_mut!((*huc).fw) };
    if !unsafe { crate::intel_uc_fw_upstream::intel_uc_fw_is_loadable(fw) } {
        return -ENOEXEC;
    }
    if unsafe { intel_huc_is_authenticated(huc, INTEL_HUC_AUTH_BY_GSC) } {
        unsafe {
            crate::intel_uc_fw_upstream::intel_uc_fw_change_status(fw, INTEL_UC_FIRMWARE_RUNNING);
        }
        return 0;
    }
    GEM_WARN_ON!(unsafe { crate::intel_uc_fw_upstream::intel_uc_fw_is_loaded(fw) });
    let gt = unsafe { huc_to_gt(huc) };
    let err = unsafe { intel_pxp_huc_load_and_auth((*(*gt).i915).pxp) };
    if err != 0 {
        return err;
    }
    unsafe {
        crate::intel_uc_fw_upstream::intel_uc_fw_change_status(fw, INTEL_UC_FIRMWARE_TRANSFERRED);
        intel_huc_wait_for_auth_complete(huc, INTEL_HUC_AUTH_BY_GSC)
    }
}

// upstream: intel_huc_fw.c intel_huc_fw_upload()
pub unsafe fn intel_huc_fw_upload(huc: *mut IntelHuc) -> i32 {
    if unsafe { intel_huc_is_loaded_by_gsc(huc) } {
        return -ENODEV;
    }
    unsafe {
        crate::intel_uc_fw_upstream::intel_uc_fw_upload(
            ptr::addr_of_mut!((*huc).fw),
            0,
            HUC_UKERNEL,
        )
    }
}
